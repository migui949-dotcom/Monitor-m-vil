//! pantalla-host: el servicio del ordenador para la app "Pantalla" del móvil, en un solo binario.
//!
//!   pantalla-host            escucha (descubrimiento por UDP, conexión cifrada, permiso, escritorio)
//!   pantalla-host info       muestra nombre, huella y si este sistema puede crear el escritorio
//!   pantalla-host --version

mod display;
mod host;
mod notify;
mod rfb;
mod seclog;
mod util;
mod video;

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod storage;
#[cfg(windows)]
mod windows;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--version" | "-V" => println!("pantalla-host {}", env!("CARGO_PKG_VERSION")),
        "info" => {
            if let Err(e) = host::load_identity() {
                eprintln!("no se pudo leer la identidad: {e}");
                std::process::exit(1);
            }
            println!("nombre: {}", host::host_name());
            println!("estado: {}", util::state_dir().display());
            #[cfg(target_os = "linux")]
            println!("escritorio virtual: {}", if linux::is_hyprland() { "sí (Hyprland)" } else { "no (hace falta Hyprland)" });
            #[cfg(windows)]
            println!("escritorio virtual: {}", windows::capability());
        }
        "" => {
            let rt = tokio::runtime::Runtime::new().expect("runtime");
            if let Err(e) = rt.block_on(host::run()) {
                eprintln!("pantalla-host: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("uso: pantalla-host [info|--version]");
            std::process::exit(2);
        }
    }
}
