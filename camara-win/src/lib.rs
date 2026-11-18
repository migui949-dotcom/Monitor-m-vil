//! «Cámara del móvil» para Windows 11: fuente de Media Foundation que el servicio de cámaras de
//! Windows (Frame Server) carga cuando una app (Meet, Teams, Discord, la app Cámara…) abre la
//! cámara virtual que crea pantalla-host con MFCreateVirtualCamera.
//!
//! Las imágenes las deja pantalla-host (NV12 1280x720) en una memoria compartida que crea esta DLL
//! (`Global\PantallaCamaraMovil`: el servicio puede crear objetos globales y un usuario normal no).
//! Si no hay imágenes recientes se ve una imagen de espera. La DLL apunta en la memoria cuándo pidió
//! la última imagen: así pantalla-host sabe que la cámara está en uso y enciende la del móvil.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{implement, w, IUnknown, Interface, GUID, HRESULT, PCWSTR, PROPVARIANT, PWSTR};
use windows::Win32::Foundation::{BOOL, CLASS_E_CLASSNOTAVAILABLE, E_NOTIMPL, E_POINTER, HANDLE, INVALID_HANDLE_VALUE, S_FALSE, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};
use windows::Win32::System::SystemInformation::GetTickCount64;

/// CLSID de la fuente (el mismo en pantalla-host: wincam.rs)
pub const CLSID_CAMARA: GUID = GUID::from_u128(0x7a1e5c3b_4d2f_4b8a_9c61_2e5f0d8a7b14);
const PINNAME_VIDEO_CAPTURE: GUID = GUID::from_u128(0xfb6c4281_0353_11d1_905f_0000c0cc16ba);
const W: u32 = 1280;
const H: u32 = 720;
const FRAME: usize = (W * H * 3 / 2) as usize;
const FPS: u64 = 30;

// ───────────── Memoria compartida con pantalla-host ─────────────

/// Cabecera de `Global\PantallaCamaraMovil` (64 bytes; detrás, la imagen NV12)
#[repr(C)]
struct Shared {
    magic: u32, // "PCAM"
    w: u32,
    h: u32,
    _pad: u32,
    /// lo sube pantalla-host: impar mientras escribe, par cuando la imagen está completa
    seq: AtomicU64,
    /// GetTickCount64() de la última imagen que escribió pantalla-host
    host_tick: AtomicU64,
    /// GetTickCount64() de la última imagen que pidió una app (cámara en uso)
    reader_tick: AtomicU64,
    _rest: [u8; 24],
}
const MAGIC: u32 = 0x4d41_4350;
const SHM_SIZE: usize = 64 + FRAME;

struct Shm(*mut u8);
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

static SHM: Mutex<Option<Shm>> = Mutex::new(None);

fn shm() -> Option<*mut u8> {
    let mut g = SHM.lock().unwrap();
    if g.is_none() {
        unsafe {
            // todos pueden leer y escribir (pantalla-host corre como el usuario, a integridad media)
            let mut psd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("D:(A;;GA;;;WD)(A;;GA;;;AC)S:(ML;;NW;;;LW)"), SDDL_REVISION_1, &mut psd, None).ok()?;
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd.0, bInheritHandle: BOOL(0) };
            let h: HANDLE = CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&sa), PAGE_READWRITE, 0, SHM_SIZE as u32, w!("Global\\PantallaCamaraMovil")).ok()?;
            let v = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, SHM_SIZE);
            if v.Value.is_null() {
                return None;
            }
            let p = v.Value as *mut u8;
            let s = &mut *(p as *mut Shared);
            s.magic = MAGIC;
            s.w = W;
            s.h = H;
            *g = Some(Shm(p));
        }
    }
    g.as_ref().map(|s| s.0)
}

fn placeholder() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut y = vec![40u8; w * h];
    let (x0, x1, y0, y1) = (w / 2 - 150, w / 2 + 150, h / 2 - 90, h / 2 + 90);
    for r in y0..y1 {
        for c in x0..x1 {
            let inner = r >= y0 + 10 && r < y1 - 10 && c >= x0 + 14 && c < x1 - 14;
            let dot = r.abs_diff(h / 2) < 12 && c.abs_diff(w / 2) < 12;
            y[r * w + c] = if inner && !dot { 55 } else { 150 };
        }
    }
    for _ in 0..w * h / 4 {
        y.extend_from_slice(&[133, 127]);
    }
    y
}

/// La imagen más reciente de pantalla-host (o la de espera)
fn current_frame(last: &mut Vec<u8>) {
    let Some(p) = shm() else {
        if last.len() != FRAME {
            *last = placeholder();
        }
        return;
    };
    unsafe {
        let s = &*(p as *const Shared);
        let now = GetTickCount64();
        s.reader_tick.store(now, Ordering::Relaxed);
        let fresh = now.saturating_sub(s.host_tick.load(Ordering::Acquire)) < 3000;
        if !fresh {
            if last.len() != FRAME {
                *last = placeholder();
            }
            return;
        }
        let a = s.seq.load(Ordering::Acquire);
        if a & 1 == 1 || a == 0 {
            if last.len() != FRAME {
                *last = placeholder();
            }
            return;
        }
        let mut copy = vec![0u8; FRAME];
        std::ptr::copy_nonoverlapping(p.add(64), copy.as_mut_ptr(), FRAME);
        if s.seq.load(Ordering::Acquire) == a {
            *last = copy; // si cambió mientras se copiaba, se queda la anterior
        } else if last.len() != FRAME {
            *last = placeholder();
        }
    }
}

// ───────────── Fábrica (DllGetClassObject) ─────────────

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Option<&IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> windows::core::Result<()> {
        if outer.is_some() {
            return Err(windows::Win32::Foundation::CLASS_E_NOAGGREGATION.into());
        }
        let a: IMFActivate = Activator::new()?.into();
        unsafe { a.query(riid, ppv).ok() }
    }
    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

#[no_mangle]
pub extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if rclsid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    if unsafe { *rclsid } != CLSID_CAMARA {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let f: IClassFactory = Factory.into();
    unsafe { f.query(riid, ppv) }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE // el servicio de cámaras la mantiene cargada; no merece la pena llevar la cuenta
}

// ───────────── Activador (IMFActivate = atributos + crear la fuente) ─────────────

#[implement(IMFActivate)]
struct Activator {
    attrs: IMFAttributes,
}

impl Activator {
    fn new() -> windows::core::Result<Self> {
        let mut a = None;
        unsafe { MFCreateAttributes(&mut a, 4)? };
        Ok(Activator { attrs: a.unwrap() })
    }
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> windows::core::Result<()> {
        let src = MediaSource::create()?;
        unsafe { src.query(riid, ppv).ok() }
    }
    fn ShutdownObject(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn DetachObject(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// IMFAttributes que reenvía todo a un almacén de atributos de Media Foundation
macro_rules! forward_attributes {
    ($t:ty, $f:ident) => {
        impl IMFAttributes_Impl for $t {
            fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> windows::core::Result<()> { unsafe { self.$f.GetItem(k, Some(v)) } }
            fn GetItemType(&self, k: *const GUID) -> windows::core::Result<MF_ATTRIBUTE_TYPE> { unsafe { self.$f.GetItemType(k) } }
            fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> windows::core::Result<BOOL> { unsafe { self.$f.CompareItem(k, v) } }
            fn Compare(&self, t: Option<&IMFAttributes>, m: MF_ATTRIBUTES_MATCH_TYPE) -> windows::core::Result<BOOL> { unsafe { self.$f.Compare(t, m) } }
            fn GetUINT32(&self, k: *const GUID) -> windows::core::Result<u32> { unsafe { self.$f.GetUINT32(k) } }
            fn GetUINT64(&self, k: *const GUID) -> windows::core::Result<u64> { unsafe { self.$f.GetUINT64(k) } }
            fn GetDouble(&self, k: *const GUID) -> windows::core::Result<f64> { unsafe { self.$f.GetDouble(k) } }
            fn GetGUID(&self, k: *const GUID) -> windows::core::Result<GUID> { unsafe { self.$f.GetGUID(k) } }
            fn GetStringLength(&self, k: *const GUID) -> windows::core::Result<u32> { unsafe { self.$f.GetStringLength(k) } }
            fn GetString(&self, k: *const GUID, p: PWSTR, n: u32, l: *mut u32) -> windows::core::Result<()> {
                unsafe { self.$f.GetString(k, std::slice::from_raw_parts_mut(p.0, n as usize), Some(l)) }
            }
            fn GetAllocatedString(&self, k: *const GUID, p: *mut PWSTR, l: *mut u32) -> windows::core::Result<()> { unsafe { self.$f.GetAllocatedString(k, p, l) } }
            fn GetBlobSize(&self, k: *const GUID) -> windows::core::Result<u32> { unsafe { self.$f.GetBlobSize(k) } }
            fn GetBlob(&self, k: *const GUID, p: *mut u8, n: u32, l: *mut u32) -> windows::core::Result<()> {
                unsafe { self.$f.GetBlob(k, std::slice::from_raw_parts_mut(p, n as usize), Some(l)) }
            }
            fn GetAllocatedBlob(&self, k: *const GUID, p: *mut *mut u8, l: *mut u32) -> windows::core::Result<()> { unsafe { self.$f.GetAllocatedBlob(k, p, l) } }
            fn GetUnknown(&self, k: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> windows::core::Result<()> {
                unsafe {
                    let u: IUnknown = self.$f.GetUnknown(k)?;
                    u.query(riid, ppv).ok()
                }
            }
            fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> windows::core::Result<()> { unsafe { self.$f.SetItem(k, v) } }
            fn DeleteItem(&self, k: *const GUID) -> windows::core::Result<()> { unsafe { self.$f.DeleteItem(k) } }
            fn DeleteAllItems(&self) -> windows::core::Result<()> { unsafe { self.$f.DeleteAllItems() } }
            fn SetUINT32(&self, k: *const GUID, v: u32) -> windows::core::Result<()> { unsafe { self.$f.SetUINT32(k, v) } }
            fn SetUINT64(&self, k: *const GUID, v: u64) -> windows::core::Result<()> { unsafe { self.$f.SetUINT64(k, v) } }
            fn SetDouble(&self, k: *const GUID, v: f64) -> windows::core::Result<()> { unsafe { self.$f.SetDouble(k, v) } }
            fn SetGUID(&self, k: *const GUID, v: *const GUID) -> windows::core::Result<()> { unsafe { self.$f.SetGUID(k, v) } }
            fn SetString(&self, k: *const GUID, v: &PCWSTR) -> windows::core::Result<()> { unsafe { self.$f.SetString(k, *v) } }
            fn SetBlob(&self, k: *const GUID, p: *const u8, n: u32) -> windows::core::Result<()> {
                unsafe { self.$f.SetBlob(k, std::slice::from_raw_parts(p, n as usize)) }
            }
            fn SetUnknown(&self, k: *const GUID, u: Option<&IUnknown>) -> windows::core::Result<()> { unsafe { self.$f.SetUnknown(k, u) } }
            fn LockStore(&self) -> windows::core::Result<()> { unsafe { self.$f.LockStore() } }
            fn UnlockStore(&self) -> windows::core::Result<()> { unsafe { self.$f.UnlockStore() } }
            fn GetCount(&self) -> windows::core::Result<u32> { unsafe { self.$f.GetCount() } }
            fn GetItemByIndex(&self, i: u32, k: *mut GUID, v: *mut PROPVARIANT) -> windows::core::Result<()> { unsafe { self.$f.GetItemByIndex(i, k, Some(v)) } }
            fn CopyAllItems(&self, d: Option<&IMFAttributes>) -> windows::core::Result<()> { unsafe { self.$f.CopyAllItems(d) } }
        }
    };
}
forward_attributes!(Activator_Impl, attrs);

// ───────────── Fuente ─────────────

fn not_found() -> windows::core::Error {
    // HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND): «no tengo esa propiedad» (lo que esperan de IKsControl)
    HRESULT(0x8007_0492u32 as i32).into()
}

fn media_type() -> windows::core::Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((W as u64) << 32) | H as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (FPS << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, FRAME as u32)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, W)?;
        Ok(t)
    }
}

fn stream_attrs(a: &IMFAttributes) -> windows::core::Result<()> {
    unsafe {
        a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        a.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
        Ok(())
    }
}

struct SourceState {
    pd: Option<IMFPresentationDescriptor>,
    stream: Option<IMFMediaStream2>,
    stream_obj: Option<windows::core::ComObject<Stream>>,
}

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl)]
struct MediaSource {
    queue: IMFMediaEventQueue,
    attrs: IMFAttributes,
    st: Mutex<SourceState>,
}

impl MediaSource {
    fn create() -> windows::core::Result<IMFMediaSourceEx> {
        unsafe {
            let queue = MFCreateEventQueue()?;
            let mut a = None;
            MFCreateAttributes(&mut a, 4)?;
            let attrs = a.unwrap();
            let mt = media_type()?;
            let sd = MFCreateStreamDescriptor(0, &[Some(mt.clone())])?;
            sd.GetMediaTypeHandler()?.SetCurrentMediaType(&mt)?;
            stream_attrs(&sd.cast::<IMFAttributes>()?)?;
            let pd = MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))?;
            pd.SelectStream(0)?;
            let src = windows::core::ComObject::new(MediaSource { queue, attrs, st: Mutex::new(SourceState { pd: Some(pd), stream: None, stream_obj: None }) });
            let src_if: IMFMediaSourceEx = src.to_interface();
            let stream = windows::core::ComObject::new(Stream::new(src_if.cast()?, sd)?);
            {
                let mut st = src.st.lock().unwrap();
                st.stream = Some(stream.to_interface());
                st.stream_obj = Some(stream);
            }
            Ok(src_if)
        }
    }
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(&self, f: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        let q = self.queue.clone(); // sin cerrojo: GetEvent puede bloquear
        unsafe { q.GetEvent(f.0 as u32) }
    }
    fn BeginGetEvent(&self, cb: Option<&IMFAsyncCallback>, state: Option<&IUnknown>) -> windows::core::Result<()> {
        unsafe { self.queue.BeginGetEvent(cb, state) }
    }
    fn EndGetEvent(&self, r: Option<&IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(r) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> windows::core::Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, v) }
    }
}

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> windows::core::Result<u32> {
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }
    fn CreatePresentationDescriptor(&self) -> windows::core::Result<IMFPresentationDescriptor> {
        let st = self.st.lock().unwrap();
        let pd = st.pd.as_ref().ok_or(windows::core::Error::from(MF_E_SHUTDOWN))?;
        unsafe { pd.Clone() }
    }
    fn Start(&self, pd: Option<&IMFPresentationDescriptor>, _fmt: *const GUID, pos: *const PROPVARIANT) -> windows::core::Result<()> {
        let st = self.st.lock().unwrap();
        let (Some(stream), Some(obj)) = (st.stream.clone(), st.stream_obj.clone()) else { return Err(MF_E_SHUTDOWN.into()) };
        drop(st);
        unsafe {
            if let Some(pd) = pd {
                let _ = pd.SelectStream(0);
            }
            let first = !obj.started();
            let met = if first { MENewStream } else { MEUpdatedStream };
            self.queue.QueueEventParamUnk(met.0 as u32, &GUID::zeroed(), S_OK, &stream.cast::<IUnknown>()?)?;
            let empty = PROPVARIANT::default();
            self.queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, if pos.is_null() { &empty } else { pos })?;
        }
        obj.start()
    }
    fn Stop(&self) -> windows::core::Result<()> {
        if let Some(obj) = self.st.lock().unwrap().stream_obj.clone() {
            obj.stop()?;
        }
        unsafe { self.queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, &PROPVARIANT::default()) }
    }
    fn Pause(&self) -> windows::core::Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }
    fn Shutdown(&self) -> windows::core::Result<()> {
        let mut st = self.st.lock().unwrap();
        if let Some(obj) = st.stream_obj.take() {
            obj.shutdown();
        }
        st.stream = None;
        st.pd = None;
        unsafe { self.queue.Shutdown() }
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> windows::core::Result<IMFAttributes> {
        Ok(self.attrs.clone())
    }
    fn GetStreamAttributes(&self, id: u32) -> windows::core::Result<IMFAttributes> {
        if id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        let st = self.st.lock().unwrap();
        st.stream_obj.as_ref().map(|s| s.attrs.clone()).ok_or(MF_E_SHUTDOWN.into())
    }
    fn SetD3DManager(&self, _m: Option<&IUnknown>) -> windows::core::Result<()> {
        Ok(()) // las imágenes van en memoria normal
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(&self, _s: *const GUID, _r: *const GUID, _p: *mut *mut c_void) -> windows::core::Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for MediaSource_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> windows::core::Result<()> {
        Err(not_found())
    }
    fn KsMethod(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> windows::core::Result<()> {
        Err(not_found())
    }
    fn KsEvent(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> windows::core::Result<()> {
        Err(not_found())
    }
}

// ───────────── Flujo de vídeo ─────────────

struct StreamState {
    source: Option<IMFMediaSource>,
    state: MF_STREAM_STATE,
    started: bool,
    frame: Vec<u8>,
    next: Option<Instant>,
}

#[implement(IMFMediaStream2, IKsControl)]
struct Stream {
    queue: IMFMediaEventQueue,
    sd: IMFStreamDescriptor,
    attrs: IMFAttributes,
    t0: AtomicU64,
    st: Mutex<StreamState>,
}

impl Stream {
    fn new(source: IMFMediaSource, sd: IMFStreamDescriptor) -> windows::core::Result<Stream> {
        unsafe {
            let mut a = None;
            MFCreateAttributes(&mut a, 4)?;
            let attrs = a.unwrap();
            stream_attrs(&attrs)?;
            Ok(Stream {
                queue: MFCreateEventQueue()?,
                sd,
                attrs,
                t0: AtomicU64::new(0),
                st: Mutex::new(StreamState { source: Some(source), state: MF_STREAM_STATE_STOPPED, started: false, frame: vec![], next: None }),
            })
        }
    }
    fn started(&self) -> bool {
        self.st.lock().unwrap().started
    }
    fn start(&self) -> windows::core::Result<()> {
        {
            let mut st = self.st.lock().unwrap();
            st.started = true;
            st.state = MF_STREAM_STATE_RUNNING;
        }
        self.t0.store(unsafe { MFGetSystemTime() } as u64, Ordering::Relaxed);
        unsafe { self.queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, &PROPVARIANT::default()) }
    }
    fn stop(&self) -> windows::core::Result<()> {
        self.st.lock().unwrap().state = MF_STREAM_STATE_STOPPED;
        unsafe { self.queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, &PROPVARIANT::default()) }
    }
    fn shutdown(&self) {
        self.st.lock().unwrap().source = None; // rompe el ciclo fuente ↔ flujo
        unsafe {
            let _ = self.queue.Shutdown();
        }
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, f: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        let q = self.queue.clone();
        unsafe { q.GetEvent(f.0 as u32) }
    }
    fn BeginGetEvent(&self, cb: Option<&IMFAsyncCallback>, state: Option<&IUnknown>) -> windows::core::Result<()> {
        unsafe { self.queue.BeginGetEvent(cb, state) }
    }
    fn EndGetEvent(&self, r: Option<&IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(r) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> windows::core::Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, v) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> windows::core::Result<IMFMediaSource> {
        self.st.lock().unwrap().source.clone().ok_or(MF_E_SHUTDOWN.into())
    }
    fn GetStreamDescriptor(&self) -> windows::core::Result<IMFStreamDescriptor> {
        Ok(self.sd.clone())
    }
    fn RequestSample(&self, token: Option<&IUnknown>) -> windows::core::Result<()> {
        let mut st = self.st.lock().unwrap();
        if st.source.is_none() {
            return Err(MF_E_SHUTDOWN.into());
        }
        if st.state != MF_STREAM_STATE_RUNNING {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        // a ~30 imágenes por segundo, aunque se pidan más deprisa
        let period = Duration::from_micros(1_000_000 / FPS);
        let now = Instant::now();
        let next = st.next.unwrap_or(now);
        if next > now {
            std::thread::sleep(next - now);
        }
        st.next = Some(next.max(now) + period);
        let mut frame = std::mem::take(&mut st.frame);
        current_frame(&mut frame);
        unsafe {
            let buf = MFCreateMemoryBuffer(FRAME as u32)?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(frame.as_ptr(), p, FRAME);
            buf.Unlock()?;
            buf.SetCurrentLength(FRAME as u32)?;
            st.frame = frame;
            drop(st);
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(MFGetSystemTime())?;
            s.SetSampleDuration((10_000_000 / FPS) as i64)?;
            if let Some(t) = token {
                s.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            self.queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &s.cast::<IUnknown>()?)
        }
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, v: MF_STREAM_STATE) -> windows::core::Result<()> {
        self.st.lock().unwrap().state = v;
        Ok(())
    }
    fn GetStreamState(&self) -> windows::core::Result<MF_STREAM_STATE> {
        Ok(self.st.lock().unwrap().state)
    }
}

impl IKsControl_Impl for Stream_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> windows::core::Result<()> {
        Err(not_found())
    }
    fn KsMethod(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> windows::core::Result<()> {
        Err(not_found())
    }
    fn KsEvent(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> windows::core::Result<()> {
        Err(not_found())
    }
}

#[allow(dead_code)]
fn _unused() -> HRESULT {
    E_NOTIMPL
}
