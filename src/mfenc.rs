//! Windows: H.264 por hardware con Media Foundation (el codificador de la tarjeta gráfica: NVENC,
//! Quick Sync o AMF, según el equipo). Viene con Windows: no hace falta instalar nada.
//!
//! La imagen capturada (BGRX) se pasa a NV12 en el procesador y se le da al codificador en memoria
//! normal. Los codificadores por hardware son «asíncronos» (piden entrada y avisan de salida con
//! eventos); el de software de Microsoft es síncrono: se aceptan los dos modelos.
//! Si algo falla, video::Encoder vuelve a OpenH264.

#![allow(non_upper_case_globals)] // las constantes de eventos de Media Foundation se llaman así

use crate::display::{Frame, PixelOrder};
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

pub struct MfEnc {
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    /// cabecera SPS/PPS (para repetirla delante de cada IDR si el codificador no lo hace)
    seq: Vec<u8>,
    out_size: u32,
    provides_samples: bool,
    /// el codificador asíncrono ha pedido entrada y aún no se le ha dado
    need_input: u32,
    name: String,
    w: usize,
    h: usize,
    fps: f32,
    n: i64,
    nv12: Vec<u8>,
}

fn e<T>(r: windows::core::Result<T>, what: &str) -> Result<T, String> {
    r.map_err(|x| format!("{what}: {x}"))
}

impl MfEnc {
    /// `hardware`: solo codificadores de la tarjeta gráfica; si no, el de software de Windows (pruebas)
    pub fn new(w: usize, h: usize, fps: f32, bps: u32, hardware: bool) -> Result<MfEnc, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            e(MFStartup(MF_VERSION, MFSTARTUP_FULL), "MFStartup")?;
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let flags = if hardware { MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER } else { MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            e(MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut count), "MFTEnumEx")?;
            let acts: Vec<IMFActivate> = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
            if !list.is_null() {
                CoTaskMemFree(Some(list as *const _));
            }
            if acts.is_empty() {
                return Err(if hardware { "la tarjeta gráfica no tiene codificador H.264 de Media Foundation".into() } else { "no hay codificador H.264".into() });
            }
            let mut last = String::new();
            for a in acts {
                let name = friendly_name(&a);
                match Self::open(&a, &name, w, h, fps, bps) {
                    Ok(enc) => return Ok(enc),
                    Err(x) => {
                        crate::host::log(&format!("vídeo: {name}: {x}"));
                        last = x;
                    }
                }
            }
            Err(last)
        }
    }

    unsafe fn open(a: &IMFActivate, name: &str, w: usize, h: usize, fps: f32, bps: u32) -> Result<MfEnc, String> {
        let mft: IMFTransform = e(a.ActivateObject(), "activar")?;
        let attrs = mft.GetAttributes().ok();
        let is_async = attrs.as_ref().and_then(|at| at.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
        if is_async {
            e(attrs.as_ref().unwrap().SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1), "desbloquear asíncrono")?;
        }
        if let Some(at) = &attrs {
            let _ = at.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let codec: Option<ICodecAPI> = mft.cast().ok();
        if let Some(c) = &codec {
            // poco retardo, sin imágenes B, tasa variable con techo (la pantalla quieta gasta poco)
            let set = |g: &GUID, v: VARIANT| { let _ = c.SetValue(g, &v); };
            set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32));
            set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bps / 2));
            set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bps));
            set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from((fps as u32) * 10));
        }
        let size = ((w as u64) << 32) | h as u64;
        let rate = ((fps.round() as u64) << 32) | 1;
        let mut ok_out = false;
        let mut why = String::new();
        // perfil base primero (lo decodifica todo); si no, principal (sin B, igual de rápido)
        for profile in [eAVEncH264VProfile_Base.0 as u32, eAVEncH264VProfile_Main.0 as u32] {
            let t = e(MFCreateMediaType(), "tipo")?;
            let _ = t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264);
            let _ = t.SetUINT32(&MF_MT_AVG_BITRATE, bps);
            let _ = t.SetUINT64(&MF_MT_FRAME_SIZE, size);
            let _ = t.SetUINT64(&MF_MT_FRAME_RATE, rate);
            let _ = t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1);
            let _ = t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            let _ = t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile);
            match mft.SetOutputType(0, &t, 0) {
                Ok(()) => {
                    ok_out = true;
                    break;
                }
                Err(x) => why = x.to_string(),
            }
        }
        if !ok_out {
            return Err(format!("tipo de salida: {why}"));
        }
        let t = e(MFCreateMediaType(), "tipo")?;
        let _ = t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        let _ = t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
        let _ = t.SetUINT64(&MF_MT_FRAME_SIZE, size);
        let _ = t.SetUINT64(&MF_MT_FRAME_RATE, rate);
        let _ = t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1);
        let _ = t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
        e(mft.SetInputType(0, &t, 0), "tipo de entrada")?;
        let info = e(mft.GetOutputStreamInfo(0), "info de salida")?;
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        e(mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0), "empezar")?;
        e(mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0), "empezar flujo")?;
        let events = if is_async { Some(e(mft.cast::<IMFMediaEventGenerator>(), "eventos")?) } else { None };
        let mut enc = MfEnc {
            mft,
            events,
            codec,
            seq: vec![],
            out_size: info.cbSize.max((w * h) as u32),
            provides_samples,
            need_input: 0,
            name: format!("{name} (Media Foundation{})", if is_async { "" } else { ", software" }),
            w,
            h,
            fps,
            n: 0,
            nv12: vec![0; w * h * 3 / 2],
        };
        enc.read_seq_header();
        Ok(enc)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn read_seq_header(&mut self) {
        unsafe {
            if let Ok(t) = self.mft.GetOutputCurrentType(0) {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut n = 0u32;
                if t.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut p, &mut n).is_ok() && !p.is_null() {
                    self.seq = std::slice::from_raw_parts(p, n as usize).to_vec();
                    CoTaskMemFree(Some(p as *const _));
                }
            }
        }
    }

    pub fn encode(&mut self, f: &Frame, idr: bool) -> Result<Vec<u8>, String> {
        bgrx_to_nv12(f, self.w, self.h, &mut self.nv12);
        unsafe {
            let buf = e(MFCreateMemoryBuffer(self.nv12.len() as u32), "búfer")?;
            let mut p: *mut u8 = std::ptr::null_mut();
            e(buf.Lock(&mut p, None, None), "bloquear")?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), p, self.nv12.len());
            let _ = buf.Unlock();
            let _ = buf.SetCurrentLength(self.nv12.len() as u32);
            let s = e(MFCreateSample(), "muestra")?;
            e(s.AddBuffer(&buf), "añadir")?;
            let dur = (10_000_000.0 / self.fps as f64) as i64;
            let _ = s.SetSampleTime(self.n * dur);
            let _ = s.SetSampleDuration(dur);
            self.n += 1;
            if idr {
                if let Some(c) = &self.codec {
                    let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
                let _ = s.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
            let mut out = Vec::new();
            match self.events.clone() {
                None => {
                    e(self.mft.ProcessInput(0, &s, 0), "entrada")?;
                    while self.pull(&mut out)? {}
                }
                Some(ev) => {
                    // asíncrono: esperar a que pida entrada, dársela y recoger lo que salga
                    // (sin quedarse colgado: como mucho ~60 ms; lo que falte sale con la siguiente imagen)
                    let deadline = Instant::now() + Duration::from_millis(60);
                    let mut given = false;
                    loop {
                        if given && !out.is_empty() {
                            break;
                        }
                        if given && self.need_input > 0 && Instant::now() >= deadline {
                            break;
                        }
                        if !given && self.need_input > 0 {
                            e(self.mft.ProcessInput(0, &s, 0), "entrada")?;
                            self.need_input -= 1;
                            given = true;
                            continue;
                        }
                        match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                            Ok(m) => match MF_EVENT_TYPE(e(m.GetType(), "evento")? as i32) {
                                METransformNeedInput => self.need_input += 1,
                                METransformHaveOutput => {
                                    self.pull(&mut out)?;
                                }
                                _ => {}
                            },
                            Err(x) if x.code() == MF_E_NO_EVENTS_AVAILABLE => {
                                if Instant::now() >= deadline + Duration::from_millis(if given { 0 } else { 440 }) {
                                    if !given {
                                        return Err("el codificador no pide imágenes".into());
                                    }
                                    break;
                                }
                                std::thread::sleep(Duration::from_micros(500));
                            }
                            Err(x) => return Err(format!("evento: {x}")),
                        }
                    }
                }
            }
            Ok(self.with_headers(out))
        }
    }

    /// Saca una salida si la hay (false = el codificador necesita más entrada)
    unsafe fn pull(&mut self, out: &mut Vec<u8>) -> Result<bool, String> {
        let sample = if self.provides_samples {
            None
        } else {
            let s = e(MFCreateSample(), "muestra")?;
            let b = e(MFCreateMemoryBuffer(self.out_size), "búfer")?;
            e(s.AddBuffer(&b), "añadir")?;
            Some(s)
        };
        let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        let r = self.mft.ProcessOutput(0, &mut db, &mut status);
        let sample = ManuallyDrop::take(&mut db[0].pSample);
        drop(ManuallyDrop::take(&mut db[0].pEvents));
        match r {
            Ok(()) => {
                if let Some(s) = sample {
                    let b = e(s.ConvertToContiguousBuffer(), "búfer de salida")?;
                    let mut p: *mut u8 = std::ptr::null_mut();
                    let mut len = 0u32;
                    e(b.Lock(&mut p, None, Some(&mut len)), "bloquear salida")?;
                    out.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
                    let _ = b.Unlock();
                }
                Ok(true)
            }
            Err(x) if x.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(x) if x.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // el codificador cambia el tipo de salida (p. ej. ya sabe el SPS): se acepta el que ofrece
                let t = e(self.mft.GetOutputAvailableType(0, 0), "tipo nuevo")?;
                e(self.mft.SetOutputType(0, &t, 0), "tipo nuevo")?;
                self.read_seq_header();
                Ok(true)
            }
            Err(x) => Err(format!("salida: {x}")),
        }
    }

    /// Si sale una IDR sin SPS/PPS delante, se le pone la cabecera (la app la necesita para empezar)
    fn with_headers(&self, v: Vec<u8>) -> Vec<u8> {
        if v.is_empty() || self.seq.is_empty() {
            return v;
        }
        let types = nal_types(&v);
        if types.contains(&5) && !types.contains(&7) {
            let mut o = self.seq.clone();
            o.extend_from_slice(&v);
            return o;
        }
        v
    }
}

impl Drop for MfEnc {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(ev) = &self.events {
                if let Ok(s) = ev.cast::<IMFShutdown>() {
                    let _ = s.Shutdown();
                }
            }
        }
    }
}

fn friendly_name(a: &IMFActivate) -> String {
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut n = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n).is_ok() && !p.is_null() {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            return s;
        }
    }
    "codificador".into()
}

/// Tipos de unidad NAL de un flujo Annex B
fn nal_types(v: &[u8]) -> Vec<u8> {
    let mut t = vec![];
    let mut i = 0;
    while i + 3 < v.len() {
        if v[i] == 0 && v[i + 1] == 0 && (v[i + 2] == 1 || (v[i + 2] == 0 && v[i + 3] == 1)) {
            let start = if v[i + 2] == 1 { i + 3 } else { i + 4 };
            if start < v.len() {
                t.push(v[start] & 0x1f);
            }
            i = start;
        } else {
            i += 1;
        }
    }
    t
}

/// BGRX/RGBX → NV12 (BT.601 rango limitado, como el resto)
fn bgrx_to_nv12(f: &Frame, w: usize, h: usize, out: &mut [u8]) {
    let (ri, bi) = match f.order {
        PixelOrder::Bgrx => (2, 0),
        PixelOrder::Rgbx => (0, 2),
    };
    let (cw, ch) = (w.min(f.width & !1), h.min(f.height & !1));
    let (ys, uv) = out.split_at_mut(w * h);
    for yy in (0..ch).step_by(2) {
        let r0 = &f.data[yy * f.stride..];
        let r1 = &f.data[(yy + 1) * f.stride..];
        let uvrow = &mut uv[(yy / 2) * w..(yy / 2) * w + w];
        for xx in (0..cw).step_by(2) {
            let (mut rs, mut gs, mut bs) = (0i32, 0i32, 0i32);
            for (dy, row) in [r0, r1].into_iter().enumerate() {
                for dx in 0..2 {
                    let p = (xx + dx) * 4;
                    let (r, g, b) = (row[p + ri] as i32, row[p + 1] as i32, row[p + bi] as i32);
                    ys[(yy + dy) * w + xx + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    rs += r;
                    gs += g;
                    bs += b;
                }
            }
            let (r, g, b) = (rs >> 2, gs >> 2, bs >> 2);
            uvrow[xx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uvrow[xx + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}
