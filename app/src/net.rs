//! Red: descubrir equipos (wifi y cable) y abrir la sesión cifrada con el que se elija.
//!
//! Mismo protocolo que movild.py:
//!   UDP 47800  "MOVIL?" → JSON con nombre/id/clave del equipo
//!   TCP 47801  b"I" → JSON de información (para el equipo del cable, vía adb reverse a 127.0.0.1)
//!              b"N" → Noise_XX_25519_ChaChaPoly_BLAKE2s, mensajes con longitud u16 BE;
//!                     cifrados: [canal u8][datos], canal 0 = control JSON.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blake2::digest::{Update, VariableOutput};
use serde_json::{json, Value};

const UDP_PORT: u16 = 47800;
const TCP_PORT: u16 = 47801;
const PROTO: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

#[derive(Clone, Debug)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub os: String,
    /// "linux" | "windows" (pantalla-host); vacío con movild antiguo
    pub platform: String,
    pub key: Vec<u8>,
    pub addr: SocketAddr,
    pub cable: bool,
    pub busy: bool,
    pub seen: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Connecting(String),
    Waiting(String),
    Connected { name: String, cable: bool },
    Failed(String),
}

pub struct Shared {
    pub hosts: HashMap<String, Host>,
    pub status: Status,
    /// sesión activa (para mandar toques) y su vídeo
    pub link: Option<Arc<crate::link::Link>>,
    pub video: Option<Arc<crate::video::Video>>,
    /// pantalla "apagada" a petición del equipo (negro + brillo mínimo)
    pub dark: bool,
    /// stand-by (lo enciende el equipo) y sus datos
    pub standby: bool,
    pub sb: Value,
    /// la interfaz pide cortar la sesión actual
    pub disconnect: bool,
}

pub type State = Arc<Mutex<Shared>>;

/// Huella corta de una clave (igual que en movild.py): blake2s de 4 bytes → "ABCD-EF01"
pub fn fingerprint(key: &[u8]) -> String {
    hash_hex(key, 4).to_uppercase().chars().enumerate()
        .flat_map(|(i, c)| if i == 4 { vec!['-', c] } else { vec![c] }).collect()
}

fn hash_hex(data: &[u8], n: usize) -> String {
    let mut h = blake2::Blake2sVar::new(n).unwrap();
    h.update(data);
    let mut out = vec![0u8; n];
    h.finalize_variable(&mut out).unwrap();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// Identidad del móvil: clave X25519 guardada en los datos privados de la app
pub struct Identity {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
    dir: PathBuf,
}

impl Identity {
    pub fn load(dir: PathBuf) -> Identity {
        let path = dir.join("identidad.key");
        let builder = snow::Builder::new(PROTO.parse().unwrap());
        let private = match std::fs::read(&path) {
            Ok(k) if k.len() == 32 => k,
            _ => {
                let kp = builder.generate_keypair().unwrap();
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(&path, &kp.private);
                kp.private
            }
        };
        // la pública se obtiene con un apretón de manos local de prueba
        let public = public_from_private(&private);
        Identity { private, public, dir }
    }

    /// Clave conocida de un equipo (se recuerda la primera vez). Devuelve false si ha cambiado.
    fn check_pin(&self, host_id: &str, key: &[u8]) -> bool {
        let path = self.dir.join("equipos.json");
        let mut known: HashMap<String, String> =
            std::fs::read(&path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        match known.get(host_id) {
            Some(k) => *k == hex,
            None => {
                known.insert(host_id.to_string(), hex);
                let _ = std::fs::write(&path, serde_json::to_vec(&known).unwrap());
                true
            }
        }
    }
}

fn public_from_private(private: &[u8]) -> Vec<u8> {
    // X25519: pública = escalar * punto base. Se hace un handshake NN... más simple: snow no expone
    // la derivación, así que se usa un handshake XX en memoria y se lee la estática remota.
    let p: snow::params::NoiseParams = PROTO.parse().unwrap();
    let other = snow::Builder::new(p.clone()).generate_keypair().unwrap();
    let mut i = snow::Builder::new(p.clone()).local_private_key(private).unwrap().build_initiator().unwrap();
    let mut r = snow::Builder::new(p).local_private_key(&other.private).unwrap().build_responder().unwrap();
    let (mut a, mut b) = ([0u8; 1024], [0u8; 1024]);
    let n = i.write_message(&[], &mut a).unwrap(); r.read_message(&a[..n], &mut b).unwrap();
    let n = r.write_message(&[], &mut a).unwrap(); i.read_message(&a[..n], &mut b).unwrap();
    let n = i.write_message(&[], &mut a).unwrap(); r.read_message(&a[..n], &mut b).unwrap();
    r.get_remote_static().unwrap().to_vec()
}

fn parse_info(v: &Value, addr: SocketAddr, cable: bool) -> Option<Host> {
    Some(Host {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        os: v["os"].as_str().unwrap_or("").to_string(),
        platform: v["platform"].as_str().unwrap_or("").to_string(),
        key: hex_decode(v["key"].as_str()?),
        addr,
        cable,
        busy: v["busy"].as_bool().unwrap_or(false),
        seen: Instant::now(),
    })
}

/// Hilo de descubrimiento: cada 2 s pregunta por wifi (difusión UDP) y por el cable (127.0.0.1).
pub fn discovery(state: State) {
    std::thread::spawn(move || {
        let udp = UdpSocket::bind("0.0.0.0:0").ok();
        if let Some(u) = &udp {
            let _ = u.set_broadcast(true);
            let _ = u.set_read_timeout(Some(Duration::from_millis(250)));
        }
        loop {
            let mut found: Vec<Host> = Vec::new();
            // Cable: adb reverse lleva 127.0.0.1:47801 al equipo enchufado
            let local: SocketAddr = ([127, 0, 0, 1], TCP_PORT).into();
            if let Ok(mut s) = TcpStream::connect_timeout(&local, Duration::from_millis(400)) {
                let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
                let mut buf = Vec::new();
                if s.write_all(b"I").is_ok() && s.read_to_end(&mut buf).is_ok() {
                    if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                        found.extend(parse_info(&v, local, true));
                    }
                }
            }
            // Wifi: difusión
            if let Some(u) = &udp {
                let _ = u.send_to(b"MOVIL?", ("255.255.255.255", UDP_PORT));
                let t0 = Instant::now();
                let mut buf = [0u8; 2048];
                while t0.elapsed() < Duration::from_millis(700) {
                    if let Ok((n, from)) = u.recv_from(&mut buf) {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) {
                            let addr = SocketAddr::new(from.ip(), TCP_PORT);
                            found.extend(parse_info(&v, addr, false));
                        }
                    }
                }
            }
            {
                let mut st = state.lock().unwrap();
                for h in found {
                    // si un equipo aparece por cable y por wifi, gana el cable
                    let keep_cable = st.hosts.get(&h.id).map(|o| o.cable && !h.cable
                        && o.seen.elapsed() < Duration::from_secs(5)).unwrap_or(false);
                    if !keep_cable { st.hosts.insert(h.id.clone(), h); }
                }
                st.hosts.retain(|_, h| h.seen.elapsed() < Duration::from_secs(7));
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
    });
}

/// Pregunta a una dirección concreta (TCP "I") — para la conexión automática de depuración
pub fn probe(addr: &str) -> Option<Host> {
    let a: SocketAddr = addr.parse().ok()?;
    let mut s = TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
    s.write_all(b"I").ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    parse_info(&serde_json::from_slice(&buf).ok()?, a, true)
}

// ───────────── Sesión ─────────────
fn read_frame(s: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len)?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_frame(s: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(data.len() + 2);
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    s.write_all(&out)
}

/// Conecta con un equipo en otro hilo. El estado se va actualizando en `state.status`.
pub fn connect(state: State, ident: Arc<Identity>, host: Host, device: String) {
    std::thread::spawn(move || {
        let set = |s: Status| state.lock().unwrap().status = s;
        state.lock().unwrap().disconnect = false;
        set(Status::Connecting(host.name.clone()));
        match session(&state, &ident, &host, &device) {
            Ok(()) => set(Status::Idle),
            Err(e) => set(Status::Failed(e)),
        }
    });
}

fn e(m: &'static str) -> impl Fn(std::io::Error) -> String {
    move |err| format!("{m}: {err}")
}

fn session(state: &State, ident: &Identity, host: &Host, device: &str) -> Result<(), String> {
    let mut s = TcpStream::connect_timeout(&host.addr, Duration::from_secs(4)).map_err(e("No se pudo conectar"))?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    s.write_all(b"N").map_err(e("Conexión"))?;

    let mut hs = snow::Builder::new(PROTO.parse().unwrap())
        .local_private_key(&ident.private).map_err(|x| x.to_string())?
        .build_initiator().map_err(|x| x.to_string())?;
    let (mut a, mut b) = (vec![0u8; 65535], vec![0u8; 65535]);
    let n = hs.write_message(&[], &mut a).map_err(|x| x.to_string())?;           // → e
    write_frame(&mut s, &a[..n]).map_err(e("Saludo"))?;
    let msg = read_frame(&mut s).map_err(e("Saludo"))?;                            // ← e, ee, s, es
    hs.read_message(&msg, &mut b).map_err(|_| "Saludo cifrado inválido".to_string())?;
    let remote = hs.get_remote_static().ok_or("Sin clave del equipo")?.to_vec();
    if remote != host.key || !ident.check_pin(&host.id, &remote) {
        return Err(format!("¡La identidad de {} no coincide con la que se recordaba! Conexión cortada.", host.name));
    }
    let n = hs.write_message(&[], &mut a).map_err(|x| x.to_string())?;           // → s, se
    write_frame(&mut s, &a[..n]).map_err(e("Saludo"))?;
    let mut t = hs.into_transport_mode().map_err(|x| x.to_string())?;

    let mut send = |t: &mut snow::TransportState, s: &mut TcpStream, v: Value| -> Result<(), String> {
        let mut plain = vec![0u8];
        plain.extend(serde_json::to_vec(&v).unwrap());
        let n = t.write_message(&plain, &mut a).map_err(|x| x.to_string())?;
        write_frame(s, &a[..n]).map_err(|x| x.to_string())
    };
    let recv = |t: &mut snow::TransportState, s: &mut TcpStream, b: &mut Vec<u8>| -> Result<Value, String> {
        let msg = read_frame(s).map_err(|x| x.to_string())?;
        let n = t.read_message(&msg, b).map_err(|_| "Mensaje cifrado inválido".to_string())?;
        if n == 0 || b[0] != 0 { return Ok(Value::Null); }
        serde_json::from_slice(&b[1..n]).map_err(|x| x.to_string())
    };

    send(&mut t, &mut s, json!({"t": "hello", "device": device, "want": ["screen", "audio", "storage"]}))?;
    s.set_read_timeout(Some(Duration::from_secs(45))).ok();     // tiempo para pulsar Aceptar
    let screen;
    loop {
        let v = recv(&mut t, &mut s, &mut b)?;
        match v["t"].as_str() {
            Some("waiting") => state.lock().unwrap().status = Status::Waiting(host.name.clone()),
            Some("accepted") => { screen = v["screen"].as_bool().unwrap_or(false); break }
            Some("rejected") => return Err(v["why"].as_str().unwrap_or("Rechazado").to_string()),
            _ => {}
        }
    }
    // ── Sesión: varios canales sobre la misma conexión cifrada ──
    use crate::link::{Link, CH_CONTROL, CH_SCREEN};
    let link = Arc::new(Link::new(t, s.try_clone().map_err(|x| x.to_string())?));
    let video = Arc::new(crate::video::Video::new());
    let (rfb_tx, rfb_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let mut st = state.lock().unwrap();
        st.link = Some(link.clone());
        st.video = Some(video.clone());
        st.status = Status::Connected { name: host.name.clone(), cable: host.cable };
    }
    if screen {
        let (l, v) = (link.clone(), video.clone());
        std::thread::spawn(move || {
            if let Err(e) = crate::rfb::run(l.clone(), rfb_rx, v) {
                if !l.is_closed() { log::error!("escritorio: {e}"); }
            }
        });
    }
    // registro de la app → ordenador (cada segundo, lo que haya pendiente)
    {
        let l = link.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            if l.is_closed() { break; }
            let lines = crate::applog::take_pending();
            if !lines.is_empty() && l.send_json(json!({"t": "log", "lines": lines})).is_err() { break; }
        });
    }
    // estado del móvil → equipo cada 10 s (para su Quickshell, también por wifi)
    {
        let l = link.clone();
        std::thread::spawn(move || loop {
            if l.is_closed() { break; }
            #[cfg(target_os = "android")]
            {
                let mut v: Value = serde_json::from_str(&crate::android::info()).unwrap_or(json!({}));
                v["t"] = json!("info");
                v["device"] = json!(device_name());
                v["cpu_temp"] = json!(cpu_temp());
                if l.send_json(v).is_err() { break; }
            }
            std::thread::sleep(Duration::from_secs(10));
        });
    }
    // pings cada 10 s (el equipo corta si no recibe nada en 30 s)
    {
        let l = link.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(10));
            if l.is_closed() || l.send_json(json!({"t": "ping"})).is_err() { break; }
        });
    }
    s.set_read_timeout(Some(Duration::from_secs(35))).ok();
    let speaker = crate::audio::Speaker::start();
    // almacén de 10 GB de este equipo (canal 5)
    let store_tx = crate::applog::EXT_DIR.get()
        .map(|d| crate::storage::serve(link.clone(), d.clone(), host.id.clone()));
    let mic: Arc<Mutex<Option<crate::audio::Mic>>> = Arc::new(Mutex::new(None));
    let cam: Arc<Mutex<Option<crate::camera::Camera>>> = Arc::new(Mutex::new(None));
    let result = loop {
        if state.lock().unwrap().disconnect { link.close(); break Ok(()); }
        match link.recv(&mut s) {
            Ok((CH_CONTROL, d)) => {
                let v: Value = serde_json::from_slice(&d).unwrap_or(Value::Null);
                if v["t"] == "mic" { set_mic(&mic, &link, v["on"].as_bool().unwrap_or(false)); }
                if v["t"] == "brightness" {
                    #[cfg(target_os = "android")]
                    crate::android::set_brightness(v["v"].as_f64().unwrap_or(-1.0) as f32);
                }
                if v["t"] == "standby" {
                    state.lock().unwrap().standby = v["on"].as_bool().unwrap_or(false);
                    log::info!("stand-by: {}", v["on"]);
                }
                if v["t"] == "sb" { state.lock().unwrap().sb = v.clone(); }
                if v["t"] == "screen" {
                    let dark = !v["on"].as_bool().unwrap_or(true);
                    state.lock().unwrap().dark = dark;
                    #[cfg(target_os = "android")]
                    crate::android::set_dim(dark);
                    log::info!("pantalla: {}", if dark { "apagada" } else { "encendida" });
                }
                if v["t"] == "media" {
                    // estado de media.py (cámara/micro pedidos desde Quickshell) reenviado por movild
                    set_mic(&mic, &link, v["mic"].as_bool().unwrap_or(false));
                    set_cam(&cam, &link, v["cam"].as_bool().unwrap_or(false), v["facing"].as_str().unwrap_or("user"));
                }
            }
            Ok((CH_SCREEN, d)) => { let _ = rfb_tx.send(d); }
            Ok((crate::audio::CH_AUDIO, d)) => { if let Some(sp) = &speaker { sp.push(&d); } }
            Ok((crate::storage::CH_STORE, d)) => { if let Some(tx) = &store_tx { let _ = tx.send(d); } }
            Ok(_) => {}
            Err(_) if link.is_closed() || state.lock().unwrap().disconnect => break Ok(()),
            Err(_) => break Err("Se perdió la conexión con el equipo".to_string()),
        }
    };
    link.close();
    video.stop();
    *mic.lock().unwrap() = None;
    *cam.lock().unwrap() = None;
    {
        let mut st = state.lock().unwrap();
        st.link = None;
        st.video = None;
        st.standby = false;
        if st.dark {
            st.dark = false;
            #[cfg(target_os = "android")]
            crate::android::set_dim(false);
        }
    }
    result
}

/// Enciende/apaga el micrófono (pide el permiso de Android la primera vez y espera a que se conceda)
fn set_mic(mic: &Arc<Mutex<Option<crate::audio::Mic>>>, link: &Arc<crate::link::Link>, on: bool) {
    if !on {
        *mic.lock().unwrap() = None;
        let _ = link.send_json(json!({"t": "mic_state", "on": false}));
        return;
    }
    if mic.lock().unwrap().is_some() { return; }
    let (mic, link) = (mic.clone(), link.clone());
    std::thread::spawn(move || {
        if !wait_permission(&link) {
            let _ = link.send_json(json!({"t": "mic_state", "on": false, "why": "sin permiso"}));
            return;
        }
        let m = crate::audio::Mic::start(link.clone());
        let ok = m.is_some();
        *mic.lock().unwrap() = m;
        let _ = link.send_json(json!({"t": "mic_state", "on": ok}));
    });
}

/// Espera al permiso de cámara/micrófono (lo pide la primera vez). false si no se concede en 40 s.
fn wait_permission(link: &Arc<crate::link::Link>) -> bool {
    #[cfg(target_os = "android")]
    if !crate::android::has_media_permissions() {
        log::info!("pidiendo permiso de cámara y micrófono");
        crate::android::ask_media_permissions();
        let t0 = Instant::now();
        while !crate::android::has_media_permissions() {
            if t0.elapsed() > Duration::from_secs(40) || link.is_closed() { return false; }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    let _ = link;
    true
}

/// Enciende/apaga la cámara o cambia de delantera/trasera
fn set_cam(cam: &Arc<Mutex<Option<crate::camera::Camera>>>, link: &Arc<crate::link::Link>, on: bool, facing: &str) {
    {
        let mut c = cam.lock().unwrap();
        if !on {
            if c.take().is_some() { let _ = link.send_json(json!({"t": "cam_state", "camOk": false})); }
            return;
        }
        if c.as_ref().map(|x| x.facing == facing).unwrap_or(false) { return; }
        *c = None;                                  // cambio de cámara: se cierra la anterior
    }
    let (cam, link, facing) = (cam.clone(), link.clone(), facing.to_string());
    std::thread::spawn(move || {
        if !wait_permission(&link) {
            let _ = link.send_json(json!({"t": "cam_state", "camOk": false, "error": "Sin permiso de cámara en el móvil"}));
            return;
        }
        let c = crate::camera::Camera::start(link.clone(), &facing);
        let ok = c.is_some();
        *cam.lock().unwrap() = c;
        let _ = link.send_json(json!({"t": "cam_state", "camOk": ok}));
    });
}

/// Temperatura más alta de la CPU (°C), si el sistema deja leerla
fn cpu_temp() -> Option<f32> {
    let mut best: Option<f32> = None;
    for e in std::fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        let ty = std::fs::read_to_string(e.path().join("type")).unwrap_or_default();
        if ty.trim().starts_with("cpu") && ty.contains("usr") {
            if let Some(t) = std::fs::read_to_string(e.path().join("temp")).ok().and_then(|x| x.trim().parse::<f32>().ok()) {
                let c = if t > 1000.0 { t / 1000.0 } else { t };
                best = Some(best.map_or(c, |b: f32| b.max(c)));
            }
        }
    }
    best
}

/// Nombre del móvil (ro.product.model), p. ej. "Mi MIX 2S"
pub fn device_name() -> String {
    #[cfg(target_os = "android")]
    unsafe {
        let mut buf = [0 as libc::c_char; 92];
        let n = libc::__system_property_get(c"ro.product.model".as_ptr(), buf.as_mut_ptr());
        if n > 0 {
            return std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        }
    }
    "Móvil".to_string()
}
