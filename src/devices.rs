//! Cámara y micrófono de cada móvil como dispositivos del sistema, visibles para Meet, Discord, OBS…
//!  · Webcam: una por móvil. La del 1º es /dev/video42 "Camara del movil" (v4l2loopback, siempre
//!    escrita: imagen de espera a 2 fps sin cámara, así las apps no la pierden). La de cada móvil más
//!    se crea al conectarlo ("Cámara de <móvil>", por /dev/v4l2loopback) y se quita al desconectarlo.
//!    Imágenes: H.264 del móvil → OpenH264 → giro → 1280x720 YUV420, escritas directamente (sin ffmpeg).
//!  · Micrófono: "Micrófono del móvil" siempre; "Micrófono de <móvil>" mientras está conectado (audio).
//!  · Automático: si una app abre la webcam o graba del micrófono de un móvil, se encienden en ese
//!    móvil (/set?cam=1&auto=1&s=N en el servidor local); si nadie los usa 10 s, se apagan.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const VIDEO: &str = "/dev/video42";
const CTL: &str = "/dev/v4l2loopback";
const W: usize = 1280;
const H: usize = 720;
const FRAME: usize = W * H * 3 / 2;
const CHUNK: usize = 60_000; // la app parte cada imagen en trozos de este tamaño
pub const PREVIEW_W: usize = 640;
pub const PREVIEW_H: usize = 360;
/// Principio del nombre de las webcams creadas al vuelo (para reconocer las que sobren)
const DYN_PREFIX: &str = "Cámara de ";

/// Webcam de un móvil (ranura)
struct Cam {
    slot: u8,
    path: String,
    label: String,
    /// número del dispositivo creado al vuelo (se quita al terminar); None = /dev/video42
    dyn_nr: Option<u32>,
    rot: AtomicI32,
    /// cambia al apagar la cámara: el decodificador empieza de cero
    gen: AtomicU64,
    feed: SyncSender<(u64, Vec<u8>)>,
    preview: Mutex<Option<(Instant, Vec<u8>)>>,
    fps: Mutex<(u32, Option<Instant>, f32)>,
    stop: AtomicBool,
}

static CAMS: Mutex<Vec<Arc<Cam>>> = Mutex::new(Vec::new());

fn cam(slot: u8) -> Option<Arc<Cam>> {
    CAMS.lock().unwrap().iter().find(|c| c.slot == slot).cloned()
}

/// Última imagen de la cámara del móvil `slot` en pequeño (RGB) si es de hace menos de 1 s
pub fn preview(slot: u8) -> Option<Vec<u8>> {
    let c = cam(slot)?;
    let p = c.preview.lock().unwrap();
    p.as_ref().filter(|(t, _)| t.elapsed() < Duration::from_secs(1)).map(|(_, v)| v.clone())
}

/// Reduce una imagen de la webcam (I420 W x H) a RGB PREVIEW_W x PREVIEW_H; como mucho ~25 veces/s
fn make_preview(c: &Cam, f: &[u8]) {
    {
        let p = c.preview.lock().unwrap();
        if p.as_ref().map(|(t, _)| t.elapsed() < Duration::from_millis(40)).unwrap_or(false) {
            return;
        }
    }
    if f.len() < FRAME {
        return;
    }
    let (sx, sy) = (W / PREVIEW_W, H / PREVIEW_H);
    let (yp, up, vp) = (&f[..W * H], &f[W * H..W * H + W * H / 4], &f[W * H + W * H / 4..]);
    let mut out = Vec::with_capacity(PREVIEW_W * PREVIEW_H * 3);
    for py in 0..PREVIEW_H {
        let y = py * sy;
        for px in 0..PREVIEW_W {
            let x = px * sx;
            let yy = yp[y * W + x] as f32 - 16.0;
            let (u, v) = (up[(y / 2) * (W / 2) + x / 2] as f32 - 128.0, vp[(y / 2) * (W / 2) + x / 2] as f32 - 128.0);
            let c = |v: f32| v.clamp(0.0, 255.0) as u8;
            out.push(c(1.164 * yy + 1.596 * v));
            out.push(c(1.164 * yy - 0.392 * u - 0.813 * v));
            out.push(c(1.164 * yy + 2.017 * u));
        }
    }
    *c.preview.lock().unwrap() = Some((Instant::now(), out));
}

pub fn set_rot(slot: u8, deg: i32) {
    if let Some(c) = cam(slot) {
        if matches!(deg, 0 | 90 | 180 | 270) {
            c.rot.store(deg, Ordering::Relaxed);
        }
    }
}

/// Imágenes por segundo que llegan del móvil `slot` (para /state)
pub fn fps(slot: u8) -> f32 {
    cam(slot).map(|c| c.fps.lock().unwrap().2).unwrap_or(0.0)
}

/// Dispositivo y nombre de la webcam del móvil `slot` (lo que se ve en Meet)
pub fn webcam(slot: u8) -> Option<(String, String)> {
    cam(slot).map(|c| (c.path.clone(), c.label.clone()))
}

pub fn cam_off(slot: u8) {
    if let Some(c) = cam(slot) {
        c.gen.fetch_add(1, Ordering::Relaxed);
        *c.fps.lock().unwrap() = (0, None, 0.0);
    }
}

/// Un trozo del canal 4 (H.264 Annex B) del móvil `slot`
pub fn feed(slot: u8, data: &[u8]) {
    if let Some(c) = cam(slot) {
        let _ = c.feed.try_send((c.gen.load(Ordering::Relaxed), data.to_vec())); // lleno: se pierde (y se resincroniza)
    }
}

/// Al arrancar el servicio: micrófono y webcam del 1º móvil (siempre) y el vigilante automático
pub fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::audio::ensure_mic_source();
    remove_leftovers();
    spawn_cam(1, VIDEO.into(), "Camara del movil".into(), None);
    std::thread::Builder::new().name("auto".into()).spawn(auto).ok();
}

/// Webcam para un móvil más (ranura ≥ 2): se crea con su nombre. Err = por qué no hay cámara
pub fn open_slot(slot: u8, device: &str) -> Result<(), String> {
    if slot <= 1 || cam(slot).is_some() {
        return Ok(());
    }
    let mut label = format!("{DYN_PREFIX}{device}");
    while label.len() > 31 {
        label.pop(); // card_label: 32 bytes con el cero final (sin partir letras: pop quita caracteres enteros)
    }
    let nr = ctl_add(&label)?;
    crate::host::log(&format!("webcam: creada /dev/video{nr} «{label}» (móvil {slot})"));
    spawn_cam(slot, format!("/dev/video{nr}"), label, Some(nr));
    Ok(())
}

/// El móvil `slot` (≥ 2) se ha ido: fuera su webcam (la quita su hilo al cerrarla)
pub fn close_slot(slot: u8) {
    if slot <= 1 {
        cam_off(1);
        return;
    }
    let mut cams = CAMS.lock().unwrap();
    if let Some(i) = cams.iter().position(|c| c.slot == slot) {
        cams[i].stop.store(true, Ordering::Relaxed);
        cams.remove(i);
    }
}

fn spawn_cam(slot: u8, path: String, label: String, dyn_nr: Option<u32>) {
    let (ftx, frx) = sync_channel::<Vec<u8>>(1);
    let (dtx, drx) = sync_channel::<(u64, Vec<u8>)>(256);
    let c = Arc::new(Cam {
        slot, path, label, dyn_nr,
        rot: AtomicI32::new(0), gen: AtomicU64::new(0), feed: dtx,
        preview: Mutex::new(None), fps: Mutex::new((0, None, 0.0)), stop: AtomicBool::new(false),
    });
    CAMS.lock().unwrap().push(c.clone());
    let c2 = c.clone();
    std::thread::Builder::new().name(format!("webcam-{slot}")).spawn(move || writer(c2, frx)).ok();
    std::thread::Builder::new().name(format!("camara-{slot}")).spawn(move || decoder(c, drx, ftx)).ok();
}

// ───────────── Webcams al vuelo (/dev/v4l2loopback) ─────────────

const CTL_ADD: u64 = 0x4048_7E01; // _IOW('~', 1, struct v4l2_loopback_config) (72 bytes)
const CTL_REMOVE: u64 = 0x4004_7E02; // _IOW('~', 2, __u32)

fn ctl_open() -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new().read(true).write(true).open(CTL).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            "La cámara de un 2º móvil necesita preparar el sistema una vez: sudo pantalla-host preparar-sistema".to_string()
        } else {
            format!("Sin webcams al vuelo ({CTL}: {e}); ¿v4l2loopback 0.12 o más nuevo?")
        }
    })
}

fn ctl_add(label: &str) -> Result<u32, String> {
    use std::os::fd::AsRawFd;
    let f = ctl_open()?;
    // struct v4l2_loopback_config: output_nr, unused, card_label[32], min/max ancho/alto,
    // max_buffers, max_openers, debug, announce_all_caps (0 = "exclusive_caps": Chrome la ve como cámara)
    let mut conf = [0u8; 72];
    conf[0..4].copy_from_slice(&(-1i32).to_ne_bytes());
    conf[4..8].copy_from_slice(&(-1i32).to_ne_bytes());
    conf[8..8 + label.len()].copy_from_slice(label.as_bytes());
    let r = unsafe { libc::ioctl(f.as_raw_fd(), CTL_ADD as _, conf.as_mut_ptr()) };
    if r < 0 {
        return Err(format!("no se pudo crear la webcam: {}", std::io::Error::last_os_error()));
    }
    Ok(r as u32)
}

fn ctl_remove(nr: u32) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = ctl_open() else { return false };
    unsafe { libc::ioctl(f.as_raw_fd(), CTL_REMOVE as _, nr as libc::c_ulong) >= 0 }
}

/// Webcams "Cámara de …" que quedaran de antes (cierre brusco)
fn remove_leftovers() {
    for e in std::fs::read_dir("/sys/devices/virtual/video4linux").into_iter().flatten().flatten() {
        let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default();
        let nr = e.file_name().to_string_lossy().strip_prefix("video").and_then(|n| n.parse::<u32>().ok());
        if let (true, Some(nr)) = (name.trim().starts_with(DYN_PREFIX), nr) {
            if ctl_remove(nr) {
                crate::host::log(&format!("webcam: quitada /dev/video{nr} (de una sesión anterior)"));
            }
        }
    }
}

// ───────────── Decodificar ─────────────

fn has_idr(b: &[u8]) -> bool {
    openh264::nal_units(b).any(|n| {
        let i = n.iter().position(|x| *x != 0).map(|p| p + 1).unwrap_or(0); // tras 00 00 (00) 01
        n.get(i).map(|h| matches!(h & 0x1f, 5 | 7)).unwrap_or(false)
    })
}

fn decoder(c: Arc<Cam>, rx: Receiver<(u64, Vec<u8>)>, frames: SyncSender<Vec<u8>>) {
    let mut dec: Option<openh264::decoder::Decoder> = None;
    let (mut gen, mut buf, mut synced) = (u64::MAX, Vec::<u8>::new(), false);
    while !c.stop.load(Ordering::Relaxed) {
        let (g, chunk) = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(x) => x,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return,
        };
        if g != gen || dec.is_none() {
            gen = g;
            dec = openh264::decoder::Decoder::new().ok();
            buf.clear();
            synced = false;
        }
        buf.extend_from_slice(&chunk);
        // trozo más corto que CHUNK = fin de la imagen (la app parte cada imagen en trozos de 60000)
        if chunk.len() >= CHUNK && buf.len() < 8 << 20 {
            continue;
        }
        let frame = std::mem::take(&mut buf);
        if !synced {
            if !has_idr(&frame) {
                continue; // tras empezar o perder trozos: esperar a una imagen completa
            }
            synced = true;
        }
        let Some(d) = dec.as_mut() else { continue };
        let mut out = None;
        for nal in openh264::nal_units(&frame) {
            match d.decode(nal) {
                Ok(Some(img)) => {
                    use openh264::formats::YUVSource;
                    let (w, h) = img.dimensions();
                    let (sy, su, sv) = img.strides();
                    out = Some(to_webcam(img.y(), img.u(), img.v(), (sy, su, sv), w, h, c.rot.load(Ordering::Relaxed)));
                }
                Ok(None) => {}
                Err(_) => synced = false,
            }
        }
        if let Some(f) = out {
            let mut fps = c.fps.lock().unwrap();
            fps.0 += 1;
            let t0 = *fps.1.get_or_insert_with(Instant::now);
            if t0.elapsed() >= Duration::from_secs(2) {
                fps.2 = fps.0 as f32 / t0.elapsed().as_secs_f32();
                *fps = (0, Some(Instant::now()), fps.2);
            }
            drop(fps);
            if let Err(TrySendError::Full(_)) = frames.try_send(f) {} // la webcam va atrasada: se salta
        }
    }
}

/// Plano w×h (con stride) → girado `rot` grados horarios → encajado en PW×PH (bandas de `fill`)
#[allow(clippy::too_many_arguments)]
fn plane(src: &[u8], stride: usize, w: usize, h: usize, rot: i32, pw: usize, ph: usize, fill: u8, out: &mut Vec<u8>) {
    let (rw, rh) = if rot == 90 || rot == 270 { (h, w) } else { (w, h) };
    // escala para encajar (sin deformar), vecino más próximo
    let sc = (pw as f32 / rw as f32).min(ph as f32 / rh as f32);
    let (nw, nh) = (((rw as f32 * sc) as usize).max(1).min(pw), ((rh as f32 * sc) as usize).max(1).min(ph));
    let (x0, y0) = ((pw - nw) / 2, (ph - nh) / 2);
    let start = out.len();
    out.resize(start + pw * ph, fill);
    let o = &mut out[start..];
    let xs: Vec<usize> = (0..nw).map(|x| x * rw / nw).collect();
    for y in 0..nh {
        let ry = y * rh / nh;
        let row = &mut o[(y0 + y) * pw + x0..(y0 + y) * pw + x0 + nw];
        for (x, px) in row.iter_mut().enumerate() {
            let rx = xs[x];
            // (rx, ry) en la imagen girada → (sx, sy) en la original
            let (sx, sy) = match rot {
                90 => (ry, h - 1 - rx),
                180 => (w - 1 - rx, h - 1 - ry),
                270 => (w - 1 - ry, rx),
                _ => (rx, ry),
            };
            *px = src[sy * stride + sx];
        }
    }
}

fn to_webcam(y: &[u8], u: &[u8], v: &[u8], s: (usize, usize, usize), w: usize, h: usize, rot: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME);
    if rot == 0 && (w, h) == (W, H) {
        // lo normal: solo quitar el relleno de las filas
        for (p, st, pw, ph) in [(y, s.0, W, H), (u, s.1, W / 2, H / 2), (v, s.2, W / 2, H / 2)] {
            for r in 0..ph {
                out.extend_from_slice(&p[r * st..r * st + pw]);
            }
        }
        return out;
    }
    plane(y, s.0, w, h, rot, W, H, 16, &mut out);
    plane(u, s.1, w / 2, h / 2, rot, W / 2, H / 2, 128, &mut out);
    plane(v, s.2, w / 2, h / 2, rot, W / 2, H / 2, 128, &mut out);
    out
}

// ───────────── Escribir en la webcam ─────────────

fn placeholder() -> Vec<u8> {
    let mut y = vec![40u8; W * H];
    let (x0, x1, y0, y1) = (W / 2 - 150, W / 2 + 150, H / 2 - 90, H / 2 + 90);
    for r in y0..y1 {
        for c in x0..x1 {
            let inner = r >= y0 + 10 && r < y1 - 10 && c >= x0 + 14 && c < x1 - 14;
            let dot = r.abs_diff(H / 2) < 12 && c.abs_diff(W / 2) < 12;
            y[r * W + c] = if inner && !dot { 55 } else { 150 };
        }
    }
    y.extend(std::iter::repeat_n(133u8, W * H / 4));
    y.extend(std::iter::repeat_n(127u8, W * H / 4));
    y
}

/// Abre la webcam como salida YUV420 1280x720 (VIDIOC_S_FMT)
fn open_dev(path: &str) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
    let mut fmt = [0u8; 208]; // struct v4l2_format
    let put = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    put(&mut fmt, 0, 2); // V4L2_BUF_TYPE_VIDEO_OUTPUT
    for (i, v) in [W as u32, H as u32, 0x3231_5559 /* YU12 */, 1 /* FIELD_NONE */, W as u32, FRAME as u32, 8 /* SRGB */]
        .into_iter()
        .enumerate()
    {
        put(&mut fmt, 8 + i * 4, v);
    }
    use std::os::fd::AsRawFd;
    let r = unsafe { libc::ioctl(f.as_raw_fd(), 0xC0D0_5605u64 as _, fmt.as_mut_ptr()) }; // VIDIOC_S_FMT
    if r < 0 {
        crate::host::log(&format!("webcam {path}: VIDIOC_S_FMT: {}", std::io::Error::last_os_error()));
        return None;
    }
    Some(f)
}

fn writer(c: Arc<Cam>, rx: Receiver<Vec<u8>>) {
    use std::io::Write;
    let idle = placeholder();
    let mut dev: Option<std::fs::File> = None;
    let mut last_real = Instant::now() - Duration::from_secs(10);
    let mut tries = 0u32;
    while !c.stop.load(Ordering::Relaxed) {
        let frame = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => {
                last_real = Instant::now();
                make_preview(&c, &f);
                f
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if last_real.elapsed() > Duration::from_secs(1) => idle.clone(),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        if dev.is_none() {
            if !std::path::Path::new(&c.path).exists() {
                if c.dyn_nr.is_some() {
                    std::thread::sleep(Duration::from_millis(200)); // recién creada: udev aún no ha hecho el nodo
                } else {
                    std::thread::sleep(Duration::from_secs(10)); // sin v4l2loopback: nada que hacer (se mira de vez en cuando)
                }
                continue;
            }
            dev = open_dev(&c.path);
            if dev.is_none() {
                tries += 1;
                if tries == 20 {
                    crate::host::log(&format!("webcam: no se puede abrir {} (¿permisos?)", c.path));
                }
                continue;
            }
        }
        if let Some(d) = dev.as_mut() {
            if d.write_all(&frame).is_err() {
                dev = None;
            }
        }
    }
    // móvil desconectado: se cierra y se quita su webcam (si una app la tiene abierta, se reintenta)
    drop(dev);
    if let Some(nr) = c.dyn_nr {
        for _ in 0..120 {
            if ctl_remove(nr) {
                crate::host::log(&format!("webcam: quitada /dev/video{nr} «{}»", c.label));
                return;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

// ───────────── Automático ─────────────

/// Webcams (/dev/video*) que tiene abiertas algún otro programa
fn cam_readers() -> HashSet<String> {
    let me = std::process::id().to_string();
    let mut out = HashSet::new();
    for p in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = p.file_name();
        let pid = pid.to_string_lossy();
        if !pid.bytes().all(|b| b.is_ascii_digit()) || pid == me {
            continue;
        }
        for fd in std::fs::read_dir(p.path().join("fd")).into_iter().flatten().flatten() {
            if let Ok(l) = std::fs::read_link(fd.path()) {
                let l = l.to_string_lossy();
                if l.starts_with("/dev/video") {
                    out.insert(l.into_owned());
                }
            }
        }
    }
    out
}

fn get(path: &str) -> Option<serde_json::Value> {
    use std::io::{Read, Write};
    let port = crate::util::env_port("PANTALLA_MEDIA_PORT", 6083);
    let mut s = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\n\r\n").ok()?;
    let mut r = String::new();
    s.read_to_string(&mut r).ok()?;
    serde_json::from_str(r.split_once("\r\n\r\n")?.1).ok()
}

#[derive(Default)]
struct Auto {
    session: u64,
    on: [bool; 2],
    idle_since: [Option<Instant>; 2],
}

fn auto() {
    let mut st: HashMap<u8, Auto> = HashMap::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let sessions = crate::host::session_marks();
        st.retain(|s, _| sessions.iter().any(|x| x.0 == *s));
        if sessions.is_empty() {
            continue;
        }
        let readers = cam_readers();
        for (slot, mark) in sessions {
            let a = st.entry(slot).or_default();
            if a.session != mark {
                // otra sesión en esta ranura: se olvida lo encendido en la anterior
                *a = Auto { session: mark, ..Default::default() };
                if slot == 1 {
                    crate::audio::ensure_mic_source();
                }
            }
            let cam_used = webcam(slot).map(|(p, _)| readers.contains(&p)).unwrap_or(false);
            let used = [cam_used, crate::audio::mic_in_use(slot)];
            for (i, key) in ["cam", "mic"].into_iter().enumerate() {
                if used[i] {
                    a.idle_since[i] = None;
                    if !a.on[i] {
                        if let Some(state) = get(&format!("/state?s={slot}")) {
                            if !state[key].as_bool().unwrap_or(false) && get(&format!("/set?{key}=1&auto=1&s={slot}")).is_some() {
                                a.on[i] = true;
                                crate::host::log(&format!("una app usa {key} del móvil {slot}: encendido"));
                            }
                        }
                    }
                } else if a.on[i] {
                    let t = *a.idle_since[i].get_or_insert_with(Instant::now);
                    if t.elapsed() > Duration::from_secs(10) {
                        let _ = get(&format!("/set?{key}=0&s={slot}"));
                        a.on[i] = false;
                        a.idle_since[i] = None;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn giro_y_encaje() {
        // 4x2 con un punto arriba a la izquierda; girado 90° horario → arriba a la derecha
        let src = [200u8, 0, 0, 0, 0, 0, 0, 0];
        let mut out = vec![];
        super::plane(&src, 4, 4, 2, 90, 2, 4, 9, &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(out[1], 200); // fila 0, columna derecha
        let mut o2 = vec![];
        super::plane(&src, 4, 4, 2, 0, 8, 2, 9, &mut o2); // 4x2 en 8x2: bandas a los lados
        assert_eq!(&o2[0..2], &[9, 9]);
        assert_eq!(o2[2], 200);
    }
}
