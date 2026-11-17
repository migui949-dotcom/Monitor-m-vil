use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_time() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// "AAAA-MM-DD hh:mm:ss" en hora local.
pub fn now_str() -> String {
    #[cfg(unix)]
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
        )
    }
    #[cfg(not(unix))]
    {
        let s = unix_time() as u64;
        format!("{} (UTC s)", s)
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Carpeta de estado (clave, registros, estado para la barra).
/// Linux: ~/.local/state/movil-pantalla (la misma que usaba movild: misma identidad).
/// Windows: %APPDATA%\Pantalla
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PANTALLA_STATE") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        if let Some(a) = std::env::var_os("APPDATA") {
            return PathBuf::from(a).join("Pantalla");
        }
    }
    home().join(".local/state/movil-pantalla")
}

pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        return state_dir();
    }
    #[allow(unreachable_code)]
    home().join(".config/movil-pantalla")
}

/// Guarda `CLAVE=valor` en host.conf (cambia la línea si ya estaba)
pub fn set_conf(key: &str, val: &str) -> std::io::Result<()> {
    let path = config_dir().join("host.conf");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: Vec<String> = text.lines().filter(|l| !l.trim().starts_with(&format!("{key}="))).map(String::from).collect();
    out.push(format!("{key}={val}"));
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(path, out.join("\n") + "\n")
}

/// Lee `CLAVE=valor` de host.conf (formato shell sencillo).
pub fn conf(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_dir().join("host.conf")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

pub fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn debug() -> bool {
    std::env::var_os("PANTALLA_DEBUG").is_some()
}

#[macro_export]
macro_rules! dbg_log {
    ($($a:tt)*) => { if $crate::util::debug() { eprintln!("[pantalla] {}", format!($($a)*)); } };
}

// ───────────── Varios móviles a la vez: una "ranura" (1..=MAX_PHONES) por móvil conectado ─────────────
// La ranura 1 conserva los nombres de siempre (monitor MOVIL, ~/Móvil, /dev/video42…), así lo que ya
// estaba configurado para un móvil sigue valiendo; las demás llevan el número (MOVIL-2…).

pub const MAX_PHONES: u8 = 4;

/// Nombre de la salida (monitor virtual) de la ranura `s`
pub fn output_name(s: u8) -> String {
    if s <= 1 { "MOVIL".into() } else { format!("MOVIL-{s}") }
}

/// ¿Es el monitor de algún móvil? (MOVIL, MOVIL-2…; en KWin "Virtual-MOVIL…")
pub fn is_phone_output(name: &str) -> bool {
    let n = name.strip_prefix("Virtual-").unwrap_or(name);
    n == "MOVIL" || n.strip_prefix("MOVIL-").map(|r| r.parse::<u8>().is_ok()).unwrap_or(false)
}

/// Clave de host.conf con la posición del monitor de la ranura `s` (POSITION, POSITION_2…)
pub fn position_key(s: u8) -> String {
    if s <= 1 { "POSITION".into() } else { format!("POSITION_{s}") }
}

/// Posición del monitor de la ranura `s`. Por defecto el 2º móvil va al lado contrario del 1º
/// (con el 1º a la izquierda, el 2º a la derecha) y los demás junto al 1º.
pub fn position(s: u8) -> String {
    if let Some(p) = conf(&position_key(s)) {
        return p;
    }
    let first = conf("POSITION").unwrap_or_else(|| "auto-right".into());
    if s == 2 {
        return match first.as_str() {
            "auto-left" => "auto-right",
            "auto-up" => "auto-down",
            "auto-down" => "auto-up",
            _ => "auto-left",
        }
        .into();
    }
    first
}

/// Escritorio (workspace) propio del monitor de la ranura `s`: PHONE_WS (11 por defecto) + 10 por ranura
pub fn phone_workspace(s: u8) -> i64 {
    let base = conf("PHONE_WS").and_then(|v| v.parse::<i64>().ok()).unwrap_or(11);
    base + (s.max(1) as i64 - 1) * 10
}
