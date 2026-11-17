//! Windows: lo que suena en el equipo (captura "loopback" de WASAPI) → altavoces del móvil (canal 2).
//! Mismo formato que en Linux: PCM 48 kHz, s16le, estéreo, bloques de 10 ms. Viene con Windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

const CHUNK: usize = 48000 * 2 * 2 / 100;
const AUTOCONVERTPCM: u32 = 0x8000_0000;
const SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;

pub struct Speakers {
    stop: Arc<AtomicBool>,
}

impl Drop for Speakers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Speakers {
    pub fn start(out: UnboundedSender<(u8, Vec<u8>)>) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        std::thread::Builder::new()
            .name("sonido".into())
            .spawn(move || {
                while !s2.load(Ordering::Relaxed) {
                    if let Err(e) = capture(&out, &s2) {
                        crate::host::log(&format!("sonido: {e}"));
                        std::thread::sleep(Duration::from_secs(2)); // p. ej. se cambió de salida de audio
                    }
                }
            })
            .ok()?;
        Some(Speakers { stop })
    }
}

/// Formato en el que llegan las muestras de WASAPI.
struct Fmt {
    float: bool,
    bits: u16,
    channels: usize,
    rate: u32,
}

fn capture(out: &UnboundedSender<(u8, Vec<u8>)>, stop: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        // 1º: que Windows lo convierta a 48 kHz s16 estéreo
        let want = WAVEFORMATEX { wFormatTag: 1, nChannels: 2, nSamplesPerSec: 48000, nAvgBytesPerSec: 48000 * 4, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 };
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK;
        let fmt = match client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags | AUTOCONVERTPCM | SRC_DEFAULT_QUALITY, 200_000, 0, &want, None) {
            Ok(()) => Fmt { float: false, bits: 16, channels: 2, rate: 48000 },
            Err(_) => {
                // 2º: el formato de mezcla del sistema, y se convierte aquí
                let client2: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
                let mix = client2.GetMixFormat()?;
                let m = *mix;
                let float = m.wFormatTag == 3 || (m.wFormatTag == 0xFFFE && m.wBitsPerSample == 32);
                client2.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, mix, None)?;
                CoTaskMemFree(Some(mix as *const _));
                return run(&client2, Fmt { float, bits: m.wBitsPerSample, channels: m.nChannels as usize, rate: m.nSamplesPerSec }, out, stop);
            }
        };
        run(&client, fmt, out, stop)
    }
}

unsafe fn run(client: &IAudioClient, fmt: Fmt, out: &UnboundedSender<(u8, Vec<u8>)>, stop: &AtomicBool) -> windows::core::Result<()> {
    let cap: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    let mut pcm: Vec<u8> = Vec::with_capacity(CHUNK * 4);
    let mut pos = 0f64; // remuestreo sencillo (lineal) si el equipo no va a 48 kHz
    let step = fmt.rate as f64 / 48000.0;
    let mut prev = (0f32, 0f32);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(5));
        loop {
            let n = cap.GetNextPacketSize()?;
            if n == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let bpf = fmt.channels * fmt.bits as usize / 8;
            let raw = std::slice::from_raw_parts(data, frames as usize * bpf);
            if !silent && fmt.rate == 48000 && !fmt.float && fmt.bits == 16 && fmt.channels == 2 {
                pcm.extend_from_slice(raw);
            } else {
                // a f32 estéreo y luego a s16 48 kHz
                let sample = |f: usize, c: usize| -> f32 {
                    if silent {
                        return 0.0;
                    }
                    let c = c.min(fmt.channels - 1);
                    let o = f * bpf + c * fmt.bits as usize / 8;
                    match (fmt.float, fmt.bits) {
                        (true, 32) => f32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]),
                        (false, 16) => i16::from_le_bytes([raw[o], raw[o + 1]]) as f32 / 32768.0,
                        (false, 24) => (i32::from_le_bytes([0, raw[o], raw[o + 1], raw[o + 2]]) >> 8) as f32 / 8_388_608.0,
                        (false, 32) => i32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]) as f32 / 2_147_483_648.0,
                        _ => 0.0,
                    }
                };
                let total = frames as usize;
                while pos < total as f64 {
                    let i = pos as usize;
                    let t = (pos - i as f64) as f32;
                    let a = if i == 0 { prev } else { (sample(i - 1, 0), sample(i - 1, 1)) };
                    let b = (sample(i, 0), sample(i, 1));
                    let (l, r) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                    for v in [l, r] {
                        pcm.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                    }
                    pos += step;
                }
                pos -= total as f64;
                if total > 0 {
                    prev = (sample(total - 1, 0), sample(total - 1, 1));
                }
            }
            cap.ReleaseBuffer(frames)?;
        }
        while pcm.len() >= CHUNK {
            let block: Vec<u8> = pcm.drain(..CHUNK).collect();
            if block.iter().all(|b| *b == 0) {
                continue; // silencio: no se envía
            }
            if out.send((2, block)).is_err() {
                let _ = client.Stop();
                return Ok(());
            }
        }
    }
    let _ = client.Stop();
    Ok(())
}
