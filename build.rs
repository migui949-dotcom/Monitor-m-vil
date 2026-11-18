// OpenH264 (openh264-sys2, "source") usa nasm si está en PATH y si no compila SIN ensamblador, en
// silencio, ~2,5 veces más lento. Aviso para que no vuelva a pasar sin darse cuenta.
fn main() {
    // Windows: la DLL de la cámara virtual (camara-win/) va dentro del .exe; sin compilarla antes, va vacía
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("pantalla_camara.dll");
        let src = std::path::Path::new("camara-win/target/x86_64-pc-windows-gnu/release/pantalla_camara.dll");
        println!("cargo:rerun-if-changed={}", src.display());
        match std::fs::read(src) {
            Ok(b) => std::fs::write(&out, b).unwrap(),
            Err(_) => {
                println!("cargo:warning=sin camara-win compilada: el .exe no llevará la cámara virtual (cd camara-win && cargo build --release --target x86_64-pc-windows-gnu)");
                std::fs::write(&out, b"").unwrap();
            }
        }
    }
    println!("cargo:rerun-if-env-changed=PATH");
    let x86 = std::env::var("CARGO_CFG_TARGET_ARCH").map(|a| a == "x86_64" || a == "x86").unwrap_or(false);
    let nasm = std::process::Command::new("nasm").arg("-v").output().map(|o| o.status.success()).unwrap_or(false);
    if x86 && !nasm {
        println!("cargo:warning=nasm no está en PATH: OpenH264 se compila SIN ensamblador (~2,5x más lento). Instala nasm y haz cargo clean -p openh264-sys2");
    }
}
