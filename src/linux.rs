//! Linux / Hyprland: monitor virtual (salida "headless"), captura con wlr-screencopy (memoria
//! compartida, sin GPU) y ratón/teclado con los protocolos virtuales de wlroots. Solo hace falta
//! Hyprland (que ya trae hyprctl y todos estos protocolos).

use crate::display::{Display, Frame, FrameSlot, PixelOrder};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1 as vkm, zwp_virtual_keyboard_v1 as vk};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1 as scf, zwlr_screencopy_manager_v1 as scm};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1 as vpm, zwlr_virtual_pointer_v1 as vp};

const OUTPUT: &str = "MOVIL";

// ───────────── Hyprland ─────────────

/// Si se arranca desde systemd o SSH puede faltar el entorno de Hyprland: se busca la sesión.
pub fn ensure_env() {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        // la instancia más reciente con socket vivo
        let mut best: Option<(std::time::SystemTime, String)> = None;
        for e in std::fs::read_dir(format!("{runtime}/hypr")).into_iter().flatten().flatten() {
            if e.path().join(".socket.sock").exists() {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map(|b| t > b.0).unwrap_or(true) {
                    best = Some((t, e.file_name().to_string_lossy().into_owned()));
                }
            }
        }
        if let Some((_, sig)) = best {
            std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", sig);
        }
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        for n in 0..10 {
            let s = format!("wayland-{n}");
            if std::path::Path::new(&format!("{runtime}/{s}")).exists() {
                std::env::set_var("WAYLAND_DISPLAY", s);
                break;
            }
        }
    }
}

fn hyprctl(args: &[&str]) -> Result<String, String> {
    let o = std::process::Command::new("hyprctl").args(args).output().map_err(|e| format!("hyprctl: {e}"))?;
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

pub fn is_hyprland() -> bool {
    ensure_env();
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() && hyprctl(&["version"]).map(|v| v.contains("Hyprland")).unwrap_or(false)
}

fn create_monitor(w: u32, h: u32) -> Result<(), String> {
    if !hyprctl(&["monitors", "all"])?.lines().any(|l| l.starts_with(&format!("Monitor {OUTPUT} "))) {
        hyprctl(&["output", "create", "headless", OUTPUT])?;
        std::thread::sleep(Duration::from_millis(300));
    }
    let pos = crate::util::conf("POSITION").unwrap_or_else(|| "auto-right".into());
    let mode = format!("{w}x{h}@60");
    // Hyprland con configuración en Lua (0.57+) o clásica
    let lua = hyprctl(&["eval", "return 1"])
        .map(|r| {
            let r = r.to_lowercase();
            !r.contains("only supported with the lua") && !r.contains("unknown") && !r.contains("invalid")
        })
        .unwrap_or(false);
    if lua {
        hyprctl(&["eval", &format!("hl.monitor({{ output = \"{OUTPUT}\", mode = \"{mode}\", position = \"{pos}\", scale = \"1\" }})")])?;
    } else {
        hyprctl(&["keyword", "monitor", &format!("{OUTPUT},{mode},{pos},1")])?;
    }
    Ok(())
}

pub fn remove_monitor() {
    let _ = hyprctl(&["output", "remove", OUTPUT]);
}

// ───────────── Display ─────────────

enum Input {
    Pointer(u16, u16, u8),
    Key(u32, bool),
}

struct Hypr {
    w: u32,
    h: u32,
    slot: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    input: Mutex<Sender<Input>>,
}

impl Display for Hypr {
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

impl Drop for Hypr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        remove_monitor();
    }
}

pub fn open(w: u32, h: u32) -> Result<Arc<dyn Display>, String> {
    ensure_env();
    if !is_hyprland() {
        return Err("no se encuentra una sesión de Hyprland".into());
    }
    create_monitor(w, h)?;
    let slot = Arc::new(FrameSlot::default());
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (slot, stop) = (slot.clone(), stop.clone());
        std::thread::Builder::new()
            .name("captura".into())
            .spawn(move || {
                if let Err(e) = capture_loop(&slot, &stop) {
                    crate::host::log(&format!("captura: {e}"));
                }
            })
            .map_err(|e| e.to_string())?;
    }
    let (tx, rx) = channel();
    {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("entrada".into())
            .spawn(move || {
                if let Err(e) = input_loop(rx, w, h, &stop) {
                    crate::host::log(&format!("entrada: {e}"));
                }
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(Arc::new(Hypr { w, h, slot, stop, input: Mutex::new(tx) }))
}

// ───────────── Utilidades Wayland ─────────────

type Err = Box<dyn std::error::Error>;

/// Despacha eventos esperando como mucho `ms` (para poder mirar `stop` de vez en cuando).
fn dispatch_timeout<S>(q: &mut EventQueue<S>, s: &mut S, ms: i32) -> Result<(), Err> {
    q.dispatch_pending(s)?;
    q.flush()?;
    if let Some(guard) = q.prepare_read() {
        let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, ms) } > 0 {
            guard.read()?;
        }
    }
    q.dispatch_pending(s)?;
    Ok(())
}

fn memfd(name: &str, size: usize) -> Result<OwnedFd, Err> {
    let cname = std::ffi::CString::new(name)?;
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(fd)
}

/// Busca la wl_output llamada MOVIL (protocolo wl_output v4: evento `name`).
#[derive(Default)]
struct Outputs {
    list: Vec<(wl_output::WlOutput, Option<String>)>,
}

impl Outputs {
    fn find(&self) -> Option<wl_output::WlOutput> {
        self.list.iter().find(|(_, n)| n.as_deref() == Some(OUTPUT)).map(|(o, _)| o.clone())
    }
}

fn bind_outputs<S>(globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<S>) -> Vec<(wl_output::WlOutput, Option<String>)>
where
    S: Dispatch<wl_output::WlOutput, ()> + 'static,
{
    globals.contents().with_list(|l| {
        l.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| (globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), qh, ()), None))
            .collect()
    })
}

macro_rules! ignore_events {
    ($state:ty: $($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for $state {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

macro_rules! output_names {
    ($state:ty) => {
        impl Dispatch<wl_output::WlOutput, ()> for $state {
            fn event(s: &mut Self, o: &wl_output::WlOutput, e: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                if let wl_output::Event::Name { name } = e {
                    if let Some(x) = s.outputs.list.iter_mut().find(|(p, _)| p == o) {
                        x.1 = Some(name);
                    }
                }
            }
        }
        impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for $state {
            fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
        }
    };
}

/// Conecta y espera (hasta 5 s) a que aparezca la salida MOVIL.
fn connect_with_output<S>(mk: impl Fn() -> S, outs: impl Fn(&mut S) -> &mut Outputs) -> Result<(Connection, EventQueue<S>, wayland_client::globals::GlobalList, S, wl_output::WlOutput), Err>
where
    S: Dispatch<wl_output::WlOutput, ()> + Dispatch<wl_registry::WlRegistry, GlobalListContents> + 'static,
{
    let t0 = Instant::now();
    loop {
        let conn = Connection::connect_to_env()?;
        let (globals, mut q) = registry_queue_init::<S>(&conn)?;
        let qh = q.handle();
        let mut s = mk();
        outs(&mut s).list = bind_outputs(&globals, &qh);
        q.roundtrip(&mut s)?;
        if let Some(o) = outs(&mut s).find() {
            return Ok((conn, q, globals, s, o));
        }
        if t0.elapsed() > Duration::from_secs(5) {
            return Err(format!("no aparece la salida {OUTPUT}").into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ───────────── Captura ─────────────

#[derive(Default)]
struct Cap {
    outputs: Outputs,
    fmt: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    invert: bool,
    ready: bool,
    failed: bool,
}

output_names!(Cap);
ignore_events!(Cap: wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, scm::ZwlrScreencopyManagerV1);

impl Dispatch<scf::ZwlrScreencopyFrameV1, ()> for Cap {
    fn event(s: &mut Self, _: &scf::ZwlrScreencopyFrameV1, e: scf::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            scf::Event::Buffer { format: WEnum::Value(f), width, height, stride } => {
                if s.fmt.is_none() || matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) {
                    s.fmt = Some((f, width, height, stride));
                }
            }
            scf::Event::BufferDone => s.buffer_done = true,
            scf::Event::Flags { flags: WEnum::Value(f) } => s.invert = f.contains(scf::Flags::YInvert),
            scf::Event::Ready { .. } => s.ready = true,
            scf::Event::Failed => s.failed = true,
            _ => {}
        }
    }
}

struct ShmBuf {
    key: (wl_shm::Format, u32, u32, u32),
    map: memmap2::MmapMut,
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
}

fn capture_loop(slot: &FrameSlot, stop: &AtomicBool) -> Result<(), Err> {
    let (_conn, mut q, globals, mut s, output) = connect_with_output(Cap::default, |s| &mut s.outputs)?;
    let qh = q.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let mgr: scm::ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ())?;
    let v3 = mgr.version() >= 3;
    let mut buf: Option<ShmBuf> = None;
    let mut first = true;
    let min_gap = Duration::from_millis(16);
    let mut last = Instant::now() - min_gap;

    while !stop.load(Ordering::Relaxed) {
        s.fmt = None;
        s.buffer_done = false;
        s.ready = false;
        s.failed = false;
        s.invert = false;
        let frame = mgr.capture_output(1, &output, &qh, ());
        // formato del búfer (v3 avisa con buffer_done; antes basta con el ida-y-vuelta)
        if v3 {
            while !s.buffer_done && !s.failed && !stop.load(Ordering::Relaxed) {
                dispatch_timeout(&mut q, &mut s, 500)?;
            }
        } else {
            q.roundtrip(&mut s)?;
        }
        let Some(key) = s.fmt.filter(|_| !s.failed) else {
            frame.destroy();
            if stop.load(Ordering::Relaxed) {
                break;
            }
            return Err("la captura falló (¿se quitó el monitor?)".into());
        };
        let (format, w, h, stride) = key;
        let order = match format {
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => PixelOrder::Bgrx,
            wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => PixelOrder::Rgbx,
            other => return Err(format!("formato de captura no soportado: {other:?}").into()),
        };
        if buf.as_ref().map(|b| b.key != key).unwrap_or(true) {
            if let Some(old) = buf.take() {
                old.buffer.destroy();
                old.pool.destroy();
            }
            let size = (stride * h) as usize;
            let fd = memfd("pantalla-captura", size)?;
            let file = std::fs::File::from(fd.try_clone()?);
            let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
            let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
            let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &qh, ());
            buf = Some(ShmBuf { key, map, buffer, pool });
            first = true;
        }
        let b = buf.as_ref().unwrap();
        // la primera vez copia ya; después espera a que algo cambie en el monitor
        if first {
            frame.copy(&b.buffer);
        } else {
            frame.copy_with_damage(&b.buffer);
        }
        while !s.ready && !s.failed && !stop.load(Ordering::Relaxed) {
            dispatch_timeout(&mut q, &mut s, 500)?;
        }
        frame.destroy();
        if !s.ready {
            continue;
        }
        first = false;
        let (w, h, stride) = (w as usize, h as usize, stride as usize);
        let mut data = vec![0u8; stride * h];
        if s.invert {
            for y in 0..h {
                data[y * stride..(y + 1) * stride].copy_from_slice(&b.map[(h - 1 - y) * stride..(h - y) * stride]);
            }
        } else {
            data.copy_from_slice(&b.map[..stride * h]);
        }
        slot.publish(Frame { width: w, height: h, stride, order, data, seq: 0 });
        // como mucho ~60 fotogramas por segundo
        let el = last.elapsed();
        if el < min_gap {
            std::thread::sleep(min_gap - el);
        }
        last = Instant::now();
    }
    Ok(())
}

// ───────────── Ratón y teclado ─────────────

#[derive(Default)]
struct Inp {
    outputs: Outputs,
}

output_names!(Inp);
ignore_events!(Inp: wl_seat::WlSeat, vpm::ZwlrVirtualPointerManagerV1, vp::ZwlrVirtualPointerV1, vkm::ZwpVirtualKeyboardManagerV1, vk::ZwpVirtualKeyboardV1);

/// Teclado virtual con un mapa XKB propio: cada keysym que llega ocupa su propia tecla
/// (como hace wtype), así no hace falta libxkbcommon y vale cualquier distribución.
struct Keys {
    kb: vk::ZwpVirtualKeyboardV1,
    map: Vec<u32>, // keysym en la posición (código xkb - 9)
    mods: u32,
    down: Vec<u32>,
    _fd: Option<OwnedFd>,
}

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 4;
const MOD_ALT: u32 = 8;
const MOD_SUPER: u32 = 64;
const MOD_ALTGR: u32 = 128;

fn mod_of(sym: u32) -> u32 {
    match sym {
        0xffe1 | 0xffe2 => MOD_SHIFT,
        0xffe3 | 0xffe4 => MOD_CTRL,
        0xffe7 | 0xffe8 | 0xffe9 => MOD_ALT,
        0xffeb | 0xffec => MOD_SUPER,
        0xfe03 | 0xffea => MOD_ALTGR,
        _ => 0,
    }
}

impl Keys {
    fn new(kb: vk::ZwpVirtualKeyboardV1) -> Result<Self, Err> {
        let mut map: Vec<u32> = vec![
            0xff08, 0xff09, 0xff0d, 0xff1b, 0xffff, 0xff50, 0xff51, 0xff52, 0xff53, 0xff54, 0xff55, 0xff56, 0xff57,
            0xff63, 0xff67, 0xff8d, 0xff61, 0xffe1, 0xffe2, 0xffe3, 0xffe4, 0xffe5, 0xffe7, 0xffe9, 0xffea, 0xffeb,
            0xffec, 0xfe03,
        ];
        map.extend(0xffbe..=0xffc9); // F1-F12
        map.extend(0x20..=0x7e); // ASCII
        map.extend([0xf1, 0xd1, 0xe1, 0xe9, 0xed, 0xf3, 0xfa, 0xc1, 0xc9, 0xcd, 0xd3, 0xda, 0xfc, 0xdc, 0xbf, 0xa1, 0xba, 0xaa, 0xe7, 0xc7, 0x20ac]);
        let mut k = Keys { kb, map, mods: 0, down: vec![], _fd: None };
        k.upload()?;
        Ok(k)
    }

    fn upload(&mut self) -> Result<(), Err> {
        let mut t = String::from("xkb_keymap {\nxkb_keycodes \"pantalla\" {\n minimum = 8;\n maximum = 255;\n");
        for i in 0..self.map.len() {
            t += &format!(" <K{}> = {};\n", i + 9, i + 9);
        }
        t += "};\nxkb_types \"pantalla\" { include \"complete\" };\nxkb_compatibility \"pantalla\" { include \"complete\" };\nxkb_symbols \"pantalla\" {\n";
        for (i, s) in self.map.iter().enumerate() {
            t += &format!(" key <K{}> {{ [ 0x{:x} ] }};\n", i + 9, s);
        }
        for (name, bit) in [("Shift", MOD_SHIFT), ("Control", MOD_CTRL), ("Mod1", MOD_ALT), ("Mod4", MOD_SUPER), ("Mod5", MOD_ALTGR)] {
            let keys: Vec<String> = self.map.iter().enumerate().filter(|(_, s)| mod_of(**s) == bit).map(|(i, _)| format!("<K{}>", i + 9)).collect();
            if !keys.is_empty() {
                t += &format!(" modifier_map {name} {{ {} }};\n", keys.join(", "));
            }
        }
        t += "};\n};\n";
        let mut bytes = t.into_bytes();
        bytes.push(0);
        let fd = memfd("pantalla-teclado", bytes.len())?;
        let mut f = std::fs::File::from(fd.try_clone()?);
        std::io::Write::write_all(&mut f, &bytes)?;
        self.kb.keymap(1, fd.as_fd(), bytes.len() as u32); // 1 = XKB_V1
        self._fd = Some(fd);
        Ok(())
    }

    fn code(&mut self, sym: u32) -> Result<u32, Err> {
        if let Some(i) = self.map.iter().position(|s| *s == sym) {
            return Ok(i as u32 + 1); // evdev = xkb - 8 = (i + 9) - 8
        }
        if self.map.len() >= 246 {
            // lleno: se quitan los añadidos sobre la marcha (se conservan los fijos)
            self.map.truncate(150);
        }
        self.map.push(sym);
        self.upload()?;
        Ok(self.map.len() as u32)
    }

    fn key(&mut self, t: u32, sym: u32, down: bool) -> Result<(), Err> {
        let code = self.code(sym)?;
        self.kb.key(t, code, if down { 1 } else { 0 });
        if down {
            self.down.push(code);
        } else {
            self.down.retain(|c| *c != code);
        }
        let m = mod_of(sym);
        if m != 0 {
            if down { self.mods |= m } else { self.mods &= !m }
            self.kb.modifiers(self.mods, 0, 0, 0);
        }
        Ok(())
    }

    fn release_all(&mut self, t: u32) {
        for c in std::mem::take(&mut self.down) {
            self.kb.key(t, c, 0);
        }
        self.mods = 0;
        self.kb.modifiers(0, 0, 0, 0);
    }
}

fn input_loop(rx: Receiver<Input>, w: u32, h: u32, stop: &AtomicBool) -> Result<(), Err> {
    let (conn, mut q, globals, mut s, output) = connect_with_output(Inp::default, |s| &mut s.outputs)?;
    let qh = q.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ())?;
    let pm: vpm::ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ())?;
    let ptr = if pm.version() >= 2 {
        pm.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ())
    } else {
        pm.create_virtual_pointer(Some(&seat), &qh, ())
    };
    let km: vkm::ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let mut keys = Keys::new(km.create_virtual_keyboard(&seat, &qh, ()))?;
    conn.flush()?;

    let t0 = Instant::now();
    let ms = || t0.elapsed().as_millis() as u32;
    let mut prev = 0u8;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Input::Pointer(x, y, mask)) => {
                let t = ms();
                ptr.motion_absolute(t, (x as u32).min(w - 1), (y as u32).min(h - 1), w, h);
                for (bit, btn) in [(1u8, 0x110u32), (2, 0x112), (4, 0x111)] {
                    if (mask ^ prev) & bit != 0 {
                        let st = if mask & bit != 0 { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
                        ptr.button(t, btn, st);
                    }
                }
                for (bit, axis, dir) in [
                    (8u8, wl_pointer::Axis::VerticalScroll, -1),
                    (16, wl_pointer::Axis::VerticalScroll, 1),
                    (32, wl_pointer::Axis::HorizontalScroll, -1),
                    (64, wl_pointer::Axis::HorizontalScroll, 1),
                ] {
                    if mask & bit != 0 && prev & bit == 0 {
                        ptr.axis_source(wl_pointer::AxisSource::Wheel);
                        ptr.axis_discrete(t, axis, 15.0 * dir as f64, dir);
                    }
                }
                ptr.frame();
                prev = mask;
            }
            Ok(Input::Key(sym, down)) => keys.key(ms(), sym, down)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        dispatch_timeout(&mut q, &mut s, 0)?;
    }
    keys.release_all(ms());
    if prev & 7 != 0 {
        for btn in [0x110u32, 0x111, 0x112] {
            ptr.button(ms(), btn, wl_pointer::ButtonState::Released);
        }
        ptr.frame();
    }
    conn.flush()?;
    Ok(())
}
