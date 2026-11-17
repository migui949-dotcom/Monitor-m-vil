//! Cámara y micrófono del móvil como dispositivos del sistema, siempre visibles para Meet,
//! Discord, OBS… (lo mismo que media.py en la versión en Python):
//!  · Webcam: /dev/video42 (v4l2loopback). Este servicio es el ÚNICO que escribe, siempre: imagen
//!    de espera a 2 fps sin cámara, o las imágenes del móvil (H.264 → OpenH264 → giro → 1280x720),
//!    escritas directamente (sin ffmpeg). Así las apps no pierden la cámara.
//!  · Micrófono: la fuente "Micrófono del móvil" existe siempre (audio::ensure_mic_source).
//!  · Automático: si una app abre la webcam o graba del micrófono, se encienden en el móvil
//!    (/set?cam=1&auto=1 en el servidor local); si nadie los usa 10 s, se apagan.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const VIDEO: &str = "/dev/video42";
const W: usize = 1280;
const H: usize = 720;
const FRAME: usize = W * H * 3 / 2;
const CHUNK: usize = 60_000; // la app parte cada imagen en trozos de este tamaño

static ROT: AtomicI32 = AtomicI32::new(0);
static GEN: AtomicU64 = AtomicU64::new(0); // cambia al apagar la cámara: el decodificador empieza de cero
static FEED: OnceLock<SyncSender<(u64, Vec<u8>)>> = OnceLock::new();
static FRAMES: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();
static FPS: Mutex<(u32, Option<Instant>, f32)> = Mutex::new((0, None, 0.0));

pub fn set_rot(deg: i32) {
    if matches!(deg, 0 | 90 | 180 | 270) {
        ROT.store(deg, Ordering::Relaxed);
    }
}

/// Imágenes por segundo que llegan del móvil (para /state)
pub fn fps() -> f32 {
    FPS.lock().unwrap().2
}

pub fn cam_off() {
    GEN.fetch_add(1, Ordering::Relaxed);
    *FPS.lock().unwrap() = (0, None, 0.0);
}

/// Un trozo del canal 4 (H.264 Annex B)
pub fn feed(data: &[u8]) {
    if let Some(tx) = FEED.get() {
        let _ = tx.try_send((GEN.load(Ordering::Relaxed), data.to_vec())); // lleno: se pierde (y se resincroniza)
    }
}

pub fn start() {
    crate::audio::ensure_mic_source();
    let (ftx, frx) = sync_channel::<Vec<u8>>(1);
    let (dtx, drx) = sync_channel::<(u64, Vec<u8>)>(256);
    if FRAMES.set(ftx).is_err() || FEED.set(dtx).is_err() {
        return;
    }
    std::thread::Builder::new().name("webcam".into()).spawn(move || writer(frx)).ok();
    std::thread::Builder::new().name("camara".into()).spawn(move || decoder(drx)).ok();
    std::thread::Builder::new().name("auto".into()).spawn(auto).ok();
}

// ───────────── Decodificar ─────────────

fn has_idr(b: &[u8]) -> bool {
    openh264::nal_units(b).any(|n| {
        let i = n.iter().position(|x| *x != 0).map(|p| p + 1).unwrap_or(0); // tras 00 00 (00) 01
        n.get(i).map(|h| matches!(h & 0x1f, 5 | 7)).unwrap_or(false)
    })
}

fn decoder(rx: Receiver<(u64, Vec<u8>)>) {
    let mut dec: Option<openh264::decoder::Decoder> = None;
    let (mut gen, mut buf, mut synced) = (u64::MAX, Vec::<u8>::new(), false);
    while let Ok((g, chunk)) = rx.recv() {
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
                    out = Some(to_webcam(img.y(), img.u(), img.v(), (sy, su, sv), w, h, ROT.load(Ordering::Relaxed)));
                }
                Ok(None) => {}
                Err(_) => synced = false,
            }
        }
        if let Some(f) = out {
            let mut fps = FPS.lock().unwrap();
            fps.0 += 1;
            let t0 = *fps.1.get_or_insert_with(Instant::now);
            if t0.elapsed() >= Duration::from_secs(2) {
                fps.2 = fps.0 as f32 / t0.elapsed().as_secs_f32();
                *fps = (0, Some(Instant::now()), fps.2);
            }
            drop(fps);
            if let Some(tx) = FRAMES.get() {
                if let Err(TrySendError::Full(_)) = tx.try_send(f) {} // la webcam va atrasada: se salta
            }
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

/// Abre /dev/video42 como salida YUV420 1280x720 (VIDIOC_S_FMT)
fn open_dev() -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(VIDEO).ok()?;
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
        crate::host::log(&format!("webcam: VIDIOC_S_FMT: {}", std::io::Error::last_os_error()));
        return None;
    }
    Some(f)
}

fn writer(rx: Receiver<Vec<u8>>) {
    use std::io::Write;
    let idle = placeholder();
    let mut dev: Option<std::fs::File> = None;
    let mut last_real = Instant::now() - Duration::from_secs(10);
    loop {
        let frame = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => {
                last_real = Instant::now();
                f
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if last_real.elapsed() > Duration::from_secs(1) => idle.clone(),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return,
        };
        if dev.is_none() {
            if !std::path::Path::new(VIDEO).exists() {
                std::thread::sleep(Duration::from_secs(10)); // sin v4l2loopback: nada que hacer (se mira de vez en cuando)
                continue;
            }
            dev = open_dev();
        }
        if let Some(d) = dev.as_mut() {
            if d.write_all(&frame).is_err() {
                dev = None;
            }
        }
    }
}

// ───────────── Automático ─────────────

fn cam_readers() -> bool {
    let me = std::process::id().to_string();
    std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|p| {
        let pid = p.file_name();
        let pid = pid.to_string_lossy();
        if !pid.bytes().all(|b| b.is_ascii_digit()) || pid == me {
            return false;
        }
        std::fs::read_dir(p.path().join("fd")).into_iter().flatten().flatten()
            .any(|fd| std::fs::read_link(fd.path()).map(|l| l.as_os_str() == VIDEO).unwrap_or(false))
    })
}

fn get(path: &str) -> Option<serde_json::Value> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], 6083).into(), Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\n\r\n").ok()?;
    let mut r = String::new();
    s.read_to_string(&mut r).ok()?;
    serde_json::from_str(r.split_once("\r\n\r\n")?.1).ok()
}

fn auto() {
    let (mut on, mut idle_since, mut session) = ([false; 2], [None::<Instant>; 2], None);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let sid = crate::host::session_since();
        if sid != session {
            session = sid;
            on = [false; 2];
            idle_since = [None; 2];
            crate::audio::ensure_mic_source();
        }
        if sid.is_none() {
            continue;
        }
        let used = [std::path::Path::new(VIDEO).exists() && cam_readers(), crate::audio::mic_in_use()];
        for (i, key) in ["cam", "mic"].into_iter().enumerate() {
            if used[i] {
                idle_since[i] = None;
                if !on[i] {
                    if let Some(st) = get("/state") {
                        if !st[key].as_bool().unwrap_or(false) && get(&format!("/set?{key}=1&auto=1")).is_some() {
                            on[i] = true;
                            crate::host::log(&format!("una app usa {key}: encendido en el móvil"));
                        }
                    }
                }
            } else if on[i] {
                let t = *idle_since[i].get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_secs(10) {
                    let _ = get(&format!("/set?{key}=0"));
                    on[i] = false;
                    idle_since[i] = None;
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
