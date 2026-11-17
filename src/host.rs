//! Red: descubrimiento (UDP 47800), sesión Noise (TCP 47801), permiso y estado para la barra
//! (127.0.0.1:47802 + movild.json). Mismo protocolo que movild.py, para que la app no cambie.

use crate::util::{self, unix_time};
use blake2::digest::consts::{U4, U8};
use blake2::{Blake2s, Digest};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, Notify};

const PROTO: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const CHUNK: usize = 60000;

pub struct Identity {
    pub private: [u8; 32],
    pub public: [u8; 32],
    pub id: String,
}

static IDENT: OnceLock<Identity> = OnceLock::new();
/// origen de tiempos de los ecos (el mismo para todas las sesiones)
static ECHO_T0: OnceLock<std::time::Instant> = OnceLock::new();
static STATE: OnceLock<Mutex<St>> = OnceLock::new();

/// Un móvil conectado (una "ranura": 1, 2…; la 1 usa los nombres de siempre)
#[derive(Default)]
struct Sess {
    peer: Value,
    since: f64,
    device: String,
    fp: String,
    phone: Option<Value>,
    standby: bool,
    /// hay escritorio (monitor) para este móvil
    screen: bool,
    storage: Option<String>,
    ctl: Option<mpsc::UnboundedSender<(u8, Vec<u8>)>>,
    kick: Option<Arc<Notify>>,
    /// estado de cámara/micro (lo mismo que daba media.py en /state)
    mic_on: bool,
    mic_ok: bool,
    cam_on: bool,
    cam_ok: bool,
    /// "user" (delantera) o "environment" (trasera)
    facing: String,
    media_error: String,
    /// por qué este móvil no tiene webcam en el ordenador (si no la tiene)
    cam_unavailable: Option<String>,
    /// vídeo del escritorio: compresor, imágenes/s, ms por imagen…
    video: Option<Arc<crate::rfb::Stats>>,
    /// ida y vuelta por la red (eco que contesta la app ≥ 2.1), ms
    rtt_ms: Option<f32>,
    #[cfg(target_os = "linux")]
    mic: Option<crate::audio::Mic>,
}

struct Pending {
    info: Value,
    tx: Option<oneshot::Sender<bool>>,
}

/// Estado compartido (lo que se escribe en movild.json + lo necesario para /answer, /disconnect…).
#[derive(Default)]
struct St {
    sessions: std::collections::BTreeMap<u8, Sess>,
    pendings: Vec<Pending>,
}

/// Cuántos móviles a la vez (en Windows, uno: solo hay un monitor virtual o se duplica la pantalla)
fn max_phones() -> u8 {
    if cfg!(windows) {
        return 1;
    }
    util::conf("MAX_PHONES").and_then(|v| v.parse().ok()).unwrap_or(util::MAX_PHONES).clamp(1, util::MAX_PHONES)
}

/// (ranura, marca de su sesión) de cada móvil conectado: para olvidar lo encendido en una sesión anterior
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn session_marks() -> Vec<(u8, u64)> {
    st().sessions.iter().map(|(k, v)| (*k, v.since.to_bits())).collect()
}

fn st() -> std::sync::MutexGuard<'static, St> {
    STATE.get_or_init(|| Mutex::new(St::default())).lock().unwrap_or_else(|e| e.into_inner())
}

fn ident() -> &'static Identity {
    IDENT.get().expect("identidad")
}

pub fn log(msg: &str) {
    crate::dbg_log!("{msg}");
    crate::seclog::append(&util::state_dir().join("movild.log"), &[msg.to_string()], true);
}

fn app_log(lines: &[Value]) {
    let lines: Vec<String> = lines.iter().take(500).map(|l| l.as_str().map(String::from).unwrap_or_else(|| l.to_string())).collect();
    crate::seclog::append(&util::state_dir().join("app-movil.log"), &lines, false);
}

pub fn status_path() -> PathBuf {
    std::env::var_os("MOVILD_STATUS").map(PathBuf::from).unwrap_or_else(|| util::state_dir().join("movild.json"))
}

fn facing(s: &Sess) -> String {
    if s.facing.is_empty() { "user".into() } else { s.facing.clone() }
}

/// Cámara/micro de un móvil (lo de /state, más qué dispositivos son en este ordenador)
fn media_json(slot: u8, s: &Sess) -> Value {
    #[cfg(target_os = "linux")]
    let (fps, cam_dev) = (crate::devices::fps(slot), crate::devices::webcam(slot));
    #[cfg(not(target_os = "linux"))]
    let (fps, cam_dev): (f32, Option<(String, String)>) = (0.0, None);
    #[cfg(target_os = "linux")]
    let mic_source = crate::audio::mic_name(slot);
    #[cfg(not(target_os = "linux"))]
    let mic_source = String::new();
    json!({"s": slot, "device": s.device, "cam": s.cam_on, "mic": s.mic_on, "facing": facing(s), "codec": "h264",
           "error": s.media_error, "camOk": s.cam_ok, "micOk": s.mic_ok, "fps": fps,
           "using": if s.cam_on { "h264" } else { "" }, "host": "pantalla-host",
           "video": cam_dev.as_ref().map(|d| d.0.clone()), "cam_label": cam_dev.map(|d| d.1),
           "cam_unavailable": s.cam_unavailable,
           "mic_source": mic_source,
           "mic_label": if slot <= 1 { "Micrófono del móvil".to_string() } else { format!("Micrófono de {}", s.device) }})
}

fn session_json(slot: u8, s: &Sess) -> Value {
    #[cfg(target_os = "linux")]
    let sink = Some(crate::audio::sink_name(slot));
    #[cfg(not(target_os = "linux"))]
    let sink: Option<String> = None;
    json!({"s": slot, "output": if s.screen { Some(util::output_name(slot)) } else { None },
           "peer": s.peer, "since": s.since, "phone": s.phone, "standby": s.standby,
           "position": util::position(slot), "ws": util::phone_workspace(slot), "storage": s.storage,
           "sink": sink, "media": media_json(slot, s),
           "video": s.video.as_ref().map(|v| {
               let r = |x: f32| ((x as f64) * 10.0).round() / 10.0;
               let i = v.get();
               json!({"encoder": i.encoder, "fps": r(i.fps), "ms": r(i.ms), "age_ms": r(i.age_ms), "kbps": i.kbps.round(),
                      "rtt_ms": s.rtt_ms.map(r)})
           })})
}

fn write_status() {
    let s = st();
    let first = s.sessions.values().next();
    let pendings: Vec<Value> = s.pendings.iter().map(|p| p.info.clone()).collect();
    let v = json!({
        // compatibilidad (barra y paneles de antes): lo del primer móvil
        "connected": first.is_some(), "peer": first.map(|f| f.peer.clone()), "since": first.map(|f| f.since),
        "pending": pendings.first(), "standby": first.map(|f| f.standby).unwrap_or(false),
        "phone": first.and_then(|f| f.phone.clone()),
        // todos
        "pendings": pendings, "max": max_phones(),
        "sessions": s.sessions.iter().map(|(k, v)| session_json(*k, v)).collect::<Vec<_>>(),
    });
    drop(s);
    // se escribe sobre el mismo archivo (no se reemplaza): Quickshell lo vigila por inotify
    let _ = std::fs::write(status_path(), v.to_string());
}

// ───────────── Identidad ─────────────

pub fn load_identity() -> std::io::Result<()> {
    let dir = util::state_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("movild.key");
    let private: [u8; 32] = match std::fs::read(&path) {
        Ok(k) if k.len() == 32 => k.try_into().unwrap(),
        _ => {
            let mut k = [0u8; 32];
            getrandom::getrandom(&mut k).expect("getrandom");
            std::fs::write(&path, k)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            k
        }
    };
    let public = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(private)).to_bytes();
    let id = hex(&Blake2s::<U8>::digest(public));
    let _ = IDENT.set(Identity { private, public, id });
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Huella corta y legible (la misma que enseña la app): "ABCD-EF01".
pub fn fingerprint(key: &[u8]) -> String {
    let h = hex(&Blake2s::<U4>::digest(key)).to_uppercase();
    format!("{}-{}", &h[..4], &h[4..])
}

pub fn fingerprint_self() -> String {
    fingerprint(&ident().public)
}

pub fn host_name() -> String {
    if let Ok(n) = std::env::var("MOVILD_NAME") {
        return n;
    }
    if let Some(n) = util::conf("NAME") {
        return n;
    }
    let h = hostname();
    match h.as_str() {
        "archlinuxMaiki" => "PC".into(),
        "debian" => "Portátil".into(),
        "nobara" => "Nobara".into(),
        _ => h,
    }
}

fn hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            return s.trim().to_string();
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Ordenador".into())
}

fn os_name() -> String {
    #[cfg(windows)]
    {
        return "Windows".into();
    }
    #[allow(unreachable_code)]
    {
        if let Ok(t) = std::fs::read_to_string("/etc/os-release") {
            for l in t.lines() {
                if let Some(v) = l.strip_prefix("NAME=") {
                    return v.trim_matches('"').split_whitespace().next().unwrap_or("Linux").to_string();
                }
            }
        }
        "Linux".into()
    }
}

fn platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "linux" }
}

fn info() -> Value {
    let i = ident();
    json!({"t": "info", "name": host_name(), "os": os_name(), "platform": platform(),
           "id": i.id, "key": hex(&i.public), "fp": fingerprint(&i.public),
           "busy": st().sessions.len() >= max_phones() as usize, "phones": st().sessions.len(), "host": "pantalla-host", "v": env!("CARGO_PKG_VERSION")})
}

// ───────────── Descubrimiento ─────────────

async fn discovery(port: u16) -> std::io::Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", port)).await?;
    sock.set_broadcast(true)?;
    let mut buf = [0u8; 512];
    loop {
        let (n, addr) = sock.recv_from(&mut buf).await?;
        if buf[..n].trim_ascii() == b"MOVIL?" {
            let _ = sock.send_to(info().to_string().as_bytes(), addr).await;
        }
    }
}

// ───────────── Permiso ─────────────

#[cfg(unix)]
fn quickshell_running() -> bool {
    std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        std::fs::read_to_string(e.path().join("comm")).map(|c| matches!(c.trim(), "qs" | "quickshell")).unwrap_or(false)
    })
}

async fn ask_user(device: &str, fp: &str, via: &str) -> bool {
    const TIMEOUT: u64 = 30;
    if std::env::var("MOVILD_TEST_ACCEPT").as_deref() == Ok("1") {
        return true;
    }
    // se abre el panel "Escritorio" para enseñar la tarjeta (con o sin la barra Quickshell)
    #[cfg(target_os = "linux")]
    if !crate::panel::alive() && util::conf("PANEL").as_deref() != Some("no") {
        crate::panel::spawn();
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if crate::panel::alive() {
                break;
            }
        }
    }
    #[cfg(target_os = "linux")]
    let card = quickshell_running() || crate::panel::alive();
    #[cfg(all(unix, not(target_os = "linux")))]
    let card = quickshell_running();
    #[cfg(unix)]
    if card {
        let (tx, rx) = oneshot::channel();
        let mut idb = [0u8; 6];
        getrandom::getrandom(&mut idb).unwrap();
        let id = hex(&idb);
        let connected = st().sessions.len();
        {
            let mut s = st();
            s.pendings.push(Pending {
                info: json!({"id": id, "device": device, "fp": fp, "via": via, "connected": connected,
                             "expires": unix_time() + TIMEOUT as f64, "timeout": TIMEOUT}),
                tx: Some(tx),
            });
        }
        write_status();
        let r = tokio::time::timeout(Duration::from_secs(TIMEOUT), rx).await;
        st().pendings.retain(|p| p.info["id"].as_str() != Some(id.as_str()));
        write_status();
        return matches!(r, Ok(Ok(true)));
    }
    crate::notify::ask(device, fp, via, TIMEOUT).await
}

type Query<'a> = std::collections::HashMap<&'a str, &'a str>;

/// El móvil al que va una orden: `s=N` (ranura) o, sin él, el primero conectado
fn pick(q: &Query) -> Option<u8> {
    let s = st();
    match q.get("s").and_then(|v| v.parse::<u8>().ok()) {
        Some(n) => s.sessions.contains_key(&n).then_some(n),
        None => s.sessions.keys().next().copied(),
    }
}

fn ctl_of(slot: u8) -> Option<mpsc::UnboundedSender<(u8, Vec<u8>)>> {
    st().sessions.get(&slot).and_then(|s| s.ctl.clone())
}

/// GET /answer?id=…&ok=1|0, /disconnect, /screen?on=, /brightness?v=, /standby?on=, /position?p=
/// (todas con &s=N para elegir el móvil; sin él, el primero)  (solo 127.0.0.1)
async fn answer_http(stream: TcpStream) {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    if tokio::time::timeout(Duration::from_secs(3), r.read_line(&mut line)).await.is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let (route, qs) = path.split_once('?').unwrap_or((&path, ""));
    let q: Query = qs.split('&').filter_map(|kv| kv.split_once('=')).collect();
    let slot = pick(&q);
    let ctl = slot.and_then(ctl_of);
    let send = |v: Value| ctl.as_ref().map(|c| c.send((0, v.to_string().into_bytes())).is_ok()).unwrap_or(false);
    let ok = match route {
        "/disconnect" => slot.and_then(|n| st().sessions.get(&n).and_then(|s| s.kick.clone())).map(|k| k.notify_waiters()).is_some(),
        "/media" => match (q.get("mic"), slot) {
            (Some(v), Some(n)) => {
                let cur = st().sessions.get(&n).map(|s| s.mic_on).unwrap_or(false);
                let on = if *v == "toggle" { !cur } else { matches!(*v, "1" | "on" | "true") };
                set_mic(n, on).await
            }
            _ => false,
        },
        "/screen" => send(json!({"t": "screen", "on": matches!(q.get("on").copied().unwrap_or("1"), "1" | "on")})),
        "/position" => {
            let p = q.get("p").copied().unwrap_or("");
            // sin móvil conectado vale para el primero (o el de s=N), así se puede dejar elegido antes
            let n = slot.or_else(|| q.get("s").and_then(|v| v.parse().ok())).unwrap_or(1);
            if matches!(p, "left" | "right" | "up" | "down") {
                let _ = util::set_conf(&util::position_key(n), &format!("auto-{p}"));
                #[cfg(target_os = "linux")]
                tokio::task::spawn_blocking(move || crate::linux::reposition(n));
                write_status();
                true
            } else {
                false
            }
        }
        "/brightness" => send(json!({"t": "brightness", "v": q.get("v").and_then(|v| v.parse::<f64>().ok()).unwrap_or(-1.0)})),
        "/standby" => {
            let Some(n) = slot else { return reply(&mut r, false).await };
            let cur = st().sessions.get(&n).map(|s| s.standby).unwrap_or(false);
            let on = match q.get("on").copied().unwrap_or("toggle") {
                "toggle" => !cur,
                v => matches!(v, "1" | "on"),
            };
            let ok = send(json!({"t": "standby", "on": on}));
            if ok {
                if let Some(s) = st().sessions.get_mut(&n) {
                    s.standby = on;
                }
                write_status();
            }
            ok
        }
        _ => {
            let mut s = st();
            let id = q.get("id").copied();
            match s.pendings.iter_mut().find(|p| p.info["id"].as_str() == id).and_then(|p| p.tx.take()) {
                Some(tx) => {
                    let _ = tx.send(q.get("ok").copied() == Some("1"));
                    true
                }
                None => false,
            }
        }
    };
    reply(&mut r, ok).await;
}

async fn reply(r: &mut BufReader<TcpStream>, ok: bool) {
    let status = if ok { "200 OK" } else { "404 Not Found" };
    let _ = r.get_mut().write_all(format!("HTTP/1.0 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
}

// ───────────── Cámara / micrófono (compatible con /state y /set de media.py) ─────────────

/// /state: el móvil pedido (o el primero) y, sin s=, también la lista de todos
fn media_state(q: &Query) -> Value {
    let slot = pick(q);
    let s = st();
    let mut v = match slot.and_then(|n| s.sessions.get(&n).map(|x| (n, x))) {
        Some((n, x)) => media_json(n, x),
        None => json!({"cam": false, "mic": false, "facing": "user", "codec": "h264", "error": "", "camOk": false,
                       "micOk": false, "fps": 0.0, "using": "", "host": "pantalla-host"}),
    };
    if !q.contains_key("s") {
        v["sessions"] = Value::Array(s.sessions.iter().map(|(k, x)| media_json(*k, x)).collect());
    }
    v
}

/// RGB PREVIEW_W x PREVIEW_H → BMP de 24 bits (filas de abajo arriba, en BGR)
#[cfg(target_os = "linux")]
fn to_bmp(rgb: &[u8]) -> Vec<u8> {
    let (w, h) = (crate::devices::PREVIEW_W, crate::devices::PREVIEW_H);
    let row = w * 3; // 640*3 ya es múltiplo de 4: sin relleno
    let size = 54 + row * h;
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(b"BM");
    for x in [size as u32, 0, 54, 40, w as u32, h as u32] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&24u16.to_le_bytes());
    for x in [0u32, (row * h) as u32, 2835, 2835, 0, 0] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    for y in (0..h).rev() {
        for p in rgb[y * row..(y + 1) * row].chunks(3) {
            v.extend_from_slice(&[p[2], p[1], p[0]]);
        }
    }
    v
}

/// Enciende/apaga el micrófono del móvil `slot` como fuente de este equipo.
async fn set_mic(slot: u8, on: bool) -> bool {
    let Some(ctl) = ctl_of(slot) else { return false };
    #[cfg(target_os = "linux")]
    {
        let (cur, device) = st().sessions.get(&slot).map(|s| (s.mic.is_some(), s.device.clone())).unwrap_or_default();
        if on && !cur {
            let m = tokio::task::spawn_blocking(move || crate::audio::Mic::start(slot, &device)).await.ok().flatten();
            let mut s = st();
            let Some(x) = s.sessions.get_mut(&slot) else { return false };
            if m.is_none() {
                x.media_error = "No se pudo crear el micrófono (¿falta pactl?)".into();
            }
            x.mic = m;
        } else if !on && cur {
            let m = st().sessions.get_mut(&slot).and_then(|x| x.mic.take());
            let _ = tokio::task::spawn_blocking(move || drop(m)).await;
        }
    }
    {
        let mut s = st();
        let Some(x) = s.sessions.get_mut(&slot) else { return false };
        x.mic_on = on;
        if !on {
            x.mic_ok = false;
        }
    }
    let _ = ctl.send((0, json!({"t": "mic", "on": on}).to_string().into_bytes()));
    send_media(slot, &ctl);
    write_status();
    true
}

/// Estado de cámara/micro a la app (ella enciende o apaga lo suyo)
fn send_media(slot: u8, ctl: &mpsc::UnboundedSender<(u8, Vec<u8>)>) {
    let s = st();
    let Some(x) = s.sessions.get(&slot) else { return };
    let m = json!({"t": "media", "cam": x.cam_on, "mic": x.mic_on, "facing": facing(x)});
    drop(s);
    let _ = ctl.send((0, m.to_string().into_bytes()));
}

/// Enciende/apaga la cámara del móvil `slot` (su webcam en el ordenador); flip cambia delantera/trasera
fn set_cam(slot: u8, on: Option<bool>, flip: bool) -> bool {
    let Some(ctl) = ctl_of(slot) else { return false };
    // sin webcam al conectar (p. ej. aún sin preparar el sistema): se reintenta al encender la cámara
    #[cfg(target_os = "linux")]
    if on == Some(true) {
        let (missing, device) = st().sessions.get(&slot).map(|x| (x.cam_unavailable.is_some(), x.device.clone())).unwrap_or_default();
        if missing {
            let r = crate::devices::open_slot(slot, &device).err();
            with_sess(slot, |x| {
                x.cam_unavailable = r.clone();
                if r.is_none() {
                    x.media_error.clear();
                }
            });
        }
    }
    {
        let mut s = st();
        let Some(x) = s.sessions.get_mut(&slot) else { return false };
        if flip {
            x.facing = if facing(x) == "user" { "environment".into() } else { "user".into() };
        }
        if let Some(on) = on {
            x.cam_on = on;
            if !on {
                x.cam_ok = false;
            }
        }
        #[cfg(target_os = "linux")]
        if !x.cam_on {
            crate::devices::cam_off(slot);
        }
        // sin webcam en este ordenador se puede ver igual en el panel, pero se avisa
        if x.cam_on {
            if let Some(why) = x.cam_unavailable.clone() {
                x.media_error = why;
            }
        }
        #[cfg(not(target_os = "linux"))]
        if x.cam_on {
            x.media_error = "La cámara del móvil aún no funciona en Windows".into();
        }
    }
    send_media(slot, &ctl);
    write_status();
    true
}

async fn media_http(stream: TcpStream) {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    if tokio::time::timeout(Duration::from_secs(3), r.read_line(&mut line)).await.is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let (route, qs) = path.split_once('?').unwrap_or((&path, ""));
    let q: Query = qs.split('&').filter_map(|kv| kv.split_once('=')).collect();
    // GET /preview: la última imagen de la cámara en pequeño (RGB, PREVIEW_W x PREVIEW_H) para el panel
    // GET /preview.bmp: lo mismo como imagen BMP (la barra Quickshell la enseña con un Image normal)
    #[cfg(target_os = "linux")]
    if route == "/preview" || route == "/preview.bmp" {
        let bmp = route.ends_with(".bmp");
        let resp = match pick(&q).and_then(crate::devices::preview) {
            Some(px) => {
                let (px, ct) = if bmp { (to_bmp(&px), "image/bmp") } else { (px, "application/octet-stream") };
                let mut v = format!("HTTP/1.0 200 OK\r\nContent-Type: {ct}\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n", px.len()).into_bytes();
                v.extend_from_slice(&px);
                v
            }
            None => b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        };
        let _ = r.get_mut().write_all(&resp).await;
        return;
    }
    let body = match route {
        "/state" => Some(media_state(&q)),
        "/set" => {
            if let Some(n) = pick(&q) {
                let (mic_on, cam_on, cur_facing) =
                    st().sessions.get(&n).map(|x| (x.mic_on, x.cam_on, facing(x))).unwrap_or_default();
                if let Some(v) = q.get("mic") {
                    let on = if *v == "toggle" { !mic_on } else { matches!(*v, "1" | "on" | "true") };
                    set_mic(n, on).await;
                }
                let cam = q.get("cam").map(|v| if *v == "toggle" { !cam_on } else { matches!(*v, "1" | "on" | "true") });
                let flip = q.get("facing").copied() == Some("flip")
                    || q.get("facing").map(|f| *f != cur_facing && matches!(*f, "user" | "environment")).unwrap_or(false);
                if cam.is_some() || flip {
                    set_cam(n, cam, flip);
                }
                #[cfg(target_os = "linux")]
                if cam == Some(true) && !q.contains_key("auto") && !crate::panel::alive() && !quickshell_running() {
                    crate::panel::spawn();
                }
            }
            Some(media_state(&q))
        }
        _ => None,
    };
    let resp = match body {
        Some(b) => {
            let b = b.to_string();
            format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len())
        }
        None => "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
    };
    let _ = r.get_mut().write_all(resp.as_bytes()).await;
}

// ───────────── Sesión ─────────────

async fn read_frame(r: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<Vec<u8>> {
    let n = r.read_u16().await? as usize;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await?;
    Ok(b)
}

async fn write_frame(w: &mut (impl AsyncWriteExt + Unpin), d: &[u8]) -> std::io::Result<()> {
    let mut m = Vec::with_capacity(d.len() + 2);
    m.extend_from_slice(&(d.len() as u16).to_be_bytes());
    m.extend_from_slice(d);
    w.write_all(&m).await
}

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn handle(mut sock: TcpStream) -> Res<()> {
    let peer_ip = sock.peer_addr()?.ip();
    let via = if peer_ip.is_loopback() { "cable" } else { "wifi" };
    let mut mode = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut mode)).await??;
    if mode[0] == b'I' {
        sock.write_all(info().to_string().as_bytes()).await?;
        return Ok(());
    }
    if mode[0] != b'N' {
        return Ok(());
    }

    // Saludo Noise XX (este equipo responde)
    let mut hs = snow::Builder::new(PROTO.parse()?).local_private_key(&ident().private).build_responder()?;
    let mut buf = vec![0u8; 65535];
    let t10 = Duration::from_secs(10);
    let m1 = tokio::time::timeout(t10, read_frame(&mut sock)).await??;
    hs.read_message(&m1, &mut buf)?;
    let n = hs.write_message(&[], &mut buf)?;
    write_frame(&mut sock, &buf[..n]).await?;
    let m3 = tokio::time::timeout(t10, read_frame(&mut sock)).await??;
    hs.read_message(&m3, &mut buf)?;
    let phone_key = hs.get_remote_static().ok_or("sin clave del móvil")?.to_vec();
    let fp = fingerprint(&phone_key);
    let noise = Arc::new(Mutex::new(hs.into_transport_mode()?));

    let (mut rd, mut wr) = sock.into_split();
    let decrypt = |noise: &Arc<Mutex<snow::TransportState>>, c: &[u8]| -> Res<Vec<u8>> {
        let mut out = vec![0u8; c.len()];
        let n = noise.lock().unwrap().read_message(c, &mut out)?;
        out.truncate(n);
        Ok(out)
    };
    let encrypt = |noise: &Arc<Mutex<snow::TransportState>>, p: &[u8]| -> Res<Vec<u8>> {
        let mut out = vec![0u8; p.len() + 16];
        let n = noise.lock().unwrap().write_message(p, &mut out)?;
        out.truncate(n);
        Ok(out)
    };
    let send_json = |v: Value| {
        let mut p = vec![0u8];
        p.extend_from_slice(v.to_string().as_bytes());
        p
    };

    let hello: Value = {
        let m = decrypt(&noise, &tokio::time::timeout(t10, read_frame(&mut rd)).await??)?;
        if m.first() == Some(&0) { serde_json::from_slice(&m[1..]).unwrap_or(Value::Null) } else { Value::Null }
    };
    crate::dbg_log!("hola del móvil: {hello}");
    let device: String = hello.get("device").and_then(|d| d.as_str()).unwrap_or("Móvil").chars().take(40).collect();
    // el mismo móvil otra vez (se cortó la red y vuelve antes de que caduque la sesión vieja):
    // se cierra la vieja en vez de abrirle un segundo escritorio
    let old = st().sessions.iter().find(|(_, x)| x.fp == fp).map(|(k, x)| (*k, x.kick.clone()));
    if let Some((n, kick)) = old {
        log(&format!("{device} vuelve a conectarse: se cierra su sesión anterior (móvil {n})"));
        if let Some(k) = kick {
            k.notify_waiters();
        }
        for _ in 0..50 {
            if !st().sessions.contains_key(&n) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if st().sessions.len() >= max_phones() as usize {
        let why = if max_phones() == 1 { "Ya hay un móvil conectado a este equipo".to_string() }
                  else { format!("Ya hay {} móviles conectados a este equipo", max_phones()) };
        let c = encrypt(&noise, &send_json(json!({"t": "rejected", "why": why})))?;
        write_frame(&mut wr, &c).await?;
        return Ok(());
    }
    write_frame(&mut wr, &encrypt(&noise, &send_json(json!({"t": "waiting"})))?).await?;
    log(&format!("petición de {device} ({fp}) por {via}"));
    if !ask_user(&device, &fp, via).await {
        write_frame(&mut wr, &encrypt(&noise, &send_json(json!({"t": "rejected", "why": "Rechazado en el equipo"})))?).await?;
        log("rechazada");
        return Ok(());
    }
    // ranura: la primera libre (la 1 = nombres de siempre: MOVIL, ~/Móvil, /dev/video42…)
    let slot = {
        let mut s = st();
        let n = (1..=max_phones()).find(|n| !s.sessions.contains_key(n));
        if let Some(n) = n {
            s.sessions.insert(n, Sess {
                peer: json!({"device": device, "fp": fp, "via": via}),
                since: unix_time(),
                device: device.clone(),
                fp: fp.clone(),
                ..Default::default()
            });
        }
        n
    };
    let Some(slot) = slot else {
        let c = encrypt(&noise, &send_json(json!({"t": "rejected", "why": "No quedan huecos para más móviles"})))?;
        write_frame(&mut wr, &c).await?;
        return Ok(());
    };
    write_status();
    log(&format!("aceptada: {device} por {via} (móvil {slot})"));

    let result = session(rd, wr, noise, &hello, slot, device).await;

    st().sessions.remove(&slot);
    write_status();
    if let Err(e) = &result {
        log(&format!("sesión terminada (móvil {slot}): {e}"));
    } else {
        log(&format!("sesión terminada (móvil {slot})"));
    }
    Ok(())
}

/// Cambia algo de la sesión `slot` (si sigue ahí)
fn with_sess(slot: u8, f: impl FnOnce(&mut Sess)) {
    if let Some(x) = st().sessions.get_mut(&slot) {
        f(x);
    }
}

async fn session(
    mut rd: tokio::net::tcp::OwnedReadHalf,
    mut wr: tokio::net::tcp::OwnedWriteHalf,
    noise: Arc<Mutex<snow::TransportState>>,
    hello: &Value,
    slot: u8,
    device: String,
) -> Res<()> {
    // Escritor: todo lo que se manda pasa por aquí (canal, datos) → cifrado en trozos de 60 000
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let noise_w = noise.clone();
    let writer = tokio::spawn(async move {
        let mut cbuf = vec![0u8; 65535];
        while let Some((ch, data)) = out_rx.recv().await {
            let mut i = 0;
            loop {
                let end = (i + CHUNK).min(data.len());
                let mut p = Vec::with_capacity(end - i + 1);
                p.push(ch);
                p.extend_from_slice(&data[i..end]);
                let n = match noise_w.lock().unwrap().write_message(&p, &mut cbuf) {
                    Ok(n) => n,
                    Err(_) => return,
                };
                if write_frame(&mut wr, &cbuf[..n]).await.is_err() {
                    return;
                }
                i = end;
                if i >= data.len() {
                    break;
                }
            }
        }
        let _ = wr.shutdown().await;
    });

    let kick = Arc::new(Notify::new());
    {
        let (o, k) = (out_tx.clone(), kick.clone());
        with_sess(slot, |x| {
            x.ctl = Some(o);
            x.kick = Some(k);
        });
    }

    let want: Vec<String> = hello.get("want").and_then(|w| w.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let stop = Arc::new(AtomicBool::new(false));

    // Canal 1: escritorio. Primero se crea el monitor, luego va "accepted" y solo después
    // empieza el RFB: la app descarta lo que llegue por el canal 1 antes de "accepted".
    let mut screen_in: Option<std::sync::mpsc::Sender<Vec<u8>>> = None;
    let mut display = None;
    let mut screen_error = None;
    if want.iter().any(|w| w == "screen") {
        let (w, h) = screen_size(hello);
        match tokio::task::spawn_blocking(move || crate::display::open(w, h, slot)).await? {
            Ok(d) => display = Some(d),
            Err(e) => {
                log(&format!("pantalla (móvil {slot}): {e}"));
                screen_error = Some(e);
            }
        }
    }
    with_sess(slot, |x| x.screen = display.is_some());

    // webcam y micrófono de este móvil en el ordenador (los del 1º ya existen siempre)
    #[cfg(target_os = "linux")]
    {
        let dev = device.clone();
        let (cam_err, _) = tokio::task::spawn_blocking(move || {
            crate::audio::add_mic_source(slot, &dev);
            (crate::devices::open_slot(slot, &dev).err(), ())
        })
        .await?;
        if let Some(e) = &cam_err {
            log(&format!("webcam (móvil {slot}): {e}"));
        }
        with_sess(slot, |x| x.cam_unavailable = cam_err);
    }
    write_status();

    // si no hay escritorio, el motivo va al móvil para que lo enseñe
    let accepted = json!({"t": "accepted", "name": host_name(), "screen": display.is_some(), "screen_error": screen_error});
    out_tx.send((0, accepted.to_string().into_bytes()))?;

    if let Some(d) = &display {
        let (in_tx, in_rx) = std::sync::mpsc::channel();
        let (v_tx, mut v_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let stats = Arc::new(crate::rfb::Stats::default());
        with_sess(slot, |x| x.video = Some(stats.clone()));
        crate::rfb::spawn(d.clone(), in_rx, v_tx, stop.clone(), 60.0, stats.clone());
        // el panel ve fps/ms al momento: cada 3 s se reescribe el estado mientras dura la sesión, y se
        // manda un eco a la app para medir la ida y vuelta por la red (la app ≥ 2.1 lo devuelve tal cual)
        {
            let (stop, out) = (stop.clone(), out_tx.clone());
            tokio::spawn(async move {
                let t0 = *ECHO_T0.get_or_init(std::time::Instant::now);
                while !stop.load(Ordering::Relaxed) {
                    let ts = t0.elapsed().as_micros() as u64;
                    let _ = out.send((0, json!({"t": "echo", "ts": ts, "host": true}).to_string().into_bytes()));
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    write_status();
                }
            });
        }
        let out = out_tx.clone();
        tokio::spawn(async move {
            while let Some(m) = v_rx.recv().await {
                if out.send((1, m)).is_err() {
                    break;
                }
            }
        });
        screen_in = Some(in_tx);
    }

    // Canal 2: altavoces del móvil
    #[cfg(target_os = "linux")]
    let speakers = if want.iter().any(|w| w == "audio") {
        let (out, dev) = (out_tx.clone(), device.clone());
        tokio::task::spawn_blocking(move || crate::audio::Speakers::start(out, slot, &dev)).await.ok().flatten()
    } else {
        None
    };
    #[cfg(windows)]
    let _speakers = if want.iter().any(|w| w == "audio") { crate::winaudio::Speakers::start(out_tx.clone()) } else { None };

    // Canal 5: almacén del móvil → ~/Móvil (el 1º) o "~/Móvil (<nombre>)" (los demás), por FUSE
    #[cfg(target_os = "linux")]
    let (store, fuse, mp) = if want.iter().any(|w| w == "storage") {
        let r = crate::storage::Remote::new(out_tx.clone());
        let r2 = r.clone();
        let mp = crate::storage::mount_point(slot, &device);
        let mp2 = mp.clone();
        match tokio::task::spawn_blocking(move || crate::storage::mount(r2, &mp2)).await? {
            Ok(f) => {
                log(&format!("almacén: montado en {}", mp.display()));
                with_sess(slot, |x| x.storage = Some(mp.to_string_lossy().into_owned()));
                write_status();
                (Some(r), Some(f), Some(mp))
            }
            Err(e) => {
                log(&format!("almacén: no se pudo montar ({e})"));
                (None, None, None)
            }
        }
    } else {
        (None, None, None)
    };

    let res: Res<()> = loop {
        let frame = tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(30), read_frame(&mut rd)) => r,
            _ = kick.notified() => break Ok(()),
        };
        let frame = match frame {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => break Err(e.into()),
            Err(_) => break Err("sin noticias del móvil en 30 s".into()),
        };
        let msg = {
            let mut out = vec![0u8; frame.len()];
            match noise.lock().unwrap().read_message(&frame, &mut out) {
                Ok(n) => {
                    out.truncate(n);
                    out
                }
                Err(e) => break Err(e.into()),
            }
        };
        if msg.is_empty() {
            continue;
        }
        let (ch, body) = (msg[0], &msg[1..]);
        match ch {
            0 => {
                let m: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                match m.get("t").and_then(|t| t.as_str()).unwrap_or("") {
                    "log" => app_log(m.get("lines").and_then(|l| l.as_array()).map(|v| v.as_slice()).unwrap_or(&[])),
                    "echo" => {
                        // eco de la app: ida y vuelta por la red
                        if let (Some(ts), Some(t0)) = (m["ts"].as_u64(), ECHO_T0.get()) {
                            let now = t0.elapsed().as_micros() as u64;
                            if now >= ts {
                                let ms = (now - ts) as f32 / 1000.0;
                                with_sess(slot, |x| x.rtt_ms = Some(ms));
                            }
                        }
                    }
                    "ping" => {
                        let _ = out_tx.send((0, br#"{"t": "pong"}"#.to_vec()));
                    }
                    "bye" => break Ok(()),
                    "info" => {
                        let mut m = m.clone();
                        m.as_object_mut().map(|o| o.remove("t"));
                        with_sess(slot, |x| x.phone = Some(m));
                        write_status();
                    }
                    "standby_off" => {
                        with_sess(slot, |x| x.standby = false);
                        write_status();
                    }
                    // botones del menú de la app (igual que los de la barra)
                    "media_set" => {
                        if let Some(on) = m["mic"].as_bool() {
                            tokio::spawn(set_mic(slot, on));
                        }
                        let flip = m["facing"].is_string();
                        if m["cam"].is_boolean() || flip {
                            set_cam(slot, m["cam"].as_bool(), flip);
                        }
                        // encendida a mano: el panel enseña lo que ve la cámara
                        #[cfg(target_os = "linux")]
                        if m["cam"].as_bool() == Some(true) && !crate::panel::alive() {
                            crate::panel::spawn();
                        }
                    }
                    "cam_rot" => {
                        #[cfg(target_os = "linux")]
                        crate::devices::set_rot(slot, m["deg"].as_i64().unwrap_or(0) as i32);
                    }
                    "standby_set" => {
                        let on = m["on"].as_bool().unwrap_or(false);
                        let _ = out_tx.send((0, json!({"t": "standby", "on": on}).to_string().into_bytes()));
                        with_sess(slot, |x| x.standby = on);
                        write_status();
                    }
                    "mic_state" => {
                        log(&format!("micrófono del móvil {slot}: {m}"));
                        with_sess(slot, |x| {
                            x.mic_ok = m["on"].as_bool().unwrap_or(false);
                            x.media_error = m["why"].as_str().unwrap_or("").to_string();
                        });
                        write_status();
                    }
                    "cam_state" => {
                        with_sess(slot, |x| {
                            x.cam_ok = m["camOk"].as_bool().unwrap_or(false);
                            let e = m["error"].as_str().unwrap_or("").to_string();
                            x.media_error = if e.is_empty() { x.cam_unavailable.clone().filter(|_| x.cam_on).unwrap_or_default() } else { e };
                        });
                        write_status();
                    }
                    other => crate::dbg_log!("control sin tratar: {other} {m}"),
                }
            }
            5 => {
                #[cfg(target_os = "linux")]
                if let Some(r) = &store {
                    r.on_message(body);
                }
            }
            3 => {
                #[cfg(target_os = "linux")]
                if let Some(mic) = st().sessions.get_mut(&slot).and_then(|x| x.mic.as_mut()) {
                    mic.write(body);
                }
            }
            4 => {
                // cámara del móvil (H.264): solo si está pedida
                #[cfg(target_os = "linux")]
                if st().sessions.get(&slot).map(|x| x.cam_on).unwrap_or(false) {
                    crate::devices::feed(slot, body);
                }
            }
            1 => {
                if let Some(tx) = &screen_in {
                    if tx.send(body.to_vec()).is_err() {
                        screen_in = None;
                    }
                }
            }
            _ => {}
        }
    };

    stop.store(true, Ordering::Relaxed);
    drop(screen_in);
    #[cfg(target_os = "linux")]
    {
        let mic = st().sessions.get_mut(&slot).and_then(|x| x.mic.take());
        let had_store = fuse.is_some();
        let _ = tokio::task::spawn_blocking(move || {
            drop(mic);
            drop(speakers);
            if let (Some(f), Some(mp)) = (fuse, mp.as_ref()) {
                crate::storage::unmount(mp);
                drop(f);
            }
            crate::devices::close_slot(slot);
            crate::audio::remove_mic_source(slot);
        })
        .await;
        drop(store);
        if had_store {
            log(&format!("almacén del móvil {slot}: desmontado"));
        }
        with_sess(slot, |x| {
            x.mic_on = false;
            x.mic_ok = false;
            x.cam_on = false;
            x.cam_ok = false;
            x.media_error.clear();
        });
    }
    drop(out_tx);
    with_sess(slot, |x| x.ctl = None);
    writer.abort();
    // al soltar el último Arc se quita el monitor virtual (fuera del hilo de tokio: llama a hyprctl)
    let _ = tokio::task::spawn_blocking(move || drop(display)).await;
    res
}

/// Tamaño del monitor del móvil: lo que diga la app (si lo manda) o host.conf, o 1440x720.
fn screen_size(hello: &Value) -> (u32, u32) {
    let from_hello = |k: &str| hello.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
    if let (Some(w), Some(h)) = (from_hello("w"), from_hello("h")) {
        if (320..=7680).contains(&w) && (240..=4320).contains(&h) {
            return (w, h);
        }
    }
    if let Some(s) = util::conf("SIZE") {
        if let Some((w, h)) = s.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                return (w, h);
            }
        }
    }
    (1440, 720)
}

// ───────────── Cable (adb reverse) ─────────────

/// Cada móvil enchufado por USB (con depuración USB) llega a este equipo por el cable: adb reverse.
/// Con varios móviles enchufados `adb get-state` falla ("more than one device"): se va uno a uno.
async fn adb_watch(port: u16) {
    let mut done: std::collections::HashSet<String> = Default::default();
    loop {
        let found = tokio::process::Command::new("adb").arg("devices").output().await;
        let Ok(o) = found else {
            tokio::time::sleep(Duration::from_secs(30)).await; // adb no está: se vuelve a mirar de vez en cuando
            continue;
        };
        let serials: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().skip(1)
            .filter_map(|l| l.split_once('\t').filter(|(_, st)| st.trim() == "device").map(|(s, _)| s.to_string()))
            .collect();
        done.retain(|s| serials.contains(s));
        for serial in serials {
            let l = tokio::process::Command::new("adb").args(["-s", &serial, "reverse", "--list"]).output().await;
            let has = matches!(&l, Ok(o) if String::from_utf8_lossy(&o.stdout).contains(&format!("tcp:{port}")));
            if !has {
                let r = tokio::process::Command::new("adb")
                    .args(["-s", &serial, "reverse", &format!("tcp:{port}"), &format!("tcp:{port}")])
                    .output()
                    .await;
                if matches!(&r, Ok(o) if o.status.success()) && done.insert(serial.clone()) {
                    log(&format!("cable: adb reverse activo ({serial})"));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

// ───────────── Arranque ─────────────

/// Deja el sistema limpio: sin monitor del móvil ni salidas de sonido.
fn cleanup() {
    if util::test_mode() {
        return; // lo que hay es del servicio de verdad
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::ensure_env();
        if crate::linux::is_hyprland() {
            crate::linux::remove_all_monitors();
        }
        crate::audio::cleanup();
        crate::devices::start();
        crate::storage::unmount_all();
    }
}

pub async fn run() -> Res<()> {
    load_identity()?;
    tokio::task::spawn_blocking(cleanup).await?;
    let udp = util::env_port("MOVILD_UDP_PORT", 47800);
    let tcp = util::env_port("MOVILD_TCP_PORT", 47801);
    let ans = util::env_port("MOVILD_ANSWER_PORT", 47802);
    write_status();

    let listener = TcpListener::bind(("0.0.0.0", tcp)).await?;
    let answer = TcpListener::bind(("127.0.0.1", ans)).await?;
    tokio::spawn(async move {
        if let Err(e) = discovery(udp).await {
            log(&format!("descubrimiento: {e}"));
        }
    });
    tokio::spawn(async move {
        while let Ok((s, _)) = answer.accept().await {
            tokio::spawn(answer_http(s));
        }
    });
    tokio::spawn(adb_watch(tcp));
    // /state y /set de cámara/micro para la barra (si media.py no está ocupando el puerto)
    match TcpListener::bind(("127.0.0.1", util::env_port("PANTALLA_MEDIA_PORT", 6083))).await {
        Ok(media) => {
            tokio::spawn(async move {
                while let Ok((s, _)) = media.accept().await {
                    tokio::spawn(media_http(s));
                }
            });
        }
        Err(e) => crate::dbg_log!("puerto de cámara/micro ocupado: {e}"),
    }
    let i = ident();
    log(&format!("arranque: {} ({}, huella {}) puertos {udp}/{tcp} [pantalla-host {}]", host_name(), i.id, fingerprint(&i.public), env!("CARGO_PKG_VERSION")));
    eprintln!("pantalla-host: escuchando como «{}» (huella {})", host_name(), fingerprint(&i.public));

    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        #[cfg(unix)]
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        };
        #[cfg(not(unix))]
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = tokio::signal::ctrl_c() => break,
        };
        let (s, _) = accepted?;
        let _ = s.set_nodelay(true);
        tokio::spawn(async move {
            if let Err(e) = handle(s).await {
                crate::dbg_log!("conexión: {e}");
            }
        });
    }
    // al parar: se cortan las sesiones y se limpia
    let kicks: Vec<Arc<Notify>> = st().sessions.values().filter_map(|x| x.kick.clone()).collect();
    if !kicks.is_empty() {
        for k in kicks {
            k.notify_waiters();
        }
        for _ in 0..30 {
            if st().sessions.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    tokio::task::spawn_blocking(cleanup).await?;
    log("parado");
    {
        let mut s = st();
        s.sessions.clear();
        s.pendings.clear();
    }
    write_status();
    Ok(())
}
