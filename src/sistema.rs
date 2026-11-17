//! `pantalla-host preparar-sistema` (como administrador; `instalar` lo lanza con pkexec o sudo):
//! lo único que necesita root, para que todo funcione nada más instalar:
//!  · webcam virtual "Camara del movil" (/dev/video42, v4l2loopback) — Meet, Discord, OBS…
//!  · permiso para crear webcams al vuelo (/dev/v4l2loopback, regla udev "uaccess"): una
//!    "Cámara de <móvil>" por cada móvil más que se conecte
//!  · abrir los puertos 47800/udp y 47801/tcp si hay cortafuegos (firewalld o ufw) — conexión por wifi

use std::process::{Command, Stdio};

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).stdin(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn out(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

fn has(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).chain(["/usr/sbin".into(), "/sbin".into()]).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

/// ¿Falta algo que necesite administrador? (para no pedir la contraseña si ya está todo)
pub fn needs_root() -> bool {
    !std::path::Path::new("/dev/video42").exists() || firewall_closed() || !ctl_writable()
}

const UDEV_RULE: &str = "/etc/udev/rules.d/70-pantalla-webcams.rules";

/// ¿Puede el usuario crear webcams al vuelo? (hace falta para la cámara del 2º móvil y siguientes)
fn ctl_writable() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/v4l2loopback").is_ok()
        || std::path::Path::new(UDEV_RULE).exists()
}

/// El usuario con sesión abierta puede crear y quitar webcams v4l2loopback (como con /dev/video*)
fn webcam_ctl() {
    let _ = std::fs::write(UDEV_RULE, "# pantalla-host: una webcam por cada móvil conectado (Cámara de <móvil>)\n\
                                       KERNEL==\"v4l2loopback\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\"\n");
    run("udevadm", &["control", "--reload"]);
    run("udevadm", &["trigger", "--subsystem-match=misc", "--sysname-match=v4l2loopback"]);
    println!("Webcams al vuelo: permitido (/dev/v4l2loopback).");
}

fn firewall_closed() -> bool {
    if has("firewall-cmd") && out("firewall-cmd", &["--state"]).trim() == "running" {
        let ports = out("firewall-cmd", &["--list-ports"]);
        let svc_zone_open = out("firewall-cmd", &["--list-all"]).contains("1025-65535/udp");
        return !(ports.contains("47801/tcp") || svc_zone_open);
    }
    if has("ufw") {
        let st = out("ufw", &["status"]);
        return st.contains("Status: active") && !st.contains("47801");
    }
    false
}

fn webcam() -> Result<(), String> {
    // cargado ya cuenta (tras actualizar el kernel sin reiniciar, modinfo no lo encuentra aunque funcione)
    let modinfo = || std::path::Path::new("/sys/module/v4l2loopback").exists() || run("modinfo", &["v4l2loopback"]);
    if !modinfo() {
        println!("Instalando v4l2loopback (webcam virtual)…");
        let kernel = out("uname", &["-r"]).trim().to_string();
        let ok = if has("apt-get") {
            run("apt-get", &["install", "-y", "v4l2loopback-dkms", &format!("linux-headers-{kernel}")])
                || run("apt-get", &["install", "-y", "v4l2loopback-dkms"])
        } else if has("dnf") {
            run("dnf", &["install", "-y", "v4l2loopback"]) || run("dnf", &["install", "-y", "akmod-v4l2loopback"])
        } else if has("pacman") {
            run("pacman", &["-S", "--noconfirm", "--needed", "v4l2loopback-dkms", "linux-headers"])
                || run("pacman", &["-S", "--noconfirm", "--needed", "v4l2loopback-dkms"])
        } else if has("zypper") {
            run("zypper", &["install", "-y", "v4l2loopback-kmp-default"])
        } else {
            false
        };
        if !ok || !modinfo() {
            return Err("no se pudo instalar v4l2loopback (instálalo con el gestor de paquetes de tu distribución)".into());
        }
    }
    std::fs::write("/etc/modprobe.d/movil-webcam.conf",
                   "options v4l2loopback video_nr=42 exclusive_caps=1 card_label=\"Camara del movil\"\n")
        .map_err(|e| e.to_string())?;
    std::fs::write("/etc/modules-load.d/movil-webcam.conf", "v4l2loopback\n").map_err(|e| e.to_string())?;
    if !std::path::Path::new("/dev/video42").exists() {
        // cargado con otras opciones (sin el 42): se recarga si nadie lo usa
        if out("lsmod", &[]).lines().any(|l| l.starts_with("v4l2loopback ")) {
            run("modprobe", &["-r", "v4l2loopback"]);
        }
        if !run("modprobe", &["v4l2loopback"]) {
            return Err("no se pudo cargar v4l2loopback (¿arranque seguro activado? hay que firmar el módulo o desactivarlo)".into());
        }
    }
    println!("Webcam virtual: /dev/video42 \"Camara del movil\".");
    Ok(())
}

fn firewall() {
    if has("firewall-cmd") && out("firewall-cmd", &["--state"]).trim() == "running" {
        if run("firewall-cmd", &["--permanent", "--add-port=47800/udp", "--add-port=47801/tcp"]) {
            run("firewall-cmd", &["--reload"]);
            println!("Cortafuegos (firewalld): abiertos 47800/udp y 47801/tcp.");
        }
    } else if has("ufw") && out("ufw", &["status"]).contains("Status: active") {
        run("ufw", &["allow", "47800/udp"]);
        run("ufw", &["allow", "47801/tcp"]);
        println!("Cortafuegos (ufw): abiertos 47800/udp y 47801/tcp.");
    }
}

pub fn prepare() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("hace falta ser administrador: sudo pantalla-host preparar-sistema".into());
    }
    firewall();
    webcam()?;
    webcam_ctl();
    Ok(())
}

/// Desde `instalar` (usuario normal): pide la contraseña una vez y lo prepara
pub fn ask_and_prepare(bin: &std::path::Path) {
    if !needs_root() {
        return;
    }
    println!("Falta preparar el sistema (webcam virtual y/o cortafuegos): se pide la contraseña de administrador.");
    let b = bin.to_string_lossy().to_string();
    let graphical = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let ok = if has("pkexec") && graphical {
        run("pkexec", &[&b, "preparar-sistema"])
    } else {
        run("sudo", &[&b, "preparar-sistema"])
    };
    if !ok {
        println!("No se ha preparado el sistema. Se puede hacer luego con: sudo {b} preparar-sistema");
    }
}
