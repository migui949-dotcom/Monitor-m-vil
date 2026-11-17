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
mod kde;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod panel;
#[cfg(target_os = "linux")]
mod pw;
#[cfg(target_os = "linux")]
mod storage;
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
            serve()
        }
        _ => {
            eprintln!("uso: pantalla-host [instalar [--sustituir-movild] | desinstalar | panel | info | --version]");
            std::process::exit(2);
        }
    }
}
