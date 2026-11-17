//! Linux / KDE Plasma (KWin 6, Wayland): monitor virtual con zkde_screencast (el mismo que usa
//! krfb-virtualmonitor), imágenes por PipeWire (pw.rs) y ratón/teclado con org_kde_kwin_fake_input.
//!
//! KWin solo deja usar esos dos protocolos a programas que lo declaran en su archivo .desktop
//! (X-KDE-Wayland-Interfaces); `pantalla-host instalar` lo crea. Sin él, KWin no anuncia los
//! protocolos y aquí se explica qué falta.

use crate::display::{Display, Frame, FrameSlot};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1 as xom, zxdg_output_v1 as xo};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input as fi;
use wayland_protocols_plasma::screencast::v1::client::{zkde_screencast_stream_unstable_v1 as kss, zkde_screencast_unstable_v1 as ksc};

type Err = Box<dyn std::error::Error>;

pub const INTERFACES: &str = "zkde_screencast_unstable_v1,org_kde_kwin_fake_input";

/// ¿Es una sesión de KWin? (aunque no nos deje usar sus protocolos)
pub fn is_kwin() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").map(|d| d.to_uppercase().contains("KDE")).unwrap_or(false)
        || std::env::var_os("KDE_FULL_SESSION").is_some()
}

#[derive(Default)]
struct St {
    node: Option<Result<u32, String>>,
    closed: bool,
    /// salida → (nombre, x, y, ancho, alto) en coordenadas lógicas
    outs: HashMap<u32, (String, i32, i32, i32, i32)>,
    keymap: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for St {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<kss::ZkdeScreencastStreamUnstableV1, ()> for St {
    fn event(s: &mut Self, _: &kss::ZkdeScreencastStreamUnstableV1, e: kss::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            kss::Event::Created { node } => s.node = Some(Ok(node)),
            kss::Event::Failed { error } => s.node = Some(Err(error)),
            kss::Event::Closed => s.closed = true,
            _ => {}
        }
    }
}
impl Dispatch<xo::ZxdgOutputV1, u32> for St {
    fn event(s: &mut Self, _: &xo::ZxdgOutputV1, e: xo::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let o = s.outs.entry(*id).or_insert((String::new(), 0, 0, 0, 0));
        match e {
            xo::Event::Name { name } => o.0 = name,
            xo::Event::LogicalPosition { x, y } => (o.1, o.2) = (x, y),
            xo::Event::LogicalSize { width, height } => (o.3, o.4) = (width, height),
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for St {
    fn event(s: &mut Self, _: &wl_keyboard::WlKeyboard, e: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Keymap { fd, size, .. } = e {
            s.keymap = read_keymap(fd, size as usize);
        }
    }
}
macro_rules! ignore {
    ($($i:ty),*) => {$(
        impl Dispatch<$i, ()> for St {
            fn event(_: &mut Self, _: &$i, _: <$i as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(ksc::ZkdeScreencastUnstableV1, fi::OrgKdeKwinFakeInput, xom::ZxdgOutputManagerV1, wl_seat::WlSeat);
impl Dispatch<wl_output::WlOutput, u32> for St {
    fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &u32, _: &Connection, _: &QueueHandle<Self>) {}
}

fn read_keymap(fd: OwnedFd, size: usize) -> Option<String> {
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, fd.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return None;
        }
        let s = std::slice::from_raw_parts(p as *const u8, size);
        let end = s.iter().position(|b| *b == 0).unwrap_or(size);
        let t = String::from_utf8_lossy(&s[..end]).into_owned();
        libc::munmap(p, size);
        Some(t)
    }
}

enum Input {
    Pointer(u16, u16, u8),
    Key(u32, bool),
}

struct Kde {
    w: u32,
    h: u32,
    slot: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    input: Mutex<Sender<Input>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Display for Kde {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn wait_frame(&self, after: u64, timeout: Duration) -> Option<Arc<Frame>> {
        self.slot.wait(after, timeout)
    }
    fn pointer(&self, x: u16, y: u16, mask: u8) {
        let _ = self.input.lock().unwrap().send(Input::Pointer(x, y, mask));
    }
    fn key(&self, keysym: u32, down: bool) {
        let _ = self.input.lock().unwrap().send(Input::Key(keysym, down));
    }
}

impl Drop for Kde {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

fn bind_new_outputs(globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<St>, xm: &xom::ZxdgOutputManagerV1, seen: &mut Vec<u32>) {
    let list: Vec<(u32, u32)> = globals.contents().with_list(|l| l.iter().filter(|g| g.interface == "wl_output").map(|g| (g.name, g.version)).collect());
    for (name, ver) in list {
        if !seen.contains(&name) {
            seen.push(name);
            let o: wl_output::WlOutput = globals.registry().bind(name, ver.min(4), qh, name);
            xm.get_xdg_output(&o, qh, name);
        }
    }
}

/// Coloca el monitor del móvil (POSITION de host.conf: auto-left, auto-right, auto-up, auto-down o XxY)
fn place(st: &St, out: &str, w: i32, h: i32, slot: u8) {
    let others: Vec<_> = st.outs.values().filter(|o| o.0 != out && o.3 > 0 && !crate::util::is_phone_output(&o.0)).collect();
    if others.is_empty() {
        return;
    }
    let pos = crate::util::position(slot);
    let (minx, maxx) = (others.iter().map(|o| o.1).min().unwrap(), others.iter().map(|o| o.1 + o.3).max().unwrap());
    let (miny, maxy) = (others.iter().map(|o| o.2).min().unwrap(), others.iter().map(|o| o.2 + o.4).max().unwrap());
    let left = others.iter().min_by_key(|o| o.1).unwrap();
    let right = others.iter().max_by_key(|o| o.1 + o.3).unwrap();
    let (x, y) = match pos.as_str() {
        "auto-left" => (minx - w, left.2 + (left.4 - h).max(0) / 2),
        "auto-up" => (minx, miny - h),
        "auto-down" => (minx, maxy),
        p => match p.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))) {
            Some(xy) => xy,
            None => (maxx, right.2 + (right.4 - h).max(0) / 2),
        },
    };
    let arg = format!("output.{out}.position.{x},{y}");
    match std::process::Command::new("kscreen-doctor").arg(&arg).output() {
        Ok(o) if o.status.success() => crate::host::log(&format!("kde: {out} en {x},{y}")),
        Ok(o) => crate::host::log(&format!("kde: kscreen-doctor {arg}: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(_) => crate::host::log("kde: sin kscreen-doctor, el monitor queda donde lo ponga KWin"),
    }
}

pub fn open(w: u32, h: u32, slot_nr: u8) -> Result<Arc<dyn Display>, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland: {e}"))?;
    let (globals, q) = registry_queue_init::<St>(&conn).map_err(|e| format!("Wayland: {e}"))?;
    let has = |n: &str| globals.contents().with_list(|l| l.iter().any(|g| g.interface == n));
    if !has("zkde_screencast_unstable_v1") || !has("org_kde_kwin_fake_input") {
        return Err(format!(
            "KWin no deja a pantalla-host crear el monitor ni mover el ratón: falta el permiso \
             (archivo .desktop con X-KDE-Wayland-Interfaces={INTERFACES}). Ejecuta `pantalla-host instalar` \
             y vuelve a arrancarlo."
        ));
    }
    let slot = Arc::new(FrameSlot::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let (ready_tx, ready_rx) = channel::<Result<(), String>>();
    let thread = {
        let (slot, stop) = (slot.clone(), stop.clone());
        std::thread::Builder::new()
            .name("kde".into())
            .spawn(move || {
                if let Err(e) = run(conn, globals, q, w, h, slot_nr, slot, &stop, rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e.to_string()));
                    crate::host::log(&format!("kde: {e}"));
                }
            })
            .map_err(|e| e.to_string())?
    };
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("KWin no ha creado el monitor virtual a tiempo".into()),
    }
    Ok(Arc::new(Kde { w, h, slot, stop, input: Mutex::new(tx), thread: Mutex::new(Some(thread)) }))
}

#[allow(clippy::too_many_arguments)]
fn run(conn: Connection, globals: wayland_client::globals::GlobalList, mut q: EventQueue<St>, w: u32, h: u32, slot_nr: u8, slot: Arc<FrameSlot>,
       stop: &AtomicBool, rx: Receiver<Input>, ready: &Sender<Result<(), String>>) -> Result<(), Err> {
    let qh = q.handle();
    let mut st = St::default();
    let sc: ksc::ZkdeScreencastUnstableV1 = globals.bind(&qh, 2..=4, ())?;
    let fake: fi::OrgKdeKwinFakeInput = globals.bind(&qh, 3..=5, ())?;
    fake.authenticate("Escritorio (móvil)".into(), "Ratón y teclado del móvil en su monitor".into());
    let xm: xom::ZxdgOutputManagerV1 = globals.bind(&qh, 2..=3, ())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ())?;
    let _kb = seat.get_keyboard(&qh, ());
    let mut seen = vec![];
    bind_new_outputs(&globals, &qh, &xm, &mut seen);
    q.roundtrip(&mut st)?;
    let before: Vec<String> = st.outs.values().map(|o| o.0.clone()).collect();
    let name = crate::util::output_name(slot_nr);

    // 2 = puntero dibujado en la imagen
    let stream = if sc.version() >= 4 {
        sc.stream_virtual_output_with_description(name.clone(), format!("Móvil {slot_nr} (Escritorio)"), w as i32, h as i32, 1.0, 2, &qh, ())
    } else {
        sc.stream_virtual_output(name.clone(), w as i32, h as i32, 1.0, 2, &qh, ())
    };
    let t0 = Instant::now();
    while st.node.is_none() && t0.elapsed() < Duration::from_secs(8) {
        crate::linux::dispatch_timeout(&mut q, &mut st, 100)?;
    }
    let node = match st.node.take() {
        Some(Ok(n)) => n,
        Some(Err(e)) => return Err(format!("KWin no pudo crear el monitor: {e}").into()),
        None => return Err("KWin no contesta al crear el monitor".into()),
    };
    let capture = crate::pw::Capture::start(node, w, h, slot)?;

    // la salida nueva: la que no estaba antes (KWin la llama "Virtual-MOVIL")
    let mut out: Option<String> = None;
    let t1 = Instant::now();
    while out.is_none() && t1.elapsed() < Duration::from_secs(3) {
        bind_new_outputs(&globals, &qh, &xm, &mut seen);
        q.roundtrip(&mut st)?;
        out = st.outs.values().find(|o| !before.contains(&o.0) && o.0.contains(&name)).map(|o| o.0.clone())
            .or_else(|| st.outs.values().find(|o| !before.contains(&o.0) && !o.0.is_empty()).map(|o| o.0.clone()));
    }
    let out = out.unwrap_or_else(|| format!("Virtual-{name}"));
    place(&st, &out, w as i32, h as i32, slot_nr);
    q.roundtrip(&mut st)?;
    crate::host::log(&format!("kde: monitor {out} (nodo PipeWire {node})"));
    let _ = ready.send(Ok(()));

    let mut keys = Keys::new(st.keymap.as_deref());
    let mut prev = 0u8;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Input::Pointer(x, y, mask)) => {
                let (ox, oy) = st.outs.values().find(|o| o.0 == out).map(|o| (o.1, o.2)).unwrap_or((0, 0));
                fake.pointer_motion_absolute(ox as f64 + x.min(w as u16 - 1) as f64, oy as f64 + y.min(h as u16 - 1) as f64);
                for (bit, btn) in [(1u8, 0x110u32), (2, 0x112), (4, 0x111)] {
                    if (mask ^ prev) & bit != 0 {
                        fake.button(btn, (mask & bit != 0) as u32);
                    }
                }
                for (bit, axis, v) in [(8u8, 0u32, -15.0), (16, 0, 15.0), (32, 1, -15.0), (64, 1, 15.0)] {
                    if mask & bit != 0 && prev & bit == 0 {
                        fake.axis(axis, v);
                    }
                }
                prev = mask;
            }
            Ok(Input::Key(sym, down)) => keys.key(&fake, sym, down),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(Ordering::Relaxed) || st.closed {
            break;
        }
        crate::linux::dispatch_timeout(&mut q, &mut st, 0)?;
        if st.keymap.is_some() && !keys.loaded {
            keys = Keys::new(st.keymap.as_deref());
        }
    }
    keys.release_all(&fake);
    for btn in [0x110u32, 0x111, 0x112] {
        if prev & 7 != 0 {
            fake.button(btn, 0);
        }
    }
    drop(capture);
    stream.close();
    conn.flush()?;
    let _ = conn.roundtrip();
    Ok(())
}

// ───────────── Teclado ─────────────
// fake_input manda códigos de tecla (evdev) y KWin les aplica la distribución del usuario; RFB manda
// keysyms. Se lee el mapa XKB que tiene KWin (wl_keyboard.keymap) para saber qué tecla y qué nivel
// (Mayús / AltGr) da cada símbolo, así funciona con el teclado español o cualquier otro.

const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTALT: u32 = 100;

struct Keys {
    loaded: bool,
    /// keysym → (código evdev, nivel 0..3)
    map: HashMap<u32, (u32, u8)>,
    /// teclas pulsadas: keysym → (código, modificadores añadidos)
    down: HashMap<u32, (u32, Vec<u32>)>,
}

impl Keys {
    fn new(keymap: Option<&str>) -> Keys {
        let mut map = HashMap::new();
        if let Some(t) = keymap {
            map = parse_keymap(t);
        }
        // si no se pudo leer el mapa, una distribución US básica
        if map.is_empty() {
            for (i, c) in "1234567890".chars().enumerate() {
                map.insert(c as u32, (2 + i as u32, 0));
            }
            for (row, start) in [("qwertyuiop", 16u32), ("asdfghjkl", 30), ("zxcvbnm", 44)] {
                for (i, c) in row.chars().enumerate() {
                    map.insert(c as u32, (start + i as u32, 0));
                    map.insert(c.to_ascii_uppercase() as u32, (start + i as u32, 1));
                }
            }
        }
        // teclas sin símbolo imprimible: fijas (evdev)
        for (sym, code) in [
            (0xff08u32, 14u32), (0xff09, 15), (0xff0d, 28), (0xff1b, 1), (0xffff, 111), (0xff50, 102), (0xff57, 107),
            (0xff55, 104), (0xff56, 109), (0xff51, 105), (0xff52, 103), (0xff53, 106), (0xff54, 108), (0xff63, 110),
            (0xff8d, 96), (0xffe1, 42), (0xffe2, 54), (0xffe3, 29), (0xffe4, 97), (0xffe9, 56), (0xffe7, 56), (0xffea, 100),
            (0xfe03, 100), (0xffeb, 125), (0xffec, 126), (0xff67, 127), (0xff61, 99), (0x20, 57),
        ] {
            map.entry(sym).or_insert((code, 0));
        }
        for i in 0..12u32 {
            map.entry(0xffbe + i).or_insert((if i < 10 { 59 + i } else { 87 + i - 10 }, 0));
        }
        Keys { loaded: keymap.is_some(), map, down: HashMap::new() }
    }

    fn key(&mut self, fake: &fi::OrgKdeKwinFakeInput, sym: u32, down: bool) {
        if !down {
            if let Some((code, added)) = self.down.remove(&sym) {
                fake.keyboard_key(code, 0);
                for m in added.iter().rev() {
                    fake.keyboard_key(*m, 0);
                }
            }
            return;
        }
        let Some(&(code, level)) = self.map.get(&sym) else {
            // letra con tilde que no está en el teclado: tecla muerta + letra
            if let Some((dead, base)) = decompose(sym) {
                if self.map.contains_key(&dead) && self.map.contains_key(&base) {
                    self.key(fake, dead, true);
                    self.key(fake, dead, false);
                    self.key(fake, base, true);
                    self.key(fake, base, false);
                }
            }
            return;
        };
        let is_mod = matches!(sym, 0xffe1..=0xffee | 0xfe03);
        let mut added = vec![];
        if !is_mod {
            let held = |c: u32| self.down.values().any(|(k, _)| *k == c);
            if level & 1 != 0 && !held(KEY_LEFTSHIFT) && !held(54) {
                added.push(KEY_LEFTSHIFT);
            }
            if level & 2 != 0 && !held(KEY_RIGHTALT) {
                added.push(KEY_RIGHTALT);
            }
        }
        for m in &added {
            fake.keyboard_key(*m, 1);
        }
        fake.keyboard_key(code, 1);
        self.down.insert(sym, (code, added));
    }

    fn release_all(&mut self, fake: &fi::OrgKdeKwinFakeInput) {
        for (_, (code, added)) in self.down.drain() {
            fake.keyboard_key(code, 0);
            for m in added {
                fake.keyboard_key(m, 0);
            }
        }
    }
}

/// á → (dead_acute, a), etc.
fn decompose(sym: u32) -> Option<(u32, u32)> {
    let c = char::from_u32(sym)?;
    let (dead, base) = match c {
        'á' | 'é' | 'í' | 'ó' | 'ú' | 'ý' | 'Á' | 'É' | 'Í' | 'Ó' | 'Ú' | 'Ý' => (0xfe51, "aeiouyAEIOUY".chars().nth("áéíóúýÁÉÍÓÚÝ".chars().position(|x| x == c)?)?),
        'à' | 'è' | 'ì' | 'ò' | 'ù' | 'À' | 'È' | 'Ì' | 'Ò' | 'Ù' => (0xfe50, "aeiouAEIOU".chars().nth("àèìòùÀÈÌÒÙ".chars().position(|x| x == c)?)?),
        'ä' | 'ë' | 'ï' | 'ö' | 'ü' | 'Ä' | 'Ë' | 'Ï' | 'Ö' | 'Ü' => (0xfe57, "aeiouAEIOU".chars().nth("äëïöüÄËÏÖÜ".chars().position(|x| x == c)?)?),
        'â' | 'ê' | 'î' | 'ô' | 'û' | 'Â' | 'Ê' | 'Î' | 'Ô' | 'Û' => (0xfe52, "aeiouAEIOU".chars().nth("âêîôûÂÊÎÔÛ".chars().position(|x| x == c)?)?),
        'ã' | 'õ' | 'Ã' | 'Õ' => (0xfe53, "aoAO".chars().nth("ãõÃÕ".chars().position(|x| x == c)?)?),
        _ => return None,
    };
    Some((dead, base as u32))
}

/// Del mapa XKB en texto: <NOMBRE> = código; y key <NOMBRE> { … [ sym, sym, … ] … }
fn parse_keymap(t: &str) -> HashMap<u32, (u32, u8)> {
    let mut codes: HashMap<String, u32> = HashMap::new();
    let mut map = HashMap::new();
    let Some(kc) = t.find("xkb_keycodes") else { return map };
    let sy = t.find("xkb_symbols").unwrap_or(t.len());
    for line in t[kc..sy].lines() {
        let l = line.trim();
        if let (Some(a), Some(eq)) = (l.find('<'), l.find('=')) {
            if l.starts_with('<') {
                if let (Some(b), Ok(n)) = (l[a..].find('>'), l[eq + 1..].trim().trim_end_matches(';').trim().parse::<u32>()) {
                    codes.insert(l[a + 1..a + b].to_string(), n);
                }
            } else if l.starts_with("alias") {
                // alias <AB11> = <LSGT>;
                let parts: Vec<&str> = l.split(['<', '>']).collect();
                if parts.len() >= 4 {
                    if let Some(n) = codes.get(parts[3]).copied() {
                        codes.insert(parts[1].to_string(), n);
                    }
                }
            }
        }
    }
    let syms = &t[sy..];
    let mut i = 0;
    while let Some(p) = syms[i..].find("key <") {
        let s = i + p + 5;
        let Some(e) = syms[s..].find('>') else { break };
        let name = &syms[s..s + e];
        let Some(close) = syms[s..].find("};") else { break };
        let body = &syms[s..s + close];
        i = s + close;
        let Some(xkb) = codes.get(name) else { continue };
        if *xkb < 8 {
            continue;
        }
        let evdev = xkb - 8;
        // primer grupo: la primera lista [ … ] (saltando el "symbols[1]=" si lo hay)
        let body = match body.find("symbols[") {
            Some(p) => body[p..].find("]=").map(|q| &body[p + q + 2..]).unwrap_or(body),
            None => body,
        };
        let (Some(a), Some(b)) = (body.find('['), body.find(']')) else { continue };
        if b < a {
            continue;
        }
        for (lvl, name) in body[a + 1..b].split(',').map(|x| x.trim()).enumerate().take(4) {
            if let Some(sym) = keysym(name) {
                // si un símbolo sale en varias teclas, se queda la de menor nivel
                let e = map.entry(sym).or_insert((evdev, lvl as u8));
                if (lvl as u8) < e.1 {
                    *e = (evdev, lvl as u8);
                }
            }
        }
    }
    map
}

/// Nombre de keysym XKB → valor
fn keysym(n: &str) -> Option<u32> {
    if n.is_empty() || n == "NoSymbol" || n == "VoidSymbol" {
        return None;
    }
    if let Some(h) = n.strip_prefix("0x") {
        return u32::from_str_radix(h, 16).ok();
    }
    if let Some(h) = n.strip_prefix('U') {
        if h.len() >= 4 && h.chars().all(|c| c.is_ascii_hexdigit()) {
            let cp = u32::from_str_radix(h, 16).ok()?;
            return Some(if cp < 0x100 { cp } else { 0x0100_0000 | cp });
        }
    }
    let mut ch = n.chars();
    if let (Some(c), None) = (ch.next(), ch.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    NAMES.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
}

const NAMES: &[(&str, u32)] = &[
    ("space", 0x20), ("exclam", 0x21), ("quotedbl", 0x22), ("numbersign", 0x23), ("dollar", 0x24), ("percent", 0x25),
    ("ampersand", 0x26), ("apostrophe", 0x27), ("parenleft", 0x28), ("parenright", 0x29), ("asterisk", 0x2a), ("plus", 0x2b),
    ("comma", 0x2c), ("minus", 0x2d), ("period", 0x2e), ("slash", 0x2f), ("colon", 0x3a), ("semicolon", 0x3b), ("less", 0x3c),
    ("equal", 0x3d), ("greater", 0x3e), ("question", 0x3f), ("at", 0x40), ("bracketleft", 0x5b), ("backslash", 0x5c),
    ("bracketright", 0x5d), ("asciicircum", 0x5e), ("underscore", 0x5f), ("grave", 0x60), ("braceleft", 0x7b), ("bar", 0x7c),
    ("braceright", 0x7d), ("asciitilde", 0x7e), ("nobreakspace", 0xa0), ("exclamdown", 0xa1), ("cent", 0xa2), ("sterling", 0xa3),
    ("currency", 0xa4), ("yen", 0xa5), ("brokenbar", 0xa6), ("section", 0xa7), ("diaeresis", 0xa8), ("copyright", 0xa9),
    ("ordfeminine", 0xaa), ("guillemotleft", 0xab), ("guillemetleft", 0xab), ("notsign", 0xac), ("hyphen", 0xad),
    ("registered", 0xae), ("macron", 0xaf), ("degree", 0xb0), ("plusminus", 0xb1), ("twosuperior", 0xb2), ("threesuperior", 0xb3),
    ("acute", 0xb4), ("mu", 0xb5), ("paragraph", 0xb6), ("periodcentered", 0xb7), ("cedilla", 0xb8), ("onesuperior", 0xb9),
    ("masculine", 0xba), ("ordmasculine", 0xba), ("guillemotright", 0xbb), ("guillemetright", 0xbb), ("onequarter", 0xbc),
    ("onehalf", 0xbd), ("threequarters", 0xbe), ("questiondown", 0xbf), ("Agrave", 0xc0), ("Aacute", 0xc1), ("Acircumflex", 0xc2),
    ("Atilde", 0xc3), ("Adiaeresis", 0xc4), ("Aring", 0xc5), ("AE", 0xc6), ("Ccedilla", 0xc7), ("Egrave", 0xc8), ("Eacute", 0xc9),
    ("Ecircumflex", 0xca), ("Ediaeresis", 0xcb), ("Igrave", 0xcc), ("Iacute", 0xcd), ("Icircumflex", 0xce), ("Idiaeresis", 0xcf),
    ("ETH", 0xd0), ("Ntilde", 0xd1), ("Ograve", 0xd2), ("Oacute", 0xd3), ("Ocircumflex", 0xd4), ("Otilde", 0xd5),
    ("Odiaeresis", 0xd6), ("multiply", 0xd7), ("Oslash", 0xd8), ("Ugrave", 0xd9), ("Uacute", 0xda), ("Ucircumflex", 0xdb),
    ("Udiaeresis", 0xdc), ("Yacute", 0xdd), ("THORN", 0xde), ("ssharp", 0xdf), ("agrave", 0xe0), ("aacute", 0xe1),
    ("acircumflex", 0xe2), ("atilde", 0xe3), ("adiaeresis", 0xe4), ("aring", 0xe5), ("ae", 0xe6), ("ccedilla", 0xe7),
    ("egrave", 0xe8), ("eacute", 0xe9), ("ecircumflex", 0xea), ("ediaeresis", 0xeb), ("igrave", 0xec), ("iacute", 0xed),
    ("icircumflex", 0xee), ("idiaeresis", 0xef), ("eth", 0xf0), ("ntilde", 0xf1), ("ograve", 0xf2), ("oacute", 0xf3),
    ("ocircumflex", 0xf4), ("otilde", 0xf5), ("odiaeresis", 0xf6), ("division", 0xf7), ("oslash", 0xf8), ("ugrave", 0xf9),
    ("uacute", 0xfa), ("ucircumflex", 0xfb), ("udiaeresis", 0xfc), ("yacute", 0xfd), ("thorn", 0xfe), ("ydiaeresis", 0xff),
    ("EuroSign", 0x20ac), ("dead_grave", 0xfe50), ("dead_acute", 0xfe51), ("dead_circumflex", 0xfe52), ("dead_tilde", 0xfe53),
    ("dead_macron", 0xfe54), ("dead_breve", 0xfe55), ("dead_abovedot", 0xfe56), ("dead_diaeresis", 0xfe57),
    ("dead_abovering", 0xfe58), ("dead_cedilla", 0xfe5b), ("BackSpace", 0xff08), ("Tab", 0xff09), ("ISO_Left_Tab", 0xfe20),
    ("Return", 0xff0d), ("Escape", 0xff1b), ("Delete", 0xffff), ("Home", 0xff50), ("Left", 0xff51), ("Up", 0xff52),
    ("Right", 0xff53), ("Down", 0xff54), ("Prior", 0xff55), ("Next", 0xff56), ("End", 0xff57), ("Insert", 0xff63),
    ("Menu", 0xff67), ("Print", 0xff61), ("KP_Enter", 0xff8d), ("Shift_L", 0xffe1), ("Shift_R", 0xffe2), ("Control_L", 0xffe3),
    ("Control_R", 0xffe4), ("Caps_Lock", 0xffe5), ("Alt_L", 0xffe9), ("Alt_R", 0xffea), ("Meta_L", 0xffe7), ("Super_L", 0xffeb),
    ("Super_R", 0xffec), ("ISO_Level3_Shift", 0xfe03), ("F1", 0xffbe), ("F2", 0xffbf), ("F3", 0xffc0), ("F4", 0xffc1),
    ("F5", 0xffc2), ("F6", 0xffc3), ("F7", 0xffc4), ("F8", 0xffc5), ("F9", 0xffc6), ("F10", 0xffc7), ("F11", 0xffc8), ("F12", 0xffc9),
];

#[cfg(test)]
mod tests {
    #[test]
    fn mapa_es() {
        let t = "xkb_keymap {\nxkb_keycodes \"x\" {\n <AE01> = 10;\n <AC10> = 47;\n <LSGT> = 94;\n alias <AB11> = <LSGT>;\n};\n\
                 xkb_symbols \"x\" {\n key <AE01> {\n type= \"FOUR_LEVEL\",\n symbols[1]= [ 1, exclam, bar, exclamdown ]\n };\n\
                 key <AC10> { [ ntilde, Ntilde ] };\n};\n};";
        let m = super::parse_keymap(t);
        assert_eq!(m.get(&('1' as u32)), Some(&(2, 0)));
        assert_eq!(m.get(&('!' as u32)), Some(&(2, 1)));
        assert_eq!(m.get(&('|' as u32)), Some(&(2, 2)));
        assert_eq!(m.get(&0xf1), Some(&(39, 0)));
        assert_eq!(super::decompose('é' as u32), Some((0xfe51, 'e' as u32)));
    }
}
