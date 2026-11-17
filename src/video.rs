//! Fotograma BGRX/RGBX → H.264: por hardware si se puede (ffmpeg con VAAPI/NVENC, ver hwenc.rs) y si
//! no con OpenH264 (BGRX → I420 en el procesador; compilado dentro del binario).

use crate::display::{Frame, PixelOrder};
use openh264::encoder::{BitRate, Encoder as OhEncoder, EncoderConfig, FrameRate, Profile, RateControlMode, UsageType};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

/// Bits por segundo para un monitor de w×h a `fps` (el mismo cálculo para hardware y software)
pub fn bitrate(w: usize, h: usize, fps: f32) -> u32 {
    ((w * h) as f32 * fps * 0.12).clamp(2_000_000.0, 16_000_000.0) as u32
}

/// El compresor de una sesión: por hardware si hay, y si falla (al arrancar o en marcha) OpenH264.
pub struct Encoder {
    #[cfg(target_os = "linux")]
    hw: Option<crate::hwenc::HwEnc>,
    soft: Option<H264>,
    w: usize,
    h: usize,
    fps: f32,
    soft_started: bool,
}

/// Métodos por hardware que ya fallaron en este equipo (no se reintentan en cada conexión)
#[cfg(target_os = "linux")]
static BROKEN: std::sync::Mutex<Vec<crate::hwenc::Kind>> = std::sync::Mutex::new(Vec::new());

impl Encoder {
    pub fn new(w: usize, h: usize, fps: f32) -> Result<Self, String> {
        let (w, h) = (w & !1, h & !1);
        #[cfg(target_os = "linux")]
        {
            let pref = crate::util::conf("ENCODER").unwrap_or_else(|| "auto".into()).to_lowercase();
            let broken = BROKEN.lock().unwrap().clone();
            for kind in crate::hwenc::candidates(&pref).into_iter().filter(|k| !broken.contains(k)) {
                match crate::hwenc::HwEnc::new(kind, w, h, fps, bitrate(w, h, fps)) {
                    Ok(e) => return Ok(Encoder { hw: Some(e), soft: None, w, h, fps, soft_started: false }),
                    Err(e) => {
                        crate::host::log(&format!("vídeo: {} no disponible ({e})", kind.name()));
                        BROKEN.lock().unwrap().push(kind);
                    }
                }
            }
        }
        Ok(Encoder {
            #[cfg(target_os = "linux")]
            hw: None,
            soft: Some(H264::new(w, h, fps)?),
            w,
            h,
            fps,
            soft_started: false,
        })
    }

    pub fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    /// Nombre del compresor en uso (para el registro)
    pub fn name(&self) -> &'static str {
        #[cfg(target_os = "linux")]
        if let Some(h) = &self.hw {
            return h.kind().name();
        }
        "OpenH264 (software)"
    }

    pub fn encode(&mut self, f: &Frame, idr: bool) -> Result<Vec<u8>, String> {
        #[cfg(target_os = "linux")]
        if let Some(hw) = self.hw.as_mut() {
            match hw.encode(&f.data, f.stride, idr) {
                Ok(v) if f.order == PixelOrder::Bgrx => return Ok(v),
                Ok(_) => crate::host::log("vídeo: la captura no es BGRX; se pasa a software"),
                Err(e) => {
                    crate::host::log(&format!("vídeo: {} falló ({e}); se pasa a software", hw.kind().name()));
                    BROKEN.lock().unwrap().push(hw.kind());
                }
            }
            self.hw = None;
        }
        if self.soft.is_none() {
            self.soft = Some(H264::new(self.w, self.h, self.fps)?);
        }
        // al cambiar de compresor, la primera imagen tiene que ser completa
        let first = !self.soft_started;
        self.soft_started = true;
        self.soft.as_mut().unwrap().encode(f, idr || first)
    }
}

pub struct H264 {
    enc: OhEncoder,
    w: usize,
    h: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl H264 {
    /// `w`/`h` se redondean a par (lo exige 4:2:0).
    pub fn new(w: usize, h: usize, fps: f32) -> Result<Self, String> {
        let (w, h) = (w & !1, h & !1);
        let bps = bitrate(w, h, fps);
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps))
            .skip_frames(false)
            .num_threads(2);
        let enc = OhEncoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("openh264: {e}"))?;
        Ok(Self { enc, w, h, y: vec![0; w * h], u: vec![0; w * h / 4], v: vec![0; w * h / 4] })
    }

    /// Devuelve el flujo Annex-B (vacío si el codificador no ha sacado nada).
    pub fn encode(&mut self, f: &Frame, idr: bool) -> Result<Vec<u8>, String> {
        self.to_i420(f);
        if idr {
            self.enc.force_intra_frame();
        }
        let src = YUVSlices::new((&self.y, &self.u, &self.v), (self.w, self.h), (self.w, self.w / 2, self.w / 2));
        let bs = self.enc.encode(&src).map_err(|e| format!("codificar: {e}"))?;
        Ok(bs.to_vec())
    }

    /// BT.601 rango limitado (lo que asumen los decodificadores sin VUI).
    fn to_i420(&mut self, f: &Frame) {
        let (w, h) = (self.w.min(f.width & !1), self.h.min(f.height & !1));
        let (ri, bi) = match f.order {
            PixelOrder::Bgrx => (2, 0),
            PixelOrder::Rgbx => (0, 2),
        };
        let cw = self.w / 2;
        for yy in (0..h).step_by(2) {
            let r0 = &f.data[yy * f.stride..];
            let r1 = &f.data[(yy + 1) * f.stride..];
            let (yrow0, rest) = self.y[yy * self.w..].split_at_mut(self.w);
            let yrow1 = &mut rest[..self.w];
            let urow = &mut self.u[(yy / 2) * cw..(yy / 2) * cw + cw];
            let vrow = &mut self.v[(yy / 2) * cw..(yy / 2) * cw + cw];
            for xx in (0..w).step_by(2) {
                let mut rs = 0i32;
                let mut gs = 0i32;
                let mut bs = 0i32;
                for (row, yrow) in [(r0, &mut *yrow0), (r1, &mut *yrow1)] {
                    for dx in 0..2 {
                        let p = (xx + dx) * 4;
                        let r = row[p + ri] as i32;
                        let g = row[p + 1] as i32;
                        let b = row[p + bi] as i32;
                        yrow[xx + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                        rs += r;
                        gs += g;
                        bs += b;
                    }
                }
                let (r, g, b) = (rs >> 2, gs >> 2, bs >> 2);
                urow[xx / 2] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                vrow[xx / 2] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }
}
