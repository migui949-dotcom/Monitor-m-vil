//! Windows 11: la cámara del móvil como webcam «Cámara del móvil» para Meet, Teams, Discord…
//!
//! · `pantalla-camara.dll` (camara-win/, va dentro de este .exe) es una fuente de Media Foundation
//!   que carga el servicio de cámaras de Windows. Se registra una vez para todo el equipo (permiso
//!   de administrador al instalar) en %ProgramFiles%\Pantalla.
//! · Este programa crea la cámara con MFCreateVirtualCamera (mientras está en marcha), decodifica el
//!   H.264 del móvil (camimg.rs, igual que en Linux), lo gira y deja NV12 1280x720 en la memoria
//!   compartida `Global\PantallaCamaraMovil` que crea la DLL.
//! · Automático: la DLL apunta cuándo le pidieron la última imagen; si una app está usando la cámara
//!   se enciende la del móvil, y si nadie la usa en 10 s, se apaga (como en Linux).

use crate::camimg::{has_idr, to_webcam, CHUNK, FRAME, H, W};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::{w, Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_ALL_ACCESS};
use windows::Win32::System::SystemInformation::GetTickCount64;

pub const CLSID: &str = "{7A1E5C3B-4D2F-4B8A-9C61-2E5F0D8A7B14}";
const NAME: &str = "Cámara del móvil";
/// La DLL compilada (vacía si se compiló sin ella: entonces no hay cámara)
static DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pantalla_camara.dll"));

static ROT: AtomicI32 = AtomicI32::new(0);
static GEN: AtomicU64 = AtomicU64::new(0);
static FEED: Mutex<Option<SyncSender<(u64, Vec<u8>)>>> = Mutex::new(None);
static READY: AtomicBool = AtomicBool::new(false);
static FPS: Mutex<(u32, Option<Instant>, f32)> = Mutex::new((0, None, 0.0));

/// Número de compilación de Windows (22000 o más = Windows 11)
fn build() -> u32 {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 32];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"), w!("CurrentBuildNumber"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len))
    };
    if r.is_err() {
        return 0;
    }
    String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap_or(0)]).parse().unwrap_or(0)
}

/// Dónde está registrada la DLL (si lo está y el fichero existe)
fn registered_path() -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let key: Vec<u16> = format!("SOFTWARE\\Classes\\CLSID\\{CLSID}\\InprocServer32\0").encode_utf16().collect();
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(key.as_ptr()), PCWSTR::null(), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len)) };
    if r.is_err() {
        return None;
    }
    let p = String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap_or(0)]);
    std::path::Path::new(&p).exists().then_some(p)
}

/// Por qué no hay cámara (None = la hay o la puede haber)
pub fn unavailable() -> Option<String> {
    if DLL.is_empty() {
        return Some("Esta versión se compiló sin la cámara para Windows".into());
    }
    if build() < 22000 {
        return Some("La cámara del móvil en Windows necesita Windows 11".into());
    }
    if registered_path().is_none() {
        return Some("Falta instalar la cámara: vuelve a abrir pantalla-host.exe y acepta el permiso de administrador".into());
    }
    None
}

/// ¿Hace falta registrar la DLL (Windows 11, no registrada o de otra versión)?
pub fn needs_register() -> bool {
    !DLL.is_empty() && build() >= 22000 && registered_path().map(|p| !p.ends_with(&dll_name())).unwrap_or(true)
}

fn dll_name() -> String {
    // con la versión en el nombre: la vieja puede seguir cargada en el servicio de cámaras
    format!("pantalla-camara-{}.dll", env!("CARGO_PKG_VERSION"))
}

fn reg(args: &[&str]) -> bool {
    std::process::Command::new("reg").args(args).status().map(|s| s.success()).unwrap_or(false)
}

/// `pantalla-host registrar-camara` (como administrador): copia la DLL y la registra para el equipo
pub fn register() -> Result<(), String> {
    if DLL.is_empty() {
        return Err("esta versión no lleva la cámara".into());
    }
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let dir = std::path::PathBuf::from(pf).join("Pantalla");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(dll_name());
    if !path.exists() {
        std::fs::write(&path, DLL).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // las de versiones anteriores sobran (si alguna sigue cargada, se queda hasta reiniciar)
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("pantalla-camara") && n != dll_name() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let key = format!(r"HKLM\SOFTWARE\Classes\CLSID\{CLSID}");
    let p = path.to_string_lossy().to_string();
    let ok = reg(&["add", &key, "/ve", "/d", "Pantalla - cámara del móvil", "/f", "/reg:64"])
        && reg(&["add", &format!(r"{key}\InprocServer32"), "/ve", "/d", &p, "/f", "/reg:64"])
        && reg(&["add", &format!(r"{key}\InprocServer32"), "/v", "ThreadingModel", "/d", "Both", "/f", "/reg:64"]);
    if !ok {
        return Err("no se pudo registrar la cámara".into());
    }
    Ok(())
}

/// `pantalla-host quitar-camara` (como administrador)
pub fn unregister() {
    reg(&["delete", &format!(r"HKLM\SOFTWARE\Classes\CLSID\{CLSID}"), "/f", "/reg:64"]);
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let dir = std::path::PathBuf::from(pf).join("Pantalla");
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let _ = std::fs::remove_file(e.path());
    }
    let _ = std::fs::remove_dir(&dir);
}

/// Ejecuta este programa con `args` como administrador (aviso de Windows) y espera; true = salió bien
pub fn elevated(args: &str) -> bool {
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let Ok(me) = std::env::current_exe() else { return false };
    let exe: Vec<u16> = format!("{}\0", me.display()).encode_utf16().collect();
    let params: Vec<u16> = format!("{args}\0").encode_utf16().collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: 0,
        ..Default::default()
    };
    unsafe {
        if ShellExecuteExW(&mut info).is_err() || info.hProcess.is_invalid() {
            return false; // no ha aceptado el permiso
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code == 0
    }
}

// ───────────── En marcha ─────────────

/// Al arrancar el servicio: crea la cámara (si se puede) y los hilos que la alimentan
pub fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(why) = unavailable() {
        crate::host::log(&format!("cámara: {why}"));
        return;
    }
    let (ftx, frx) = sync_channel::<(u64, Vec<u8>)>(256);
    let (itx, irx) = sync_channel::<Vec<u8>>(1);
    *FEED.lock().unwrap() = Some(ftx);
    std::thread::Builder::new().name("camara-virtual".into()).spawn(virtual_camera).ok();
    std::thread::Builder::new().name("camara".into()).spawn(move || decoder(frx, itx)).ok();
    std::thread::Builder::new().name("webcam".into()).spawn(move || writer(irx)).ok();
    std::thread::Builder::new().name("auto".into()).spawn(auto).ok();
}

pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

pub fn set_rot(deg: i32) {
    ROT.store(deg, Ordering::Relaxed);
}

pub fn cam_off() {
    GEN.fetch_add(1, Ordering::Relaxed);
    *FPS.lock().unwrap() = (0, None, 0.0);
}

pub fn fps() -> f32 {
    FPS.lock().unwrap().2
}

/// Un trozo del canal 4 (H.264 Annex B)
pub fn feed(data: &[u8]) {
    if let Some(tx) = FEED.lock().unwrap().as_ref() {
        let _ = tx.try_send((GEN.load(Ordering::Relaxed), data.to_vec()));
    }
}

/// `pantalla-host probar-camara`: crea la cámara, lista las cámaras del sistema y lee imágenes de
/// «Cámara del móvil» como lo haría una app. Escribe el resultado en %TEMP%\probar-camara.txt.
pub fn self_test(solo_leer: bool) {
    let mut out = String::new();
    let r = (|| -> Result<(), String> {
        use windows::Win32::Media::MediaFoundation::*;
        out += &format!("Windows build {} · {}\n", build(), unavailable().unwrap_or_else(|| "cámara disponible".into()));
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))?;
        }
        // con el servicio en marcha la cámara ya existe: solo se lee
        let cam = if solo_leer { None } else { Some(create_camera()?) };
        if cam.is_some() {
            out += "MFCreateVirtualCamera + Start: OK\n";
        }
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1).map_err(|e| e.to_string())?;
            let attrs = attrs.unwrap();
            attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(|e| e.to_string())?;
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut n = 0u32;
            MFEnumDeviceSources(&attrs, &mut list, &mut n).map_err(|e| format!("MFEnumDeviceSources: {e}"))?;
            let mut mine = None;
            for i in 0..n as usize {
                if let Some(a) = (*list.add(i)).take() {
                    let mut p = windows::core::PWSTR::null();
                    let mut l = 0u32;
                    let name = if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut l).is_ok() { p.to_string().unwrap_or_default() } else { "?".into() };
                    out += &format!("cámara: {name}\n");
                    if name.starts_with(NAME) {
                        mine = Some(a);
                    }
                }
            }
            let a = mine.ok_or("«Cámara del móvil» no aparece en la lista")?;
            let src: IMFMediaSource = a.ActivateObject().map_err(|e| format!("activar: {e}"))?;
            let reader = MFCreateSourceReaderFromMediaSource(&src, None).map_err(|e| format!("lector: {e}"))?;
            let t0 = Instant::now();
            let (mut frames, mut last) = (0, Vec::new());
            while t0.elapsed() < Duration::from_secs(if solo_leer { 12 } else { 4 }) {
                let (mut flags, mut ts) = (0u32, 0i64);
                let mut sample = None;
                reader.ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).map_err(|e| format!("ReadSample: {e}"))?;
                if let Some(s) = sample {
                    frames += 1;
                    let b = s.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
                    let mut p: *mut u8 = std::ptr::null_mut();
                    let mut len = 0u32;
                    b.Lock(&mut p, None, Some(&mut len)).map_err(|e| e.to_string())?;
                    last = std::slice::from_raw_parts(p, len as usize).to_vec();
                    let _ = b.Unlock();
                }
            }
            out += &format!("leídas {frames} imágenes en {:.0} s ({} bytes la última)\n", t0.elapsed().as_secs_f32(), last.len());
            let f = std::env::temp_dir().join("camara.nv12");
            let _ = std::fs::write(&f, &last);
            out += &format!("última imagen: {}\n", f.display());
            let _ = src.Shutdown();
        }
        drop(cam);
        Ok(())
    })();
    if let Err(e) = r {
        out += &format!("ERROR: {e}\n");
    }
    let _ = std::fs::write(std::env::temp_dir().join("probar-camara.txt"), out);
}

type CreateFn = unsafe extern "system" fn(i32, i32, i32, PCWSTR, PCWSTR, *const GUID, u32, *mut *mut std::ffi::c_void) -> HRESULT;

fn create_camera() -> Result<windows::Win32::Media::MediaFoundation::IMFVirtualCamera, String> {
    use windows::Win32::Media::MediaFoundation::{IMFVirtualCamera, MFStartup, MFSTARTUP_FULL, MF_VERSION};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))?;
            let lib = LoadLibraryW(w!("mfsensorgroup.dll")).map_err(|e| format!("mfsensorgroup.dll: {e}"))?;
            let f = GetProcAddress(lib, windows::core::s!("MFCreateVirtualCamera")).ok_or("falta MFCreateVirtualCamera (¿Windows 10?)")?;
            let create: CreateFn = std::mem::transmute(f);
            let name: Vec<u16> = format!("{NAME}\0").encode_utf16().collect();
            let id: Vec<u16> = format!("{CLSID}\0").encode_utf16().collect();
            let mut p = std::ptr::null_mut();
            // software, mientras dure esta sesión del programa, para el usuario actual
            create(0, 0, 0, PCWSTR(name.as_ptr()), PCWSTR(id.as_ptr()), std::ptr::null(), 0, &mut p).ok().map_err(|e| format!("MFCreateVirtualCamera: {e}"))?;
            let cam = IMFVirtualCamera::from_raw(p);
            cam.Start(None).map_err(|e| format!("Start: {e}"))?;
            Ok(cam)
        }
    }
}

/// Crea «Cámara del móvil» y la mantiene mientras el programa está en marcha
fn virtual_camera() {
    match create_camera() {
        Ok(cam) => {
            READY.store(true, Ordering::Relaxed);
            crate::host::log(&format!("cámara: «{NAME}» lista"));
            loop {
                std::thread::sleep(Duration::from_secs(3600));
                let _ = &cam;
            }
        }
        Err(e) => crate::host::log(&format!("cámara: {e}")),
    }
}

fn decoder(rx: Receiver<(u64, Vec<u8>)>, out: SyncSender<Vec<u8>>) {
    let mut dec: Option<openh264::decoder::Decoder> = None;
    let (mut gen, mut buf, mut synced) = (u64::MAX, Vec::<u8>::new(), false);
    while let Ok((g, chunk)) = rx.recv() {
        if g != gen || dec.is_none() {
            gen = g;
            dec = openh264::decoder::Decoder::new().ok();
            buf.clear();
            synced = false;
        }
        buf.extend_from_slice(&chunk);
        // trozo más corto que CHUNK = fin de la imagen
        if chunk.len() >= CHUNK && buf.len() < 8 << 20 {
            continue;
        }
        let frame = std::mem::take(&mut buf);
        if !synced {
            if !has_idr(&frame) {
                continue;
            }
            synced = true;
        }
        let Some(d) = dec.as_mut() else { continue };
        let mut img_out = None;
        for nal in openh264::nal_units(&frame) {
            match d.decode(nal) {
                Ok(Some(img)) => {
                    use openh264::formats::YUVSource;
                    let (w, h) = img.dimensions();
                    img_out = Some(to_webcam(img.y(), img.u(), img.v(), img.strides(), w, h, ROT.load(Ordering::Relaxed)));
                }
                Ok(None) => {}
                Err(_) => synced = false,
            }
        }
        if let Some(i420) = img_out {
            let mut fps = FPS.lock().unwrap();
            fps.0 += 1;
            let t0 = *fps.1.get_or_insert_with(Instant::now);
            if t0.elapsed() >= Duration::from_secs(2) {
                fps.2 = fps.0 as f32 / t0.elapsed().as_secs_f32();
                *fps = (0, Some(Instant::now()), fps.2);
            }
            drop(fps);
            let _ = out.try_send(i420_to_nv12(&i420)); // la webcam va atrasada: se salta
        }
    }
}

fn i420_to_nv12(f: &[u8]) -> Vec<u8> {
    let (y, rest) = f.split_at(W * H);
    let (u, v) = rest.split_at(W * H / 4);
    let mut o = Vec::with_capacity(FRAME);
    o.extend_from_slice(y);
    for i in 0..W * H / 4 {
        o.push(u[i]);
        o.push(v[i]);
    }
    o
}

/// Memoria compartida con la DLL (la crea ella al cargarla el servicio de cámaras)
struct Shm(*mut u8, #[allow(dead_code)] HANDLE); // (el handle se guarda para que la memoria siga abierta)
unsafe impl Send for Shm {}

const OFF_SEQ: usize = 16;
const OFF_HOST: usize = 24;
const OFF_READER: usize = 32;
const MAGIC: u32 = 0x4d41_4350;

fn open_shm() -> Option<Shm> {
    unsafe {
        let h = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, w!("Global\\PantallaCamaraMovil")).ok()?;
        let v = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 64 + FRAME);
        if v.Value.is_null() {
            let _ = CloseHandle(h);
            return None;
        }
        let p = v.Value as *mut u8;
        if *(p as *const u32) != MAGIC {
            return None;
        }
        Some(Shm(p, h))
    }
}

fn at(p: *mut u8, off: usize) -> &'static AtomicU64 {
    unsafe { &*(p.add(off) as *const AtomicU64) }
}

static SHM: Mutex<Option<Shm>> = Mutex::new(None);

fn shm_ptr() -> Option<*mut u8> {
    let mut g = SHM.lock().unwrap();
    if g.is_none() {
        *g = open_shm();
    }
    g.as_ref().map(|s| s.0)
}

fn writer(rx: Receiver<Vec<u8>>) {
    while let Ok(f) = rx.recv() {
        let Some(p) = shm_ptr() else { continue }; // nadie ha abierto la cámara aún
        let seq = at(p, OFF_SEQ);
        let s = seq.load(Ordering::Relaxed);
        seq.store(s | 1, Ordering::Release); // impar: escribiendo
        unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), p.add(64), FRAME) };
        seq.store((s | 1) + 1, Ordering::Release);
        at(p, OFF_HOST).store(unsafe { GetTickCount64() }, Ordering::Release);
    }
}

/// ¿Ha pedido imágenes alguna app hace poco?
fn in_use() -> bool {
    let Some(p) = shm_ptr() else { return false };
    let t = at(p, OFF_READER).load(Ordering::Relaxed);
    t != 0 && unsafe { GetTickCount64() }.saturating_sub(t) < 2500
}

fn get(path: &str) -> Option<serde_json::Value> {
    use std::io::{Read, Write};
    let port = crate::util::env_port("PANTALLA_MEDIA_PORT", 6083);
    let mut s = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\n\r\n").ok()?;
    let mut r = String::new();
    s.read_to_string(&mut r).ok()?;
    serde_json::from_str(r.split_once("\r\n\r\n")?.1).ok()
}

/// Si una app usa la cámara, se enciende la del móvil; si nadie la usa en 10 s, se apaga
fn auto() {
    let (mut session, mut on, mut idle_since) = (0u64, false, None::<Instant>);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let Some((slot, mark)) = crate::host::session_marks().into_iter().next() else {
            on = false;
            continue;
        };
        if mark != session {
            session = mark;
            on = false;
            idle_since = None;
        }
        if in_use() {
            idle_since = None;
            if !on {
                if let Some(state) = get(&format!("/state?s={slot}")) {
                    if !state["cam"].as_bool().unwrap_or(false) && get(&format!("/set?cam=1&auto=1&s={slot}")).is_some() {
                        on = true;
                        crate::host::log("una app usa la cámara del móvil: encendida");
                    }
                }
            }
        } else if on {
            let t = *idle_since.get_or_insert_with(Instant::now);
            if t.elapsed() > Duration::from_secs(10) {
                let _ = get(&format!("/set?cam=0&s={slot}"));
                on = false;
                idle_since = None;
            }
        }
    }
}

#[allow(dead_code)]
fn _unused(i: &windows::core::IUnknown) -> windows::core::Result<windows::core::IUnknown> {
    i.cast()
}
