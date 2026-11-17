//! Recibir un flujo de vídeo de PipeWire (lo usa KDE para el monitor virtual).
//!
//! libpipewire se carga con dlopen al usarse: el binario no depende de ella para arrancar (sigue
//! funcionando en sistemas sin PipeWire) y no hacen falta cabeceras ni bindgen para compilar.
//! Se piden imágenes BGRx/BGRA en memoria compartida (memfd), sin GPU, como en Hyprland.

use crate::display::{Frame, FrameSlot, PixelOrder};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Arc;

type Ptr = *mut c_void;

/// Funciones de libpipewire que se usan (firmas de pipewire/*.h, 0.3 / 1.x)
struct Lib {
    _h: Ptr,
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    tl_new: unsafe extern "C" fn(*const c_char, Ptr) -> Ptr,
    tl_get_loop: unsafe extern "C" fn(Ptr) -> Ptr,
    tl_start: unsafe extern "C" fn(Ptr) -> c_int,
    tl_stop: unsafe extern "C" fn(Ptr),
    tl_destroy: unsafe extern "C" fn(Ptr),
    tl_lock: unsafe extern "C" fn(Ptr),
    tl_unlock: unsafe extern "C" fn(Ptr),
    ctx_new: unsafe extern "C" fn(Ptr, Ptr, usize) -> Ptr,
    ctx_connect: unsafe extern "C" fn(Ptr, Ptr, usize) -> Ptr,
    ctx_destroy: unsafe extern "C" fn(Ptr),
    core_disconnect: unsafe extern "C" fn(Ptr) -> c_int,
    stream_new: unsafe extern "C" fn(Ptr, *const c_char, Ptr) -> Ptr,
    stream_add_listener: unsafe extern "C" fn(Ptr, Ptr, *const Events, Ptr),
    stream_connect: unsafe extern "C" fn(Ptr, u32, u32, u32, *mut *const u8, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(Ptr, *mut *const u8, u32) -> c_int,
    stream_dequeue: unsafe extern "C" fn(Ptr) -> *mut PwBuffer,
    stream_queue: unsafe extern "C" fn(Ptr, *mut PwBuffer) -> c_int,
    stream_destroy: unsafe extern "C" fn(Ptr),
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

fn lib() -> Result<&'static Lib, String> {
    static L: std::sync::OnceLock<Result<Lib, String>> = std::sync::OnceLock::new();
    L.get_or_init(|| unsafe { load() }).as_ref().map_err(|e| e.clone())
}

unsafe fn load() -> Result<Lib, String> {
    let h = libc::dlopen(c"libpipewire-0.3.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if h.is_null() {
        return Err("no está libpipewire-0.3 (hace falta PipeWire)".into());
    }
    macro_rules! sym {
        ($n:literal) => {{
            let p = libc::dlsym(h, concat!($n, "\0").as_ptr() as *const c_char);
            if p.is_null() {
                return Err(format!("libpipewire sin {}", $n));
            }
            std::mem::transmute(p)
        }};
    }
    let l = Lib {
        _h: h,
        init: sym!("pw_init"),
        tl_new: sym!("pw_thread_loop_new"),
        tl_get_loop: sym!("pw_thread_loop_get_loop"),
        tl_start: sym!("pw_thread_loop_start"),
        tl_stop: sym!("pw_thread_loop_stop"),
        tl_destroy: sym!("pw_thread_loop_destroy"),
        tl_lock: sym!("pw_thread_loop_lock"),
        tl_unlock: sym!("pw_thread_loop_unlock"),
        ctx_new: sym!("pw_context_new"),
        ctx_connect: sym!("pw_context_connect"),
        ctx_destroy: sym!("pw_context_destroy"),
        core_disconnect: sym!("pw_core_disconnect"),
        stream_new: sym!("pw_stream_new"),
        stream_add_listener: sym!("pw_stream_add_listener"),
        stream_connect: sym!("pw_stream_connect"),
        stream_update_params: sym!("pw_stream_update_params"),
        stream_dequeue: sym!("pw_stream_dequeue_buffer"),
        stream_queue: sym!("pw_stream_queue_buffer"),
        stream_destroy: sym!("pw_stream_destroy"),
    };
    (l.init)(std::ptr::null_mut(), std::ptr::null_mut());
    Ok(l)
}

// ── Estructuras C (spa/buffer/buffer.h, pipewire/stream.h) ──

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
}
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: Ptr,
    datas: *mut SpaData,
}
#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: Ptr,
    chunk: *mut SpaChunk,
}
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

/// struct pw_stream_events, versión 2 (0x60 bytes)
#[repr(C)]
struct Events {
    version: u32,
    destroy: Option<unsafe extern "C" fn(Ptr)>,
    state_changed: Option<unsafe extern "C" fn(Ptr, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(Ptr, u32, Ptr)>,
    io_changed: Option<unsafe extern "C" fn(Ptr, u32, Ptr, u32)>,
    param_changed: Option<unsafe extern "C" fn(Ptr, u32, *const u8)>,
    add_buffer: Option<unsafe extern "C" fn(Ptr, Ptr)>,
    remove_buffer: Option<unsafe extern "C" fn(Ptr, Ptr)>,
    process: Option<unsafe extern "C" fn(Ptr)>,
    drained: Option<unsafe extern "C" fn(Ptr)>,
    command: Option<unsafe extern "C" fn(Ptr, Ptr)>,
    trigger_done: Option<unsafe extern "C" fn(Ptr)>,
}

static EVENTS: Events = Events {
    version: 2,
    destroy: None,
    state_changed: Some(on_state),
    control_info: None,
    io_changed: None,
    param_changed: Some(on_param),
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

// ── SPA POD (formato binario de los parámetros) ──

const T_ID: u32 = 3;
const T_INT: u32 = 4;
const T_RECT: u32 = 10;
const T_OBJECT: u32 = 15;
const T_CHOICE: u32 = 19;
const OBJ_FORMAT: u32 = 0x40003;
const OBJ_BUFFERS: u32 = 0x40004;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_FORMAT: u32 = 4;
const PARAM_BUFFERS: u32 = 5;
const BUFFERS_DATATYPE: u32 = 6;
const F_MEDIA_TYPE: u32 = 1;
const F_MEDIA_SUBTYPE: u32 = 2;
const F_VIDEO_FORMAT: u32 = 0x20001;
const F_VIDEO_SIZE: u32 = 0x20003;
const MEDIA_VIDEO: u32 = 2;
const SUBTYPE_RAW: u32 = 1;
const VF_RGBX: u32 = 7;
const VF_BGRX: u32 = 8;
const VF_RGBA: u32 = 11;
const VF_BGRA: u32 = 12;
const CHOICE_ENUM: u32 = 3;
const CHOICE_FLAGS: u32 = 4;
const DATA_MEMPTR: u32 = 1;
const DATA_MEMFD: u32 = 2;

fn w32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_ne_bytes());
}
fn pad8(v: &mut Vec<u8>) {
    while v.len() % 8 != 0 {
        v.push(0);
    }
}
/// pod = tamaño del cuerpo + tipo + cuerpo (rellenado a 8)
fn pod(t: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    w32(&mut v, body.len() as u32);
    w32(&mut v, t);
    v.extend_from_slice(body);
    pad8(&mut v);
    v
}
fn id(x: u32) -> Vec<u8> {
    pod(T_ID, &x.to_ne_bytes())
}
fn choice(kind: u32, child: u32, vals: &[u32]) -> Vec<u8> {
    let mut b = Vec::new();
    w32(&mut b, kind);
    w32(&mut b, 0);
    w32(&mut b, 4); // tamaño de cada valor
    w32(&mut b, child);
    for x in vals {
        w32(&mut b, *x);
    }
    pod(T_CHOICE, &b)
}
fn object(otype: u32, oid: u32, props: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut b = Vec::new();
    w32(&mut b, otype);
    w32(&mut b, oid);
    for (k, v) in props {
        w32(&mut b, *k);
        w32(&mut b, 0);
        b.extend_from_slice(v);
    }
    pod(T_OBJECT, &b)
}

fn enum_format(w: u32, h: u32) -> Vec<u8> {
    let mut rect = Vec::new();
    w32(&mut rect, w);
    w32(&mut rect, h);
    object(OBJ_FORMAT, PARAM_ENUM_FORMAT, &[
        (F_MEDIA_TYPE, id(MEDIA_VIDEO)),
        (F_MEDIA_SUBTYPE, id(SUBTYPE_RAW)),
        // primer valor = preferido; sin "modifier" → KWin manda memoria compartida, no dmabuf
        (F_VIDEO_FORMAT, choice(CHOICE_ENUM, T_ID, &[VF_BGRX, VF_BGRX, VF_BGRA, VF_RGBX, VF_RGBA])),
        (F_VIDEO_SIZE, pod(T_RECT, &rect)),
    ])
}

fn buffers_param() -> Vec<u8> {
    object(OBJ_BUFFERS, PARAM_BUFFERS, &[(BUFFERS_DATATYPE, choice(CHOICE_FLAGS, T_INT, &[(1 << DATA_MEMPTR) | (1 << DATA_MEMFD)]))])
}

/// Lee de un objeto Format el formato de vídeo y el tamaño (lo que KWin ha elegido)
unsafe fn parse_format(p: *const u8) -> (Option<u32>, Option<(u32, u32)>) {
    let rd = |o: usize| u32::from_ne_bytes(*(p.add(o) as *const [u8; 4]));
    let (size, t) = (rd(0) as usize, rd(4));
    let (mut fmt, mut wh) = (None, None);
    if t != T_OBJECT {
        return (fmt, wh);
    }
    let mut o = 16; // cabecera (8) + tipo y id del objeto (8)
    while o + 16 <= 8 + size {
        let key = rd(o);
        let vsize = rd(o + 8) as usize;
        let vtype = rd(o + 12);
        let body = o + 16;
        match (key, vtype) {
            (F_VIDEO_FORMAT, T_ID) => fmt = Some(rd(body)),
            (F_VIDEO_FORMAT, T_CHOICE) => fmt = Some(rd(body + 16)),
            (F_VIDEO_SIZE, T_RECT) => wh = Some((rd(body), rd(body + 4))),
            _ => {}
        }
        o = body + ((vsize + 7) & !7);
    }
    (fmt, wh)
}

// ── Flujo ──

struct State {
    lib: &'static Lib,
    stream: Ptr,
    slot: Arc<FrameSlot>,
    order: PixelOrder,
    size: (u32, u32),
}

unsafe extern "C" fn on_state(_d: Ptr, _old: c_int, new: c_int, err: *const c_char) {
    // -1 error, 0 unconnected, 1 connecting, 2 paused, 3 streaming
    let e = if err.is_null() { String::new() } else { CStr::from_ptr(err).to_string_lossy().into_owned() };
    crate::host::log(&format!("pipewire: estado {new}{}", if e.is_empty() { String::new() } else { format!(" ({e})") }));
}

unsafe extern "C" fn on_param(d: Ptr, id: u32, param: *const u8) {
    if id != PARAM_FORMAT || param.is_null() {
        return;
    }
    let s = &mut *(d as *mut State);
    let (fmt, wh) = parse_format(param);
    s.order = match fmt {
        Some(VF_RGBX) | Some(VF_RGBA) => PixelOrder::Rgbx,
        _ => PixelOrder::Bgrx,
    };
    if let Some(wh) = wh {
        s.size = wh;
    }
    crate::host::log(&format!("pipewire: formato {fmt:?} {:?}", s.size));
    let b = buffers_param();
    let mut params = [b.as_ptr()];
    (s.lib.stream_update_params)(s.stream, params.as_mut_ptr(), 1);
}

unsafe extern "C" fn on_process(d: Ptr) {
    let s = &mut *(d as *mut State);
    // el más reciente: se devuelven los que estén por delante
    let mut last: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let b = (s.lib.stream_dequeue)(s.stream);
        if b.is_null() {
            break;
        }
        if !last.is_null() {
            (s.lib.stream_queue)(s.stream, last);
        }
        last = b;
    }
    if last.is_null() {
        return;
    }
    let sb = &*(*last).buffer;
    if sb.n_datas > 0 {
        let data = &*sb.datas;
        let chunk = &*data.chunk;
        let (w, h) = (s.size.0 as usize, s.size.1 as usize);
        let stride = if chunk.stride > 0 { chunk.stride as usize } else { w * 4 };
        let need = stride * h;
        // tamaño 0 / corrupto = solo se movió el puntero o no hubo cambios: no hay imagen nueva
        if !data.data.is_null() && chunk.flags & 1 == 0 && chunk.size as usize >= need && w > 0 {
            let off = (chunk.offset % data.maxsize.max(1)) as usize;
            let src = std::slice::from_raw_parts((data.data as *const u8).add(off), need);
            s.slot.publish(Frame { width: w, height: h, stride, order: s.order, data: src.to_vec(), seq: 0 });
        }
    }
    (s.lib.stream_queue)(s.stream, last);
}

/// Un flujo de PipeWire conectado a un nodo (p. ej. el de la pantalla virtual de KDE)
pub struct Capture {
    lib: &'static Lib,
    tl: Ptr,
    ctx: Ptr,
    core: Ptr,
    stream: Ptr,
    _hook: Box<[u8; 64]>,
    state: *mut State,
}

unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Capture {
    pub fn start(node: u32, w: u32, h: u32, slot: Arc<FrameSlot>) -> Result<Capture, String> {
        let lib = lib()?;
        unsafe {
            let tl = (lib.tl_new)(c"pantalla-pw".as_ptr(), std::ptr::null_mut());
            if tl.is_null() {
                return Err("pipewire: no se pudo crear el bucle".into());
            }
            let ctx = (lib.ctx_new)((lib.tl_get_loop)(tl), std::ptr::null_mut(), 0);
            if ctx.is_null() {
                (lib.tl_destroy)(tl);
                return Err("pipewire: no se pudo crear el contexto".into());
            }
            if (lib.tl_start)(tl) < 0 {
                return Err("pipewire: no arranca el bucle".into());
            }
            (lib.tl_lock)(tl);
            let core = (lib.ctx_connect)(ctx, std::ptr::null_mut(), 0);
            if core.is_null() {
                (lib.tl_unlock)(tl);
                return Err("pipewire: no se pudo conectar (¿está el servicio de PipeWire?)".into());
            }
            let stream = (lib.stream_new)(core, c"pantalla-host".as_ptr(), std::ptr::null_mut());
            let state = Box::into_raw(Box::new(State { lib, stream, slot, order: PixelOrder::Bgrx, size: (w, h) }));
            let mut hook = Box::new([0u8; 64]);
            (lib.stream_add_listener)(stream, hook.as_mut_ptr() as Ptr, &EVENTS, state as Ptr);
            let f = enum_format(w, h);
            let mut params = [f.as_ptr()];
            // AUTOCONNECT | MAP_BUFFERS, entrada, al nodo dado
            let r = (lib.stream_connect)(stream, 0, node, 1 | 4, params.as_mut_ptr(), 1);
            (lib.tl_unlock)(tl);
            if r < 0 {
                return Err(format!("pipewire: no se pudo conectar al nodo {node} ({r})"));
            }
            Ok(Capture { lib, tl, ctx, core, stream, _hook: hook, state })
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            (self.lib.tl_lock)(self.tl);
            (self.lib.stream_destroy)(self.stream);
            (self.lib.core_disconnect)(self.core);
            (self.lib.tl_unlock)(self.tl);
            (self.lib.tl_stop)(self.tl);
            (self.lib.ctx_destroy)(self.ctx);
            (self.lib.tl_destroy)(self.tl);
            drop(Box::from_raw(self.state));
        }
    }
}

#[cfg(test)]
mod tests {
    // bytes sacados de spa_pod_builder (libspa 1.4) para el mismo formato
    #[test]
    fn pod_igual_que_spa() {
        let hex = |v: &[u8]| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let f = super::object(super::OBJ_FORMAT, super::PARAM_ENUM_FORMAT, &[
            (super::F_MEDIA_TYPE, super::id(super::MEDIA_VIDEO)),
            (super::F_MEDIA_SUBTYPE, super::id(super::SUBTYPE_RAW)),
            (super::F_VIDEO_FORMAT, super::choice(super::CHOICE_ENUM, super::T_ID, &[8, 8, 12])),
            (super::F_VIDEO_SIZE, super::pod(super::T_RECT, &[0xa0, 5, 0, 0, 0xd0, 2, 0, 0])),
        ]);
        assert_eq!(hex(&f), "800000000f000000030004000300000001000000000000000400000003000000020000000000000002000000000000000400000003000000010000000000000001000200000000001c000000130000000300000000000000040000000300000008000000080000000c000000000000000300020000000000080000000a000000a0050000d0020000");
        assert_eq!(hex(&super::buffers_param()), "300000000f000000040004000500000006000000000000001400000013000000040000000000000004000000040000000600000000000000");
    }
}
