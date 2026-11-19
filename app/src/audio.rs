//! Sonido con AAudio (baja latencia):
//!  - Altavoz: PCM 48 kHz s16 estéreo que llega del equipo (canal 2) → altavoces del móvil.
//!  - Micrófono: micrófono del móvil → PCM 48 kHz s16 mono → equipo (canal 3).

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use ndk::audio::{AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
                 AudioStream, AudioStreamBuilder, AudioUsage};

use crate::link::Link;

pub const CH_AUDIO: u8 = 2;
pub const CH_MIC: u8 = 3;
const RATE: i32 = 48_000;
/// tras este rato sin sonido (solo ceros o nada) el altavoz se pausa para no gastar batería
const SILENCE_PAUSE: Duration = Duration::from_secs(3);
/// muestras con |valor| ≤ esto cuentan como silencio (ruido de redondeo)
const SILENCE_LEVEL: i16 = 8;

/// AudioStream no se declara Send, pero AAudio permite controlarlo desde cualquier hilo
struct Stream(AudioStream);
unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

pub struct Speaker {
    stream: Arc<Stream>,
    buf: Arc<Mutex<VecDeque<i16>>>,
    /// (último momento con sonido, ¿altavoz en marcha?)
    state: Arc<Mutex<(Instant, bool)>>,
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
        let stream = Arc::new(Stream(stream));
        let state = Arc::new(Mutex::new((Instant::now(), true)));
        // vigilante: con el silencio el altavoz se pausa (AAudio y el audio del sistema dejan de trabajar)
        {
            let (weak, state, buf): (Weak<Stream>, _, _) = (Arc::downgrade(&stream), state.clone(), buf.clone());
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(500));
                let Some(stream) = weak.upgrade() else { break };
                let mut st = state.lock().unwrap();
                if st.1 && st.0.elapsed() > SILENCE_PAUSE {
                    match stream.0.request_pause() {
                        Ok(()) => { st.1 = false; buf.lock().unwrap().clear(); log::info!("altavoz: en pausa (silencio)"); }
                        Err(e) => log::warn!("altavoz pausa: {e:?}"),
                    }
                }
            });
        }
        Some(Speaker { stream, buf, state })
    }

    /// PCM s16le estéreo que llega del equipo
    pub fn push(&self, pcm: &[u8]) {
        let samples = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]));
        let sound = samples.clone().any(|v| v.unsigned_abs() > SILENCE_LEVEL as u16);
        {
            let mut st = self.state.lock().unwrap();
            if sound {
                st.0 = Instant::now();
                if !st.1 {
                    // vuelve a sonar: arrancar ya; lo que llegue mientras tanto se queda en la cola
                    match self.stream.0.request_start() {
                        Ok(()) => { st.1 = true; log::info!("altavoz: en marcha"); }
                        Err(e) => log::warn!("altavoz start: {e:?}"),
                    }
                }
            } else if !st.1 {
                return;     // en pausa y sigue el silencio: no se guarda nada
            }
        }
        let mut q = self.buf.lock().unwrap();
        q.extend(samples);
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
