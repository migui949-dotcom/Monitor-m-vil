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
