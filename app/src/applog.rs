//! Registro de la app (nada en logcat, que es texto plano): cada mensaje va a un archivo en el móvil
//! (/sdcard/Android/data/org.movilpantalla.app/files/app.log) y, mientras hay sesión, al ordenador
//! (movild lo guarda en ~/.local/state/movil-pantalla/app-movil.log). Así se puede depurar sin cable.
//!
//! En disco todo va CIFRADO (mismo formato que host/seclog.py): cada línea se cifra para una clave
//! pública cuya privada solo se obtiene con la contraseña de los registros. Leer con `movil-logs` en el PC.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct AppLog {
    file: Mutex<Option<std::fs::File>>,
}

/// Carpeta externa de la app (registro, almacén de los equipos)
pub static EXT_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Líneas pendientes de mandar al ordenador (máx. 2000)
static PENDING: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

fn stamp() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = t.as_secs() + 3600;                 // hora de España (invierno, UTC+1)
    let (days, rem) = (s / 86400, s % 86400);
    // días desde 1970 → fecha civil (algoritmo de H. Hinnant)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", rem / 3600, (rem / 60) % 60, rem % 60, t.subsec_millis())
}

/// Clave pública de los registros (la privada se deriva de la contraseña; no está en el móvil)
const LOG_PUB: [u8; 32] = [
    0xbb, 0x84, 0xec, 0xcf, 0x9b, 0xaa, 0x2e, 0xd7, 0x61, 0xe3, 0x81, 0xd6, 0x3f, 0x87, 0xa5, 0xc1,
    0x74, 0xe3, 0xeb, 0x63, 0xab, 0xda, 0x6d, 0x9d, 0xd7, 0xf5, 0xb3, 0x01, 0x7c, 0xbc, 0x8f, 0x65,
];

/// MPL1:base64(efímera_pública | nonce | ChaCha20-Poly1305(texto)), clave = BLAKE2b-256(dh | efímera | destinataria)
fn encrypt_line(text: &str) -> String {
    use base64::Engine;
    use blake2::Digest;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use rand_core::RngCore;
    let eph = x25519_dalek::EphemeralSecret::random_from_rng(rand_core::OsRng);
    let eph_pub = x25519_dalek::PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&x25519_dalek::PublicKey::from(LOG_PUB));
    let mut h = blake2::Blake2b::<blake2::digest::consts::U32>::new();
    h.update(shared.as_bytes());
    h.update(eph_pub.as_bytes());
    h.update(LOG_PUB);
    let key = h.finalize();
    let mut nonce = [0u8; 12];
    rand_core::OsRng.fill_bytes(&mut nonce);
    let ct = chacha20poly1305::ChaCha20Poly1305::new(&key)
        .encrypt(chacha20poly1305::Nonce::from_slice(&nonce), text.as_bytes())
        .unwrap_or_default();
    let mut raw = Vec::with_capacity(44 + ct.len());
    raw.extend_from_slice(eph_pub.as_bytes());
    raw.extend_from_slice(&nonce);
    raw.extend_from_slice(&ct);
    format!("MPL1:{}", base64::engine::general_purpose::STANDARD.encode(raw))
}

impl log::Log for AppLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || m.target().starts_with("movil")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) { return; }
        let target = r.target().strip_prefix("movil::").unwrap_or(r.target());
        let line = format!("{} {:5} [{}] {}", stamp(), r.level(), target, r.args());
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{}", encrypt_line(&line));
        }
        let mut p = PENDING.lock().unwrap();
        if p.len() >= 2000 { p.pop_front(); }
        p.push_back(line);
    }

    fn flush(&self) {}
}

pub fn init(dir: Option<PathBuf>) {
    if let Some(d) = &dir { let _ = EXT_DIR.set(d.clone()); }
    let file = dir.and_then(|d| {
        let _ = std::fs::create_dir_all(&d);
        let path = d.join("app.log");
        // si crece mucho, se empieza de nuevo
        if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
            let _ = std::fs::rename(&path, d.join("app.log.1"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let logger = Box::leak(Box::new(AppLog { file: Mutex::new(file) }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    std::panic::set_hook(Box::new(|info| {
        log::error!("PÁNICO: {info}");
    }));
}

/// Saca las líneas pendientes (para mandarlas al ordenador)
pub fn take_pending() -> Vec<String> {
    PENDING.lock().unwrap().drain(..).collect()
}
