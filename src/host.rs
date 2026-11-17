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
static STATE: OnceLock<Mutex<St>> = OnceLock::new();

/// Estado compartido (lo que se escribe en movild.json + lo necesario para /answer, /disconnect…).
#[derive(Default)]
struct St {
    peer: Option<Value>,
    since: Option<f64>,
    pending: Option<Value>,
    pending_tx: Option<oneshot::Sender<bool>>,
    phone: Option<Value>,
    standby: bool,
    ctl: Option<mpsc::UnboundedSender<(u8, Vec<u8>)>>,
    kick: Option<Arc<Notify>>,
    /// estado de cámara/micro (lo mismo que daba media.py en /state)
    mic_on: bool,
    mic_ok: bool,
    cam_ok: bool,
    media_error: String,
    #[cfg(target_os = "linux")]
    mic: Option<crate::audio::Mic>,
}

fn st() -> std::sync::MutexGuard<'static, St> {
    STATE.get_or_init(|| Mutex::new(St::default())).lock().unwrap()
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

fn status_path() -> PathBuf {
    std::env::var_os("MOVILD_STATUS").map(PathBuf::from).unwrap_or_else(|| util::state_dir().join("movild.json"))
}

fn write_status() {
    let s = st();
    let v = json!({
        "connected": s.peer.is_some(), "peer": s.peer, "since": s.since, "pending": s.pending,
        "standby": s.standby, "phone": if s.peer.is_some() { s.phone.clone() } else { None },
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
           "busy": st().peer.is_some(), "host": "pantalla-host", "v": env!("CARGO_PKG_VERSION")})
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
    #[cfg(unix)]
    if quickshell_running() {
        let (tx, rx) = oneshot::channel();
        let mut idb = [0u8; 6];
        getrandom::getrandom(&mut idb).unwrap();
        {
            let mut s = st();
            s.pending = Some(json!({"id": hex(&idb), "device": device, "fp": fp, "via": via,
                                    "expires": unix_time() + TIMEOUT as f64, "timeout": TIMEOUT}));
            s.pending_tx = Some(tx);
        }
        write_status();
        let r = tokio::time::timeout(Duration::from_secs(TIMEOUT), rx).await;
        {
            let mut s = st();
            s.pending = None;
            s.pending_tx = None;
        }
        write_status();
        return matches!(r, Ok(Ok(true)));
    }
    crate::notify::ask(device, fp, via, TIMEOUT).await
}

/// GET /answer?id=…&ok=1|0, /disconnect, /screen?on=, /brightness?v=, /standby?on=  (solo 127.0.0.1)
async fn answer_http(stream: TcpStream) {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    if tokio::time::timeout(Duration::from_secs(3), r.read_line(&mut line)).await.is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let (route, qs) = path.split_once('?').unwrap_or((&path, ""));
    let q: std::collections::HashMap<&str, &str> = qs.split('&').filter_map(|kv| kv.split_once('=')).collect();
    let ctl = st().ctl.clone();
    let send = |v: Value| ctl.as_ref().map(|c| c.send((0, v.to_string().into_bytes())).is_ok()).unwrap_or(false);
    let ok = match route {
        "/disconnect" => st().kick.clone().map(|k| k.notify_waiters()).is_some(),
        "/media" => match q.get("mic") {
            Some(v) => {
                let on = if *v == "toggle" { !st().mic_on } else { matches!(*v, "1" | "on" | "true") };
                set_mic(on).await
            }
            None => false,
        },
        "/screen" => send(json!({"t": "screen", "on": matches!(q.get("on").copied().unwrap_or("1"), "1" | "on")})),
        "/brightness" => send(json!({"t": "brightness", "v": q.get("v").and_then(|v| v.parse::<f64>().ok()).unwrap_or(-1.0)})),
        "/standby" => {
            let cur = st().standby;
            let on = match q.get("on").copied().unwrap_or("toggle") {
                "toggle" => !cur,
                v => matches!(v, "1" | "on"),
            };
            let ok = send(json!({"t": "standby", "on": on}));
            if ok {
                st().standby = on;
                write_status();
            }
            ok
        }
        _ => {
            let mut s = st();
            let matches = s.pending.as_ref().and_then(|p| p.get("id")).and_then(|v| v.as_str()) == q.get("id").copied();
            if matches {
                if let Some(tx) = s.pending_tx.take() {
                    let _ = tx.send(q.get("ok").copied() == Some("1"));
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
    };
    let status = if ok { "200 OK" } else { "404 Not Found" };
    let _ = r.get_mut().write_all(format!("HTTP/1.0 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
}

// ───────────── Cámara / micrófono (compatible con /state y /set de media.py) ─────────────

fn media_state() -> Value {
    let s = st();
    json!({"cam": false, "mic": s.mic_on, "facing": "user", "codec": "h264", "error": s.media_error,
           "camOk": s.cam_ok, "micOk": s.mic_ok, "fps": 0, "using": "", "host": "pantalla-host"})
}

/// Enciende/apaga el micrófono del móvil como fuente de este equipo.
async fn set_mic(on: bool) -> bool {
    let ctl = st().ctl.clone();
    let Some(ctl) = ctl else { return false };
    #[cfg(target_os = "linux")]
    {
        let cur = st().mic.is_some();
        if on && !cur {
            let m = tokio::task::spawn_blocking(crate::audio::Mic::start).await.ok().flatten();
            if m.is_none() {
                st().media_error = "No se pudo crear el micrófono (¿falta pactl?)".into();
            }
            st().mic = m;
        } else if !on && cur {
            let m = st().mic.take();
            let _ = tokio::task::spawn_blocking(move || drop(m)).await;
        }
    }
    {
        let mut s = st();
        s.mic_on = on;
        if !on {
            s.mic_ok = false;
        }
    }
    let _ = ctl.send((0, json!({"t": "mic", "on": on}).to_string().into_bytes()));
    let _ = ctl.send((0, json!({"t": "media", "cam": false, "mic": on, "facing": "user"}).to_string().into_bytes()));
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
    let q: std::collections::HashMap<&str, &str> = qs.split('&').filter_map(|kv| kv.split_once('=')).collect();
    let body = match route {
        "/state" => Some(media_state()),
        "/set" => {
            if let Some(v) = q.get("mic") {
                let on = if *v == "toggle" { !st().mic_on } else { matches!(*v, "1" | "on" | "true") };
                set_mic(on).await;
            }
            if matches!(q.get("cam"), Some(v) if *v != "0" && *v != "off" && *v != "false") {
                st().media_error = "La cámara aún no está en pantalla-host".into();
            }
            Some(media_state())
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
    if st().peer.is_some() {
        let c = encrypt(&noise, &send_json(json!({"t": "rejected", "why": "Ya hay un móvil conectado a este equipo"})))?;
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
    {
        let mut s = st();
        if s.peer.is_some() {
            return Ok(());
        }
        s.peer = Some(json!({"device": device, "fp": fp, "via": via}));
        s.since = Some(unix_time());
    }
    write_status();
    log(&format!("aceptada: {device} por {via}"));

    let result = session(rd, wr, noise, &hello).await;

    {
        let mut s = st();
        s.peer = None;
        s.since = None;
        s.phone = None;
        s.standby = false;
        s.ctl = None;
        s.kick = None;
    }
    write_status();
    if let Err(e) = &result {
        log(&format!("sesión terminada: {e}"));
    } else {
        log("sesión terminada");
    }
    Ok(())
}

async fn session(
    mut rd: tokio::net::tcp::OwnedReadHalf,
    mut wr: tokio::net::tcp::OwnedWriteHalf,
    noise: Arc<Mutex<snow::TransportState>>,
    hello: &Value,
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
        let mut s = st();
        s.ctl = Some(out_tx.clone());
        s.kick = Some(kick.clone());
    }

    let want: Vec<String> = hello.get("want").and_then(|w| w.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let stop = Arc::new(AtomicBool::new(false));

    // Canal 1: escritorio. Primero se crea el monitor, luego va "accepted" y solo después
    // empieza el RFB: la app descarta lo que llegue por el canal 1 antes de "accepted".
    let mut screen_in: Option<std::sync::mpsc::Sender<Vec<u8>>> = None;
    let mut display = None;
    if want.iter().any(|w| w == "screen") {
        let (w, h) = screen_size(hello);
        match tokio::task::spawn_blocking(move || crate::display::open(w, h)).await? {
            Ok(d) => display = Some(d),
            Err(e) => log(&format!("pantalla: {e}")),
        }
    }

    let accepted = json!({"t": "accepted", "name": host_name(), "screen": display.is_some()});
    out_tx.send((0, accepted.to_string().into_bytes()))?;

    if let Some(d) = &display {
        let (in_tx, in_rx) = std::sync::mpsc::channel();
        let (v_tx, mut v_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        crate::rfb::spawn(d.clone(), in_rx, v_tx, stop.clone(), 60.0);
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
        let out = out_tx.clone();
        tokio::task::spawn_blocking(move || crate::audio::Speakers::start(out)).await.ok().flatten()
    } else {
        None
    };
    // Canal 5: almacén del móvil → ~/Móvil (FUSE)
    #[cfg(target_os = "linux")]
    let (store, fuse) = if want.iter().any(|w| w == "storage") {
        let r = crate::storage::Remote::new(out_tx.clone());
        let r2 = r.clone();
        match tokio::task::spawn_blocking(move || crate::storage::mount(r2)).await? {
            Ok(f) => {
                log("almacén: montado en ~/Móvil");
                (Some(r), Some(f))
            }
            Err(e) => {
                log(&format!("almacén: no se pudo montar ({e})"));
                (None, None)
            }
        }
    } else {
        (None, None)
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
                    "ping" => {
                        let _ = out_tx.send((0, br#"{"t": "pong"}"#.to_vec()));
                    }
                    "bye" => break Ok(()),
                    "info" => {
                        let mut m = m.clone();
                        m.as_object_mut().map(|o| o.remove("t"));
                        st().phone = Some(m);
                        write_status();
                    }
                    "standby_off" => {
                        st().standby = false;
                        write_status();
                    }
                    "mic_state" => {
                        log(&format!("micrófono del móvil: {m}"));
                        let mut s = st();
                        s.mic_ok = m["on"].as_bool().unwrap_or(false);
                        s.media_error = m["why"].as_str().unwrap_or("").to_string();
                    }
                    "cam_state" => {
                        let mut s = st();
                        s.cam_ok = m["camOk"].as_bool().unwrap_or(false);
                        s.media_error = m["error"].as_str().unwrap_or("").to_string();
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
                if let Some(mic) = st().mic.as_mut() {
                    mic.write(body);
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
        let mic = st().mic.take();
        let had_store = fuse.is_some();
        let _ = tokio::task::spawn_blocking(move || {
            drop(mic);
            drop(speakers);
            if fuse.is_some() {
                crate::storage::unmount();
                drop(fuse);
            }
        })
        .await;
        drop(store);
        if had_store {
            log("almacén: desmontado");
        }
        let mut s = st();
        s.mic_on = false;
        s.mic_ok = false;
        s.media_error.clear();
    }
    drop(out_tx);
    st().ctl = None;
    writer.abort();
    drop(display); // al soltar el último Arc se quita el monitor virtual
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

async fn adb_watch(port: u16) {
    loop {
        let found = tokio::process::Command::new("adb").arg("get-state").output().await;
        match found {
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(30)).await; // adb no está: se vuelve a mirar de vez en cuando
                continue;
            }
            Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "device" => {
                if let Ok(l) = tokio::process::Command::new("adb").args(["reverse", "--list"]).output().await {
                    if !String::from_utf8_lossy(&l.stdout).contains(&format!("tcp:{port}")) {
                        let r = tokio::process::Command::new("adb")
                            .args(["reverse", &format!("tcp:{port}"), &format!("tcp:{port}")])
                            .output()
                            .await;
                        if matches!(&r, Ok(o) if o.status.success()) {
                            log("cable: adb reverse activo");
                        }
                    }
                }
            }
            Ok(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

// ───────────── Arranque ─────────────

/// Deja el sistema limpio: sin monitor del móvil ni salidas de sonido.
fn cleanup() {
    #[cfg(target_os = "linux")]
    {
        crate::linux::ensure_env();
        if crate::linux::is_hyprland() {
            crate::linux::remove_monitor();
        }
        crate::audio::cleanup();
        crate::storage::unmount();
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
    // al parar: se corta la sesión y se limpia
    let kick = st().kick.clone();
    if let Some(k) = kick {
        k.notify_waiters();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::task::spawn_blocking(cleanup).await?;
    log("parado");
    {
        let mut s = st();
        s.peer = None;
        s.pending = None;
    }
    write_status();
    Ok(())
}
