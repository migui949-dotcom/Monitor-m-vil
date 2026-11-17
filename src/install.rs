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
    write_launcher(&dest)?;
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
    crate::sistema::ask_and_prepare(&dest);
    println!("Instalado: {} (servicio de usuario pantalla-host, arranca con la sesión).", dest.display());
    println!("Panel: \"Escritorio\" en el menú de aplicaciones (se abre solo cuando un móvil pide conectarse).");
    // se abre el panel ahora, si hay sesión gráfica
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
        let _ = Command::new(&dest).arg("panel").stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    }
    Ok(())
}

/// Lanzador "Escritorio" (menú en fuzzel/rofi/wofi; acción "Panel") + icono. El mismo .desktop da a
/// KWin el permiso del monitor virtual (lo busca por la ruta del ejecutable, la primera palabra de Exec).
#[cfg(target_os = "linux")]
fn write_launcher(dest: &std::path::Path) -> Result<(), String> {
    let apps = home().join(".local/share/applications");
    std::fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
    let icons = home().join(".local/share/icons/hicolor/scalable/apps");
    let _ = std::fs::create_dir_all(&icons);
    let _ = std::fs::write(icons.join("escritorio-movil.svg"), include_str!("../app/icono.svg"));
    std::fs::write(
        apps.join("pantalla-host.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Escritorio\nGenericName=Móvil como pantalla\n\
             Comment=Cámara, micro, brillo y conexión del móvil\nKeywords=movil;móvil;camara;cámara;microfono;brillo;desconectar;\n\
             Exec={d} menu\nIcon=escritorio-movil\nCategories=Utility;\nActions=panel;\n\
             X-KDE-Wayland-Interfaces={i}\n\n[Desktop Action panel]\nName=Abrir el panel\nExec={d} panel\n",
            d = dest.display(),
            i = crate::kde::INTERFACES
        ),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// `instalar --solo-menu`: solo el binario y el lanzador (para equipos que siguen con movild)
#[cfg(target_os = "linux")]
pub fn install_menu() -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin_dir = home().join(".local/bin");
    let dest = bin_dir.join("pantalla-host");
    std::fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
    if me != dest {
        let tmp = bin_dir.join(".pantalla-host.nuevo");
        std::fs::copy(&me, &tmp).map_err(|e| format!("copiar: {e}"))?;
        std::fs::rename(&tmp, &dest).map_err(|e| format!("copiar: {e}"))?;
    }
    write_launcher(&dest)?;
    println!("Lanzador \"Escritorio\" instalado ({} menu). El servicio no se toca.", dest.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<(), String> {
    run("systemctl", &["--user", "disable", "--now", "pantalla-host"]);
    let _ = std::fs::remove_file(home().join(".config/systemd/user/pantalla-host.service"));
    run("systemctl", &["--user", "daemon-reload"]);
    let _ = std::fs::remove_file(home().join(".local/bin/pantalla-host"));
    let _ = std::fs::remove_file(home().join(".local/share/applications/pantalla-host.desktop"));
    let _ = std::fs::remove_file(home().join(".local/share/icons/hicolor/scalable/apps/escritorio-movil.svg"));
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
