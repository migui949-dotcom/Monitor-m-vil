// OpenH264 (openh264-sys2, "source") usa nasm si está en PATH y si no compila SIN ensamblador, en
// silencio, ~2,5 veces más lento. Aviso para que no vuelva a pasar sin darse cuenta.
fn main() {
    println!("cargo:rerun-if-env-changed=PATH");
    let x86 = std::env::var("CARGO_CFG_TARGET_ARCH").map(|a| a == "x86_64" || a == "x86").unwrap_or(false);
    let nasm = std::process::Command::new("nasm").arg("-v").output().map(|o| o.status.success()).unwrap_or(false);
    if x86 && !nasm {
        println!("cargo:warning=nasm no está en PATH: OpenH264 se compila SIN ensamblador (~2,5x más lento). Instala nasm y haz cargo clean -p openh264-sys2");
    }
}
