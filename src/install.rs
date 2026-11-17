//! `pantalla-host instalar` / `pantalla-host desinstalar`: el programa se instala a sí mismo,
//! sin permisos de administrador.
//!   Linux:   ~/.local/bin/pantalla-host + servicio de usuario de systemd (arranca con la sesión)
//!   Windows: %LOCALAPPDATA%\Pantalla\pantalla-host.exe + inicio de sesión (HKCU\…\Run)

use std::path::PathBuf;
use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

#[cfg(target_os = "linux")]
pub fn install(replace_movild: bool) -> Result<(), String> {
    let movild_active = run("systemctl", &["--user", "is-active", "-q", "movild"]) || run("systemctl", &["--user", "is-enabled", "-q", "movild"]);
    if movild_active && !replace_movild {
        return Err("movild (la versión en Python) está activo y usa los mismos puertos.\n\
                    Para sustituirlo: pantalla-host instalar --sustituir-movild   (se puede volver atrás con: pantalla-host desinstalar)"
            .into());
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin_dir = home().join(".local/bin");
    let dest = bin_dir.join("pantalla-host");
    std::fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
    if me != dest {
        // se copia a un temporal y se renombra (vale aunque el servicio esté usando el binario viejo)
        let tmp = bin_dir.join(".pantalla-host.nuevo");
        std::fs::copy(&me, &tmp).map_err(|e| format!("copiar: {e}"))?;
        std::fs::rename(&tmp, &dest).map_err(|e| format!("copiar: {e}"))?;
    }
    // KDE Plasma: KWin solo deja crear monitores virtuales y mover el ratón a programas que lo
    // declaran en su .desktop (se busca por la ruta del ejecutable)
    let apps = home().join(".local/share/applications");
    std::fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
    std::fs::write(
        apps.join("pantalla-host.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Escritorio (servicio del móvil)\nExec={}\nNoDisplay=true\n\
             X-KDE-Wayland-Interfaces={}\n",
            dest.display(),
            crate::kde::INTERFACES
        ),
    )
    .map_err(|e| e.to_string())?;
    run("kbuildsycoca6", &["--noincremental"]);
    let unit_dir = home().join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir).map_err(|e| e.to_string())?;
    std::fs::write(
        unit_dir.join("pantalla-host.service"),
        "[Unit]\n\
         Description=pantalla-host: servicio para la app \"Pantalla\" del móvil\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         ExecStart=%h/.local/bin/pantalla-host\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
    .map_err(|e| e.to_string())?;
    if movild_active {
        run("systemctl", &["--user", "disable", "--now", "movild"]);
        println!("movild parado y desactivado (sus archivos siguen ahí).");
    }
    run("systemctl", &["--user", "daemon-reload"]);
    if !run("systemctl", &["--user", "enable", "pantalla-host"]) || !run("systemctl", &["--user", "restart", "pantalla-host"]) {
        return Err("no se pudo activar el servicio de usuario".into());
    }
    println!("Instalado: {} (servicio de usuario pantalla-host, arranca con la sesión).", dest.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<(), String> {
    run("systemctl", &["--user", "disable", "--now", "pantalla-host"]);
    let _ = std::fs::remove_file(home().join(".config/systemd/user/pantalla-host.service"));
    run("systemctl", &["--user", "daemon-reload"]);
    let _ = std::fs::remove_file(home().join(".local/bin/pantalla-host"));
    let _ = std::fs::remove_file(home().join(".local/share/applications/pantalla-host.desktop"));
    // si estaba la versión en Python, se vuelve a ella
    if home().join(".config/systemd/user/movild.service").exists() {
        run("systemctl", &["--user", "enable", "--now", "movild"]);
        println!("movild (versión en Python) reactivado.");
    }
    println!("Desinstalado.");
    Ok(())
}

#[cfg(windows)]
fn dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("Pantalla")
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
pub fn install(_replace: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let d = dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let dest = d.join("pantalla-host.exe");
    // si ya está en marcha, se para para poder sustituirlo
    run("taskkill", &["/F", "/IM", "pantalla-host.exe", "/FI", &format!("PID ne {}", std::process::id())]);
    if me != dest {
        std::fs::copy(&me, &dest).map_err(|e| format!("copiar: {e}"))?;
    }
    let value = format!("\"{}\"", dest.display());
    if !run("reg", &["add", RUN_KEY, "/v", "Pantalla", "/t", "REG_SZ", "/d", &value, "/f"]) {
        return Err("no se pudo añadir al inicio de sesión".into());
    }
    // arranca ya, sin consola
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(&dest).creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW).spawn().map_err(|e| e.to_string())?;
    println!("Instalado en {} y añadido al inicio de sesión.", dest.display());
    println!("La primera vez Windows puede preguntar por el cortafuegos: permite las redes privadas.");
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> Result<(), String> {
    run("reg", &["delete", RUN_KEY, "/v", "Pantalla", "/f"]);
    run("taskkill", &["/F", "/IM", "pantalla-host.exe", "/FI", &format!("PID ne {}", std::process::id())]);
    let _ = std::fs::remove_file(dir().join("pantalla-host.exe"));
    println!("Desinstalado.");
    Ok(())
}
