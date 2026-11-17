//! Servidor RFB 3.8 mínimo con la codificación "Open H.264" (50), que es lo que pide la app:
//!   SetEncodings = [50 (Open H.264), -223 (DesktopSize)]; sin contraseña (el canal ya va cifrado con Noise).
//! Responde a cada FramebufferUpdateRequest con un rectángulo H.264 de todo el monitor.

use crate::display::Display;
use crate::video::H264;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const ENC_H264: i32 = 50;
const ENC_DESKTOP_SIZE: i32 = -223;

struct Reader {
    rx: Receiver<Vec<u8>>,
    buf: VecDeque<u8>,
    stop: Arc<AtomicBool>,
}

impl Reader {
    fn read(&mut self, n: usize) -> Option<Vec<u8>> {
        while self.buf.len() < n {
            match self.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(d) => self.buf.extend(d),
                Err(RecvTimeoutError::Timeout) => {
                    if self.stop.load(Ordering::Relaxed) {
                        return None;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
        Some(self.buf.drain(..n).collect())
    }
}

#[derive(Default)]
struct Req {
    pending: bool,
    full: bool,
    h264: bool,
}

struct Shared {
    req: Mutex<Req>,
    cv: Condvar,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Arranca el servidor en dos hilos (lector de mensajes del móvil + codificador).
/// `rx`: bytes que llegan del móvil por el canal 1. `tx`: bytes para el canal 1.
pub fn spawn(display: Arc<dyn Display>, rx: Receiver<Vec<u8>>, tx: UnboundedSender<Vec<u8>>, stop: Arc<AtomicBool>, fps: f32) {
    let shared = Arc::new(Shared { req: Mutex::new(Req::default()), cv: Condvar::new() });
    let (w, h) = display.size();

    // ── Lector ──
    {
        let shared = shared.clone();
        let display = display.clone();
        let tx = tx.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("rfb-lector".into())
            .spawn(move || {
                let mut r = Reader { rx, buf: VecDeque::new(), stop: stop.clone() };
                if handshake(&mut r, &tx, w as u16, h as u16).is_none() {
                    stop.store(true, Ordering::Relaxed);
                    shared.cv.notify_all();
                    return;
                }
                while let Some(t) = r.read(1) {
                    let ok = match t[0] {
                        0 => r.read(19).map(|_| ()), // SetPixelFormat: da igual, el vídeo es H.264
                        2 => r.read(3).and_then(|b| {
                            let n = be16(&b[1..]) as usize;
                            let list = r.read(4 * n)?;
                            let encs: Vec<i32> = list.chunks(4).map(|c| be32(c) as i32).collect();
                            crate::dbg_log!("rfb: codificaciones {:?}", encs);
                            shared.req.lock().unwrap().h264 = encs.contains(&ENC_H264);
                            Some(())
                        }),
                        3 => r.read(9).map(|b| {
                            let mut q = shared.req.lock().unwrap();
                            q.pending = true;
                            if b[0] == 0 {
                                q.full = true;
                            }
                            shared.cv.notify_all();
                        }),
                        4 => r.read(7).map(|b| display.key(be32(&b[3..]), b[0] != 0)),
                        5 => r.read(5).map(|b| display.pointer(be16(&b[1..]), be16(&b[3..]), b[0])),
                        6 => r.read(7).and_then(|b| r.read(be32(&b[3..]) as usize).map(|_| ())),
                        other => {
                            crate::dbg_log!("rfb: mensaje desconocido {other}");
                            None
                        }
                    };
                    if ok.is_none() {
                        break;
                    }
                }
                stop.store(true, Ordering::Relaxed);
                shared.cv.notify_all();
            })
            .expect("hilo");
    }

    // ── Codificador ──
    std::thread::Builder::new()
        .name("rfb-video".into())
        .spawn(move || {
            let mut enc = match H264::new(w as usize, h as usize, fps) {
                Ok(e) => e,
                Err(e) => {
                    crate::host::log(&format!("vídeo: {e}"));
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let (ew, eh) = enc.size();
            let mut last_seq = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // espera una petición
                let full = {
                    let mut q = shared.req.lock().unwrap();
                    while !q.pending && !stop.load(Ordering::Relaxed) {
                        q = shared.cv.wait_timeout(q, Duration::from_millis(500)).unwrap().0;
                    }
                    if !q.pending {
                        break;
                    }
                    if !q.h264 {
                        drop(q);
                        crate::host::log("rfb: el cliente no admite H.264");
                        break;
                    }
                    q.full
                };
                // una completa usa el último fotograma (aunque no haya cambios); una incremental espera a que cambie
                let frame = if full {
                    display.wait_frame(0, Duration::from_secs(3))
                } else {
                    display.wait_frame(last_seq, Duration::from_millis(250))
                };
                let Some(frame) = frame else { continue };
                // si mientras tanto llegó una petición completa, se hace IDR
                let idr = full || shared.req.lock().unwrap().full;
                let data = match enc.encode(&frame, idr) {
                    Ok(d) => d,
                    Err(e) => {
                        crate::host::log(&format!("vídeo: {e}"));
                        break;
                    }
                };
                last_seq = frame.seq;
                if data.is_empty() {
                    continue;
                }
                {
                    let mut q = shared.req.lock().unwrap();
                    q.pending = false;
                    if idr {
                        q.full = false;
                    }
                }
                let mut m = Vec::with_capacity(data.len() + 32);
                m.extend_from_slice(&[0, 0, 0, 1]); // FramebufferUpdate, 1 rectángulo
                m.extend_from_slice(&0u16.to_be_bytes());
                m.extend_from_slice(&0u16.to_be_bytes());
                m.extend_from_slice(&(ew as u16).to_be_bytes());
                m.extend_from_slice(&(eh as u16).to_be_bytes());
                m.extend_from_slice(&ENC_H264.to_be_bytes());
                m.extend_from_slice(&(data.len() as u32).to_be_bytes());
                m.extend_from_slice(&0u32.to_be_bytes()); // flags
                m.extend_from_slice(&data);
                if tx.send(m).is_err() {
                    break;
                }
            }
            stop.store(true, Ordering::Relaxed);
            let _ = ENC_DESKTOP_SIZE;
        })
        .expect("hilo");
}

fn handshake(r: &mut Reader, tx: &UnboundedSender<Vec<u8>>, w: u16, h: u16) -> Option<()> {
    tx.send(b"RFB 003.008\n".to_vec()).ok()?;
    let v = r.read(12)?;
    crate::dbg_log!("rfb: versión del cliente {:?}", String::from_utf8_lossy(&v));
    tx.send(vec![1, 1]).ok()?; // un tipo de seguridad: None
    let _sec = r.read(1)?;
    tx.send(vec![0, 0, 0, 0]).ok()?; // SecurityResult OK
    let _shared = r.read(1)?; // ClientInit
    let name = b"Pantalla";
    let mut m = Vec::new();
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    // 32 bpp, prof. 24, little endian, true colour, máx 255, desplazamientos 16/8/0
    m.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    m.extend_from_slice(&(name.len() as u32).to_be_bytes());
    m.extend_from_slice(name);
    tx.send(m).ok()?;
    Some(())
}
