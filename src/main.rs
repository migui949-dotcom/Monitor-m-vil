//! pantalla-host: el servicio del ordenador para la app "Pantalla" del móvil, en un solo binario.
//!
//!   pantalla-host               escucha (descubrimiento por UDP, conexión cifrada, permiso, escritorio)
//!   pantalla-host instalar      se instala (Linux: servicio de usuario; Windows: inicio de sesión)
//!   pantalla-host desinstalar
//!   pantalla-host info          nombre, huella y si este sistema puede crear el escritorio
//!   pantalla-host --version
//!
//! En Windows, abrir el .exe con doble clic lo instala.

// En Windows no se abre una consola al arrancar con la sesión
#![cfg_attr(windows, windows_subsystem = "windows")]

mod display;
mod host;
mod install;
mod notify;
mod rfb;
mod seclog;
mod util;
mod video;

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod devices;
#[cfg(target_os = "linux")]
mod hwenc;
#[cfg(target_os = "linux")]
mod kde;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod menu;
#[cfg(target_os = "linux")]
mod panel;
#[cfg(target_os = "linux")]
mod pw;
#[cfg(target_os = "linux")]
mod sistema;
#[cfg(target_os = "linux")]
mod storage;
#[cfg(target_os = "linux")]
mod teclado;
#[cfg(windows)]
mod winaudio;
#[cfg(windows)]
mod windows;

fn serve() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    if let Err(e) = rt.block_on(host::run()) {
        host::log(&format!("error: {e}"));
        eprintln!("pantalla-host: {e}");
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    windows::attach_console();
    match args.first().map(String::as_str).unwrap_or("") {
        "--version" | "-V" => println!("pantalla-host {}", env!("CARGO_PKG_VERSION")),
        "info" => {
            if let Err(e) = host::load_identity() {
                eprintln!("no se pudo leer la identidad: {e}");
                std::process::exit(1);
            }
            println!("nombre: {}", host::host_name());
            println!("huella: {}", host::fingerprint_self());
            println!("estado: {}", util::state_dir().display());
            #[cfg(target_os = "linux")]
            println!("escritorio virtual: {}", if linux::is_hyprland() { "sí (Hyprland)" } else { "no (hace falta Hyprland)" });
            #[cfg(windows)]
            println!("escritorio virtual: {}", windows::capability());
        }
        "instalar" | "install" => {
            let replace = args.iter().any(|a| a == "--sustituir-movild");
            #[cfg(target_os = "linux")]
            if args.iter().any(|a| a == "--solo-menu") {
                if let Err(e) = install::install_menu() {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
                return;
            }
            match install::install(replace) {
                Ok(()) => {
                    #[cfg(windows)]
                    windows::message("Pantalla", "Instalado. Ya puedes conectarte desde la app del móvil.");
                }
                Err(e) => {
                    eprintln!("{e}");
                    #[cfg(windows)]
                    windows::message("Pantalla", &format!("No se pudo instalar: {e}"));
                    std::process::exit(1);
                }
            }
        }
        #[cfg(target_os = "linux")]
        "preparar-sistema" => {
            if let Err(e) = sistema::prepare() {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        #[cfg(target_os = "linux")]
        "menu" => {
            if let Err(e) = menu::run() {
                eprintln!("menú: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(target_os = "linux")]
        "camara" | "cámara" => {
            if let Err(e) = panel::run_camera() {
                eprintln!("cámara: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(target_os = "linux")]
        "escritorio" => {
            // Super+N de Hyprland (lo reasigna el servicio mientras hay un móvil): con el foco en el
            // monitor del móvil, N lleva a su escritorio N (11..20 en la 1ª ranura)
            let accion = args.get(1).map(String::as_str).unwrap_or("workspace");
            let n = args.get(2).and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
            linux::workspace_key(accion, n);
        }
        #[cfg(target_os = "linux")]
        "panel" => {
            if let Err(e) = panel::run() {
                eprintln!("panel: {e}");
                std::process::exit(1);
            }
        }
        "desinstalar" | "uninstall" => {
            if let Err(e) = install::uninstall() {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        "" => {
            // Windows: con doble clic desde fuera de la carpeta de instalación → se instala
            #[cfg(windows)]
            if !windows::running_installed() {
                match install::install(false) {
                    Ok(()) => windows::message("Pantalla", "Instalado. Ya puedes conectarte desde la app del móvil."),
                    Err(e) => windows::message("Pantalla", &format!("No se pudo instalar: {e}")),
                }
                return;
            }
            // Linux: abierto a mano (doble clic o terminal) desde fuera de ~/.local/bin → se instala
            // solo (servicio, lanzador, webcam con una contraseña gráfica) y abre el panel
            #[cfg(target_os = "linux")]
            if std::env::var_os("INVOCATION_ID").is_none()
                && std::env::current_exe().ok() != Some(install::home().join(".local/bin/pantalla-host"))
            {
                if let Err(e) = install::install(false) {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
                return;
            }
            serve()
        }
        _ => {
            eprintln!("uso: pantalla-host [instalar [--sustituir-movild | --solo-menu] | desinstalar | menu | panel | preparar-sistema | info | --version]");
            std::process::exit(2);
        }
    }
}
