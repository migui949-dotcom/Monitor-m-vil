//! Fotograma BGRX/RGBX → I420 → H.264 (OpenH264 compilado dentro del binario).

use crate::display::{Frame, PixelOrder};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, Profile, RateControlMode, UsageType};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

pub struct H264 {
    enc: Encoder,
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
        let bps = ((w * h) as f32 * fps * 0.12).clamp(2_000_000.0, 16_000_000.0) as u32;
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps))
            .skip_frames(false)
            .num_threads(2);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("openh264: {e}"))?;
        Ok(Self { enc, w, h, y: vec![0; w * h], u: vec![0; w * h / 4], v: vec![0; w * h / 4] })
    }

    pub fn size(&self) -> (usize, usize) {
        (self.w, self.h)
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
