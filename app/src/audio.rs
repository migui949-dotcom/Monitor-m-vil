//! Sonido con AAudio (baja latencia):
//!  - Altavoz: PCM 48 kHz s16 estéreo que llega del equipo (canal 2) → altavoces del móvil.
//!  - Micrófono: micrófono del móvil → PCM 48 kHz s16 mono → equipo (canal 3).

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use ndk::audio::{AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
                 AudioStream, AudioStreamBuilder, AudioUsage};

use crate::link::Link;

pub const CH_AUDIO: u8 = 2;
pub const CH_MIC: u8 = 3;
const RATE: i32 = 48_000;

/// AudioStream no se declara Send, pero AAudio permite controlarlo desde cualquier hilo
struct Stream(AudioStream);
unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

pub struct Speaker {
    _stream: Stream,
    buf: Arc<Mutex<VecDeque<i16>>>,
}

impl Speaker {
    pub fn start() -> Option<Speaker> {
        let buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::with_capacity(RATE as usize)));
        let q = buf.clone();
        let prime = (RATE as usize * 2) * 40 / 1000;   // 40 ms antes de empezar a sonar
        let mut primed = false;
        let stream = AudioStreamBuilder::new().ok()?
            .direction(AudioDirection::Output)
            .format(AudioFormat::PCM_I16)
            .channel_count(2)
            .sample_rate(RATE)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .usage(AudioUsage::Media)
            .data_callback(Box::new(move |_s, data, frames| {
                let out = unsafe { std::slice::from_raw_parts_mut(data as *mut i16, frames as usize * 2) };
                let mut q = q.lock().unwrap();
                if !primed && q.len() >= prime { primed = true; }
                if !primed {
                    out.fill(0);
                } else {
                    for o in out.iter_mut() { *o = q.pop_front().unwrap_or(0); }
                    if q.is_empty() { primed = false; }      // se vació: vuelve a acumular un poco
                }
                AudioCallbackResult::Continue
            }))
            .open_stream();
        let stream = match stream { Ok(s) => s, Err(e) => { log::error!("altavoz: {e:?}"); return None; } };
        if let Err(e) = stream.request_start() { log::error!("altavoz start: {e:?}"); return None; }
        log::info!("altavoz: listo ({} frames por ráfaga)", stream.frames_per_burst());
        Some(Speaker { _stream: Stream(stream), buf })
    }

    /// PCM s16le estéreo que llega del equipo
    pub fn push(&self, pcm: &[u8]) {
        let mut q = self.buf.lock().unwrap();
        q.extend(pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
        // si se acumula más de 150 ms de retraso, se tira lo viejo (queda en 60 ms)
        let max = (RATE as usize * 2) * 150 / 1000;
        if q.len() > max {
            let keep = (RATE as usize * 2) * 60 / 1000;
            let drop = q.len() - keep;
            q.drain(..drop);
        }
    }
}

pub struct Mic {
    _stream: Stream,
}

impl Mic {
    /// Empieza a grabar y manda bloques de 10 ms al equipo
    pub fn start(link: Arc<Link>) -> Option<Mic> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            while let Ok(b) = rx.recv() {
                if link.is_closed() || link.send(CH_MIC, &b).is_err() { break; }
            }
        });
        let mut acc: Vec<u8> = Vec::with_capacity(960 * 2);
        let stream = AudioStreamBuilder::new().ok()?
            .direction(AudioDirection::Input)
            .format(AudioFormat::PCM_I16)
            .channel_count(1)
            .sample_rate(RATE)
            .performance_mode(AudioPerformanceMode::LowLatency)
            // modo "voz": el móvil aplica su cancelación de eco y reducción de ruido
            .input_preset(AudioInputPreset::VoiceCommunication)
            .data_callback(Box::new(move |_s, data, frames| {
                let inp = unsafe { std::slice::from_raw_parts(data as *const u8, frames as usize * 2) };
                acc.extend_from_slice(inp);
                while acc.len() >= 960 {                 // 480 muestras = 10 ms
                    let block: Vec<u8> = acc.drain(..960).collect();
                    let _ = tx.try_send(block);          // si el envío va lento se descarta: nunca bloquear aquí
                }
                AudioCallbackResult::Continue
            }))
            .open_stream();
        let stream = match stream { Ok(s) => s, Err(e) => { log::error!("micrófono: {e:?}"); return None; } };
        if let Err(e) = stream.request_start() { log::error!("micrófono start: {e:?}"); return None; }
        log::info!("micrófono: grabando");
        Some(Mic { _stream: Stream(stream) })
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        let _ = self._stream.0.request_stop();
        log::info!("micrófono: parado");
    }
}
