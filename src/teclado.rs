//! Teclado del móvil: el servicio se registra en Wayland como «método de entrada»
//! (zwp_input_method_v2). Así el compositor le avisa cuando una app activa un campo de texto
//! (text-input-v3: GTK, Qt, Firefox, Chromium con --enable-wayland-ime…). Si la ventana con el
//! foco está en el monitor de un móvil, a ese móvil se le pide que saque su teclado; lo que se
//! escribe en él vuelve como texto (commit_string: vale cualquier carácter, ñ, tildes, emoji).
//!
//! Hay un solo método de entrada por asiento: un hilo para todos los móviles, que vive mientras
//! haya alguno conectado. Si ya hay otro (fcitx, ibus…), el compositor lo dice (`unavailable`)
//! y el texto se escribe con teclas.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::{zwp_input_method_manager_v2 as imm, zwp_input_method_v2 as im};

type Aviso = Box<dyn Fn(bool) + Send + Sync>;

/// móviles conectados (ranura → cómo avisarle)
static AVISOS: Mutex<Vec<(u8, std::sync::Arc<Aviso>)>> = Mutex::new(Vec::new());
static HILO: Mutex<Option<Sender<String>>> = Mutex::new(None);
/// hay un campo de texto activo y es nuestro método de entrada quien lo atiende
static ACTIVO: AtomicBool = AtomicBool::new(false);

pub fn register(slot: u8, f: Aviso) {
    AVISOS.lock().unwrap().retain(|x| x.0 != slot);
    AVISOS.lock().unwrap().push((slot, std::sync::Arc::new(f)));
    let mut h = HILO.lock().unwrap();
    if h.is_none() {
        let (tx, rx) = channel();
        *h = Some(tx);
        let _ = std::thread::Builder::new().name("teclado".into()).spawn(move || {
            if let Err(e) = run(rx) {
                crate::host::log(&format!("teclado del móvil (método de entrada): {e}"));
            }
            ACTIVO.store(false, Ordering::Relaxed);
            *HILO.lock().unwrap() = None;
        });
    }
}

pub fn unregister(slot: u8) {
    let mut a = AVISOS.lock().unwrap();
    a.retain(|x| x.0 != slot);
    if a.is_empty() {
        // sin móviles el hilo se va (al soltar el emisor)
        HILO.lock().unwrap().take();
    }
}

/// ¿Se puede mandar texto como texto (y no como teclas)?
pub fn active() -> bool {
    ACTIVO.load(Ordering::Relaxed)
}

/// Texto para el campo activo (solo caracteres normales; Intro/borrar van como teclas)
pub fn commit(s: &str) -> bool {
    if !active() {
        return false;
    }
    HILO.lock().unwrap().as_ref().map(|h| h.send(s.to_string()).is_ok()).unwrap_or(false)
}

#[derive(Default)]
struct St {
    /// lo que dice el compositor, aplicado al llegar `done`
    pending: Option<bool>,
    active: bool,
    serial: u32,
    unavailable: bool,
    changed: bool,
}

impl Dispatch<im::ZwpInputMethodV2, ()> for St {
    fn event(s: &mut Self, _: &im::ZwpInputMethodV2, e: im::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            im::Event::Activate => s.pending = Some(true),
            im::Event::Deactivate => s.pending = Some(false),
            im::Event::Done => {
                s.serial = s.serial.wrapping_add(1);
                if let Some(p) = s.pending.take() {
                    // desactivar + activar en el mismo `done` = otro campo: se vuelve a mirar dónde está
                    s.changed = s.changed || p != s.active || p;
                    s.active = p;
                }
            }
            im::Event::Unavailable => s.unavailable = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for St {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_seat::WlSeat, ()> for St {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<imm::ZwpInputMethodManagerV2, ()> for St {
    fn event(_: &mut Self, _: &imm::ZwpInputMethodManagerV2, _: imm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// Ranura del móvil en cuyo monitor está la ventana con el foco (si está en uno)
fn focused_slot() -> Option<u8> {
    let run = |a: &[&str]| std::process::Command::new("hyprctl").args(a).output().ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());
    let win = run(&["activewindow", "-j"])?;
    let mon_id = win["monitor"].as_i64()?;
    let mons = run(&["monitors", "-j"])?;
    let name = mons.as_array()?.iter().find(|m| m["id"].as_i64() == Some(mon_id))?["name"].as_str()?.to_string();
    (1..=4u8).find(|s| crate::util::output_name(*s) == name)
}

fn avisar(slot: u8, on: bool) {
    let f = AVISOS.lock().unwrap().iter().find(|x| x.0 == slot).map(|x| x.1.clone());
    if let Some(f) = f {
        f(on);
    }
}

fn run(rx: Receiver<String>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut q) = registry_queue_init::<St>(&conn)?;
    let qh = q.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ())?;
    let mgr: imm::ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ()).map_err(|_| "el compositor no tiene zwp_input_method_v2")?;
    let ime = mgr.get_input_method(&seat, &qh, ());
    let mut s = St::default();
    q.roundtrip(&mut s)?;
    crate::host::log("teclado del móvil: método de entrada listo");
    // móvil al que se le ha pedido el teclado; y cuándo se le quitará (con un poco de margen:
    // al pasar de un campo a otro las apps desactivan y vuelven a activar)
    let mut shown: Option<u8> = None;
    let mut hide_at: Option<Instant> = None;
    loop {
        crate::linux::dispatch_timeout(&mut q, &mut s, 50)?;
        if s.unavailable {
            ACTIVO.store(false, Ordering::Relaxed);
            if let Some(sl) = shown.take() {
                avisar(sl, false);
            }
            ime.destroy();
            conn.flush()?;
            return Err("ya hay otro método de entrada (fcitx, ibus…): el texto irá con teclas y el teclado se abre a mano".into());
        }
        if s.changed {
            s.changed = false;
            if s.active {
                let slot = focused_slot();
                ACTIVO.store(slot.is_some(), Ordering::Relaxed);
                if shown.is_some() && shown != slot {
                    avisar(shown.take().unwrap(), false);
                }
                if let Some(sl) = slot {
                    hide_at = None;
                    if shown != Some(sl) {
                        avisar(sl, true);
                        shown = Some(sl);
                    }
                }
            } else {
                ACTIVO.store(false, Ordering::Relaxed);
                if shown.is_some() && hide_at.is_none() {
                    hide_at = Some(Instant::now() + Duration::from_millis(400));
                }
            }
        }
        if hide_at.map(|t| Instant::now() >= t).unwrap_or(false) {
            hide_at = None;
            if !s.active {
                if let Some(sl) = shown.take() {
                    avisar(sl, false);
                }
            }
        }
        loop {
            match rx.try_recv() {
                Ok(text) => {
                    if s.active {
                        ime.commit_string(text);
                        ime.commit(s.serial);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    ACTIVO.store(false, Ordering::Relaxed);
                    ime.destroy();
                    conn.flush()?;
                    return Ok(());
                }
            }
        }
        conn.flush()?;
    }
}
