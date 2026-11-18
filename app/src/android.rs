//! Llamadas a Android por JNI que no tocan la ventana (cambiar sus flags la recreaba y bloqueaba la app).

#[cfg(target_os = "android")]
mod imp {
    use jni::objects::{GlobalRef, JObject, JValue};
    use jni::JavaVM;
    use std::sync::Mutex;
    use winit::platform::android::activity::AndroidApp;

    static WAKE: Mutex<Option<GlobalRef>> = Mutex::new(None);
    static APP: std::sync::OnceLock<AndroidApp> = std::sync::OnceLock::new();

    pub fn init(app: &AndroidApp) { let _ = APP.set(app.clone()); }

    /// Pide cámara y micrófono (diálogo de Android) si aún no se tienen
    pub fn ask_media_permissions() {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "askMediaPermissions", "()V", &[])?; Ok(()) });
        }
    }

    /// Pantalla "apagada": brillo de la ventana al mínimo (la app pinta negro encima)
    pub fn set_dim(dim: bool) {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "setDim", "(Z)V", &[JValue::Bool(dim as u8)])?; Ok(()) });
        }
    }

    /// Estado del móvil (JSON de MainActivity.getInfo)
    pub fn info() -> String {
        let mut out = String::new();
        if let Some(app) = APP.get() {
            run(app, |env, act| {
                let s = env.call_method(act, "getInfo", "()Ljava/lang/String;", &[])?.l()?;
                out = env.get_string(&jni::objects::JString::from(s))?.into();
                Ok(())
            });
        }
        out
    }

    pub fn set_brightness(v: f32) {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "setBrightness", "(F)V", &[JValue::Float(v)])?; Ok(()) });
        }
    }

    pub fn has_media_permissions() -> bool {
        let mut ok = false;
        if let Some(app) = APP.get() {
            run(app, |env, act| { ok = env.call_method(act, "hasMediaPermissions", "()Z", &[])?.z()?; Ok(()) });
        }
        ok
    }

    /// Cortafuegos propio: 1 = puesto, 0 = apagado
    pub fn firewall_state() -> i32 {
        let mut v = 0;
        if let Some(app) = APP.get() {
            run(app, |env, act| { v = env.call_method(act, "firewallState", "()I", &[])?.i()?; Ok(()) });
        }
        v
    }

    pub fn set_firewall(on: bool) {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "setFirewall", "(Z)V", &[JValue::Bool(on as u8)])?; Ok(()) });
        }
    }

    pub fn battery_exempt() -> bool {
        let mut ok = false;
        if let Some(app) = APP.get() {
            run(app, |env, act| { ok = env.call_method(act, "batteryExempt", "()Z", &[])?.z()?; Ok(()) });
        }
        ok
    }

    pub fn ask_battery() {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "askBattery", "()V", &[])?; Ok(()) });
        }
    }

    /// Giro físico del móvil (0/90/180/270, -1 si no se sabe)
    pub fn device_orientation() -> i32 {
        let mut v = -1;
        if let Some(app) = APP.get() {
            run(app, |env, act| { v = env.call_method(act, "deviceOrientation", "()I", &[])?.i()?; Ok(()) });
        }
        v
    }

    /// Saca (true) o quita (false) el teclado del móvil
    pub fn show_keyboard(on: bool) {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "showKeyboard", "(Z)V", &[JValue::Bool(on as u8)])?; Ok(()) });
        }
    }

    /// Lo escrito en el teclado del móvil desde la última vez
    pub fn take_typed() -> String {
        let mut out = String::new();
        if let Some(app) = APP.get() {
            run(app, |env, act| {
                let s = env.call_method(act, "takeTyped", "()Ljava/lang/String;", &[])?.l()?;
                out = env.get_string(&jni::objects::JString::from(s))?.into();
                Ok(())
            });
        }
        out
    }

    /// Alto del teclado en píxeles (-1 si Android no lo dice)
    pub fn ime_height() -> i32 {
        let mut v = -1;
        if let Some(app) = APP.get() {
            run(app, |env, act| { v = env.call_method(act, "imeHeight", "()I", &[])?.i()?; Ok(()) });
        }
        v
    }

    pub fn open_autostart() {
        if let Some(app) = APP.get() {
            run(app, |env, act| { env.call_method(act, "openAutostart", "()V", &[])?; Ok(()) });
        }
    }

    const SCREEN_BRIGHT_WAKE_LOCK: i32 = 0x0000_000a;
    const ON_AFTER_RELEASE: i32 = 0x2000_0000;

    fn run(app: &AndroidApp, f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<()>) {
        let vm = match unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) } { Ok(v) => v, Err(e) => { log::error!("JNI vm: {e}"); return; } };
        let mut env = match vm.attach_current_thread() { Ok(e) => e, Err(e) => { log::error!("JNI attach: {e}"); return; } };
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as jni::sys::jobject) };
        if let Err(e) = f(&mut env, &activity) {
            log::error!("JNI: {e}");
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
    }

    /// Pantalla encendida y con brillo mientras haya sesión (wake lock de PowerManager)
    pub fn keep_screen_on(app: &AndroidApp, on: bool) {
        run(app, |env, activity| {
            let mut slot = WAKE.lock().unwrap();
            if on && slot.is_none() {
                let name = env.new_string("power")?;
                let pm = env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;",
                                         &[JValue::Object(&name)])?.l()?;
                let tag = env.new_string("movil:pantalla")?;
                let wl = env.call_method(&pm, "newWakeLock", "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
                                         &[JValue::Int(SCREEN_BRIGHT_WAKE_LOCK | ON_AFTER_RELEASE), JValue::Object(&tag)])?.l()?;
                env.call_method(&wl, "acquire", "()V", &[])?;
                *slot = Some(env.new_global_ref(wl)?);
                log::info!("wake lock: pantalla encendida");
            } else if !on {
                if let Some(wl) = slot.take() {
                    env.call_method(wl.as_obj(), "release", "()V", &[])?;
                    log::info!("wake lock: liberado");
                }
            }
            Ok(())
        });
    }
}

#[cfg(target_os = "android")]
pub use imp::{ask_battery, ask_media_permissions, battery_exempt, device_orientation, firewall_state, has_media_permissions, info, init, keep_screen_on,
              open_autostart, set_brightness, set_dim, set_firewall, show_keyboard, take_typed, ime_height};
