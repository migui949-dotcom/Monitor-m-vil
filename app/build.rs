fn main() {
    // Camera2 del NDK (libcamera2ndk.so): ndk-sys trae las funciones pero no enlaza la librería
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=camera2ndk");
    }
}
