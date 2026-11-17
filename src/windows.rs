//! Windows: captura con DXGI Desktop Duplication, ratón/teclado con SendInput, permiso con una
//! ventana Aceptar/Rechazar. Todo viene con Windows 8+; no hace falta instalar nada.
//!
//! Monitor del móvil: si hay una pantalla virtual (p. ej. "Virtual Display Driver", IddCx) se usa
//! esa → escritorio extendido. Si no, se duplica la pantalla principal. Se puede forzar con
//! OUTPUT=\\.\DISPLAY2 en %APPDATA%\Pantalla\host.conf.

use crate::display::{Display, Frame, FrameSlot, PixelOrder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

struct OutputInfo {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    name: String,
    rect: RECT,
    virtual_display: bool,
}

fn wstr(w: &[u16]) -> String {
    String::from_utf16_lossy(&w[..w.iter().position(|c| *c == 0).unwrap_or(w.len())])
}

fn dpi_aware() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

fn outputs() -> windows::core::Result<Vec<OutputInfo>> {
    let mut v = vec![];
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let adesc = wstr(&adapter.GetDesc1()?.Description);
            let lower = adesc.to_lowercase();
            let is_virtual = ["virtual display", "iddsample", "parsec", "indirect display", "usbmmidd", "spacedesk"].iter().any(|k| lower.contains(k));
            let mut oi = 0;
            while let Ok(o) = adapter.EnumOutputs(oi) {
                oi += 1;
                let d = o.GetDesc()?;
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                if let Ok(o1) = o.cast::<IDXGIOutput1>() {
                    v.push(OutputInfo { adapter: adapter.clone(), output: o1, name: wstr(&d.DeviceName), rect: d.DesktopCoordinates, virtual_display: is_virtual });
                }
            }
        }
    }
    Ok(v)
}

fn choose() -> Result<OutputInfo, String> {
    dpi_aware();
    let mut outs = outputs().map_err(|e| format!("DXGI: {e}"))?;
    if outs.is_empty() {
        return Err("no hay pantallas".into());
    }
    if let Some(want) = crate::util::conf("OUTPUT") {
        if let Some(i) = outs.iter().position(|o| o.name.eq_ignore_ascii_case(&want)) {
            return Ok(outs.swap_remove(i));
        }
    }
    if let Some(i) = outs.iter().position(|o| o.virtual_display) {
        return Ok(outs.swap_remove(i));
    }
    let i = outs.iter().position(|o| o.rect.left == 0 && o.rect.top == 0).unwrap_or(0);
    Ok(outs.swap_remove(i))
}

/// Para `pantalla-host info`.
pub fn capability() -> String {
    match choose() {
        Ok(o) if o.virtual_display => format!("sí: monitor virtual {} ({}x{})", o.name, o.rect.right - o.rect.left, o.rect.bottom - o.rect.top),
        Ok(o) => format!("duplicando {} ({}x{}); para escritorio extendido instala un driver de pantalla virtual", o.name, o.rect.right - o.rect.left, o.rect.bottom - o.rect.top),
        Err(e) => format!("no: {e}"),
    }
}

enum Input {
    Pointer(u16, u16, u8),
    Key(u32, bool),
}

struct Win {
    w: u32,
    h: u32,
    slot: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    input: Mutex<std::sync::mpsc::Sender<Input>>,
}

impl Display for Win {
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

impl Drop for Win {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn open(_w: u32, _h: u32) -> Result<Arc<dyn Display>, String> {
    let out = choose()?;
    let (w, h) = ((out.rect.right - out.rect.left) as u32, (out.rect.bottom - out.rect.top) as u32);
    crate::host::log(&format!("pantalla: {} {}x{}{}", out.name, w, h, if out.virtual_display { " (virtual)" } else { " (duplicada)" }));
    let rect = out.rect;
    let name = out.name.clone();
    let slot = Arc::new(FrameSlot::default());
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (slot, stop) = (slot.clone(), stop.clone());
        std::thread::Builder::new()
            .name("captura".into())
            .spawn(move || {
                if let Err(e) = capture_loop(&name, &slot, &stop) {
                    crate::host::log(&format!("captura: {e}"));
                }
            })
            .map_err(|e| e.to_string())?;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let stop = stop.clone();
        std::thread::Builder::new().name("entrada".into()).spawn(move || input_loop(rx, rect, &stop)).map_err(|e| e.to_string())?;
    }
    Ok(Arc::new(Win { w, h, slot, stop, input: Mutex::new(tx) }))
}

// ───────────── Captura ─────────────

fn capture_loop(name: &str, slot: &FrameSlot, stop: &AtomicBool) -> Result<(), String> {
    dpi_aware();
    let min_gap = Duration::from_millis(16);
    let mut last = Instant::now() - min_gap;
    'outer: while !stop.load(Ordering::Relaxed) {
        // se (re)crea todo: al cambiar de modo, bloquear la sesión, UAC… se pierde el acceso
        let out = outputs().map_err(|e| e.to_string())?.into_iter().find(|o| o.name == name).ok_or("la pantalla ya no está")?;
        let (device, ctx) = unsafe {
            let mut dev = None;
            let mut ctx = None;
            D3D11CreateDevice(&out.adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))
                .map_err(|e| format!("D3D11: {e}"))?;
            (dev.unwrap(), ctx.unwrap())
        };
        let dup = match unsafe { out.output.DuplicateOutput(&device) } {
            Ok(d) => d,
            Err(e) => {
                crate::dbg_log!("DuplicateOutput: {e}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let mut staging: Option<(ID3D11Texture2D, u32, u32)> = None;
        let mut first = true;
        while !stop.load(Ordering::Relaxed) {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match unsafe { dup.AcquireNextFrame(250, &mut info, &mut res) } {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => continue 'outer,
                Err(e) => {
                    crate::dbg_log!("AcquireNextFrame: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                    continue 'outer;
                }
            }
            // solo cambios del ratón (sin imagen nueva): no hace falta copiar
            if info.LastPresentTime == 0 && !first {
                unsafe { dup.ReleaseFrame().ok() };
                continue;
            }
            let frame = (|| -> windows::core::Result<Option<Frame>> {
                let tex: ID3D11Texture2D = res.as_ref().unwrap().cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut desc) };
                if staging.as_ref().map(|s| (s.1, s.2) != (desc.Width, desc.Height)).unwrap_or(true) {
                    let sd = D3D11_TEXTURE2D_DESC {
                        Width: desc.Width,
                        Height: desc.Height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: desc.Format,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut t = None;
                    unsafe { device.CreateTexture2D(&sd, None, Some(&mut t))? };
                    staging = Some((t.unwrap(), desc.Width, desc.Height));
                }
                let st = &staging.as_ref().unwrap().0;
                unsafe {
                    ctx.CopyResource(st, &tex);
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                    let (w, h, pitch) = (desc.Width as usize, desc.Height as usize, m.RowPitch as usize);
                    let src = std::slice::from_raw_parts(m.pData as *const u8, pitch * h);
                    let data = src.to_vec();
                    ctx.Unmap(st, 0);
                    Ok(Some(Frame { width: w, height: h, stride: pitch, order: PixelOrder::Bgrx, data, seq: 0 }))
                }
            })();
            unsafe { dup.ReleaseFrame().ok() };
            match frame {
                Ok(Some(f)) => {
                    first = false;
                    slot.publish(f);
                }
                Ok(None) => {}
                Err(e) => {
                    crate::dbg_log!("copia: {e}");
                    continue 'outer;
                }
            }
            let el = last.elapsed();
            if el < min_gap {
                std::thread::sleep(min_gap - el);
            }
            last = Instant::now();
        }
    }
    Ok(())
}

// ───────────── Ratón y teclado ─────────────

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// keysym X11 → (tecla virtual, ¿extendida?)
fn vk_of(sym: u32) -> Option<(u16, bool)> {
    Some(match sym {
        0xff08 => (0x08, false),
        0xff09 => (0x09, false),
        0xff0d | 0xff8d => (0x0d, false),
        0xff1b => (0x1b, false),
        0xffff => (0x2e, true),
        0xff50 => (0x24, true),
        0xff51 => (0x25, true),
        0xff52 => (0x26, true),
        0xff53 => (0x27, true),
        0xff54 => (0x28, true),
        0xff55 => (0x21, true),
        0xff56 => (0x22, true),
        0xff57 => (0x23, true),
        0xff63 => (0x2d, true),
        0xff67 => (0x5d, true),
        0xff61 => (0x2c, false),
        0xffe1 => (0xa0, false),
        0xffe2 => (0xa1, false),
        0xffe3 => (0xa2, false),
        0xffe4 => (0xa3, true),
        0xffe5 => (0x14, false),
        0xffe7 | 0xffe9 => (0xa4, false),
        0xffea | 0xfe03 => (0xa5, true),
        0xffeb => (0x5b, true),
        0xffec => (0x5c, true),
        0xffbe..=0xffc9 => (0x70 + (sym - 0xffbe) as u16, false),
        _ => return None,
    })
}

fn char_of(sym: u32) -> Option<char> {
    match sym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(sym),
        0x20ac => Some('€'),
        0x0100_0000..=0x0110_ffff => char::from_u32(sym - 0x0100_0000),
        _ => None,
    }
}

fn input_loop(rx: std::sync::mpsc::Receiver<Input>, rect: RECT, stop: &AtomicBool) {
    dpi_aware();
    let mut prev = 0u8;
    let mut held_mods = 0u32; // ctrl/alt/win pulsadas (cambia cómo se mandan las letras)
    let mut down_unicode: Vec<u32> = vec![];
    while !stop.load(Ordering::Relaxed) {
        let ev = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(e) => e,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        match ev {
            Input::Pointer(x, y, mask) => {
                let (vx, vy, vw, vh) = unsafe {
                    (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
                };
                let px = rect.left + x as i32;
                let py = rect.top + y as i32;
                let ax = ((px - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
                let ay = ((py - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
                let mut v = vec![mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)];
                for (bit, dn, up) in [(1u8, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP), (2, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP), (4, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)] {
                    if (mask ^ prev) & bit != 0 {
                        v.push(mouse(0, 0, 0, if mask & bit != 0 { dn } else { up }));
                    }
                }
                for (bit, amount, flag) in [(8u8, 120i32, MOUSEEVENTF_WHEEL), (16, -120, MOUSEEVENTF_WHEEL), (32, -120, MOUSEEVENTF_HWHEEL), (64, 120, MOUSEEVENTF_HWHEEL)] {
                    if mask & bit != 0 && prev & bit == 0 {
                        v.push(mouse(0, 0, amount as u32, flag));
                    }
                }
                send(&v);
                prev = mask;
            }
            Input::Key(sym, down) => {
                let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
                if let Some((vk, ext)) = vk_of(sym) {
                    let m = match vk {
                        0xa2 | 0xa3 => 1,
                        0xa4 | 0xa5 => 2,
                        0x5b | 0x5c => 4,
                        _ => 0,
                    };
                    if m != 0 {
                        if down { held_mods |= m } else { held_mods &= !m }
                    }
                    send(&[key_input(vk, 0, up | if ext { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) })]);
                } else if let Some(c) = char_of(sym) {
                    // con Ctrl/Alt/Win pulsada se manda la tecla de verdad (para atajos); si no, el carácter
                    let scan = unsafe { VkKeyScanW(c as u16) };
                    if held_mods != 0 && scan != -1 {
                        send(&[key_input((scan & 0xff) as u16, 0, up)]);
                    } else {
                        let mut buf = [0u16; 2];
                        for u in c.encode_utf16(&mut buf) {
                            send(&[key_input(0, *u, KEYEVENTF_UNICODE | up)]);
                        }
                        if down {
                            down_unicode.push(sym);
                        } else {
                            down_unicode.retain(|s| *s != sym);
                        }
                    }
                }
            }
        }
    }
    // suelta lo que siga pulsado
    for (bit, up) in [(1u8, MOUSEEVENTF_LEFTUP), (2, MOUSEEVENTF_MIDDLEUP), (4, MOUSEEVENTF_RIGHTUP)] {
        if prev & bit != 0 {
            send(&[mouse(0, 0, 0, up)]);
        }
    }
    for vk in [0xa2u16, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c, 0xa0, 0xa1] {
        send(&[key_input(vk, 0, KEYEVENTF_KEYUP)]);
    }
}

// ───────────── Permiso ─────────────

pub async fn ask_permission(device: &str, fp: &str, via: &str, timeout: u64) -> bool {
    let text: Vec<u16> = format!("{device} quiere conectarse a este equipo.\n\nPor {via} · huella {fp}\n\n¿Aceptar?\0").encode_utf16().collect();
    let title: Vec<u16> = "Pantalla\0".encode_utf16().collect();
    let task = tokio::task::spawn_blocking(move || unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND | MB_SYSTEMMODAL) == IDYES
    });
    matches!(tokio::time::timeout(Duration::from_secs(timeout), task).await, Ok(Ok(true)))
}

// ───────────── Utilidades ─────────────

/// Programa sin consola: si se lanza desde cmd/PowerShell, se escribe en la de quien lo lanzó.
pub fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub fn message(title: &str, text: &str) {
    let t: Vec<u16> = format!("{title}\0").encode_utf16().collect();
    let m: Vec<u16> = format!("{text}\0").encode_utf16().collect();
    unsafe {
        MessageBoxW(None, PCWSTR(m.as_ptr()), PCWSTR(t.as_ptr()), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND);
    }
}

/// ¿Se está ejecutando la copia instalada (%LOCALAPPDATA%\Pantalla)?
pub fn running_installed() -> bool {
    let Ok(me) = std::env::current_exe() else { return true };
    let Some(local) = std::env::var_os("LOCALAPPDATA") else { return true };
    let dir = std::path::PathBuf::from(local).join("Pantalla");
    me.parent().map(|p| p.eq(&dir) || p.to_string_lossy().eq_ignore_ascii_case(&dir.to_string_lossy())).unwrap_or(true)
}
