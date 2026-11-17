//! Registro cifrado, compatible con `seclog.py` (se lee con `movil-logs`).
//!
//! Cada línea: `MPL1:<base64( efímera_pública(32) | nonce(12) | ChaCha20-Poly1305(texto) )>`
//! clave = BLAKE2b-256( X25519(efímera, destinataria) | efímera_pública | destinataria_pública )

use base64::Engine;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::io::Write;
use std::path::Path;
use x25519_dalek::{PublicKey, StaticSecret};

const LOG_PUB: [u8; 32] = [
    0xbb, 0x84, 0xec, 0xcf, 0x9b, 0xaa, 0x2e, 0xd7, 0x61, 0xe3, 0x81, 0xd6, 0x3f, 0x87, 0xa5, 0xc1,
    0x74, 0xe3, 0xeb, 0x63, 0xab, 0xda, 0x6d, 0x9d, 0xd7, 0xf5, 0xb3, 0x01, 0x7c, 0xbc, 0x8f, 0x65,
];
const MAX_BYTES: u64 = 4_000_000;

fn encrypt_line(text: &str) -> String {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).expect("getrandom");
    let eph = StaticSecret::from(seed);
    let eph_pub = PublicKey::from(&eph);
    let recip = PublicKey::from(LOG_PUB);
    let shared = eph.diffie_hellman(&recip);
    let mut h = Blake2b::<U32>::new();
    h.update(shared.as_bytes());
    h.update(eph_pub.as_bytes());
    h.update(LOG_PUB);
    let key = h.finalize();
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).expect("getrandom");
    let ct = ChaCha20Poly1305::new(Key::from_slice(&key))
        .encrypt(Nonce::from_slice(&nonce), text.as_bytes())
        .expect("cifrado");
    let mut raw = Vec::with_capacity(44 + ct.len());
    raw.extend_from_slice(eph_pub.as_bytes());
    raw.extend_from_slice(&nonce);
    raw.extend_from_slice(&ct);
    format!("MPL1:{}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// Añade líneas cifradas a un archivo (rota a ~4 MB).
pub fn append(path: &Path, lines: &[String], stamp: bool) {
    if let Ok(m) = std::fs::metadata(path) {
        if m.len() > MAX_BYTES {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
    }
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    for l in lines {
        let text = if stamp { format!("{} {}", crate::util::now_str(), l) } else { l.clone() };
        let text: String = text.chars().take(4000).collect();
        let _ = writeln!(f, "{}", encrypt_line(&text));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}
