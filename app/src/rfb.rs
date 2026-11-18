//! Cliente RFB (VNC) mínimo: solo lo que hace falta para wayvnc con H.264.
//!
//! wayvnc (en el equipo, con -g) comprime el monitor del móvil en H.264 ("Open H.264", codificación 50);
//! cada actualización trae un trozo de flujo H.264 que va directo al decodificador por hardware.
//! Los toques se envían como PointerEvent.

use std::io::{self, Read};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::link::{Link, CH_SCREEN};
use crate::video::Video;

const ENC_H264: i32 = 50;
const ENC_DESKTOP_SIZE: i32 = -223;

/// Lee los bytes que llegan por el canal del escritorio como si fuera un socket
struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            self.buf = self.rx.recv().map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "canal cerrado"))?;
            self.pos = 0;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn u8_(r: &mut impl Read) -> io::Result<u8> { let mut b = [0u8; 1]; r.read_exact(&mut b)?; Ok(b[0]) }
fn u16_(r: &mut impl Read) -> io::Result<u16> { let mut b = [0u8; 2]; r.read_exact(&mut b)?; Ok(u16::from_be_bytes(b)) }
fn u32_(r: &mut impl Read) -> io::Result<u32> { let mut b = [0u8; 4]; r.read_exact(&mut b)?; Ok(u32::from_be_bytes(b)) }
fn skip(r: &mut impl Read, n: usize) -> io::Result<()> { io::copy(&mut r.take(n as u64), &mut io::sink())?; Ok(()) }

fn update_request(link: &Link, incremental: bool, w: u16, h: u16) -> io::Result<()> {
    let mut m = vec![3u8, incremental as u8, 0, 0, 0, 0];
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    link.send(CH_SCREEN, &m)
}

/// Mensaje PointerEvent: botones (bit0 izq., bit1 central, bit2 der., bits 3/4 rueda) y posición
pub fn pointer(link: &Link, mask: u8, x: u16, y: u16) {
    let mut m = vec![5u8, mask];
    m.extend_from_slice(&x.to_be_bytes());
    m.extend_from_slice(&y.to_be_bytes());
    let _ = link.send(CH_SCREEN, &m);
}

/// Mensaje KeyEvent: tecla (keysym X11) pulsada o soltada
pub fn key(link: &Link, keysym: u32, down: bool) {
    let mut m = vec![4u8, down as u8, 0, 0];
    m.extend_from_slice(&keysym.to_be_bytes());
    let _ = link.send(CH_SCREEN, &m);
}

pub fn run(link: Arc<Link>, rx: Receiver<Vec<u8>>, video: Arc<Video>) -> io::Result<()> {
    let mut r = ChanReader { rx, buf: Vec::new(), pos: 0 };

    // Versión
    let mut ver = [0u8; 12];
    r.read_exact(&mut ver)?;
    link.send(CH_SCREEN, b"RFB 003.008\n")?;
    // Seguridad: wayvnc solo escucha en 127.0.0.1 del equipo; el cifrado lo pone nuestra conexión
    let n = u8_(&mut r)? as usize;
    let mut types = vec![0u8; n];
    r.read_exact(&mut types)?;
    if !types.contains(&1) {
        return Err(io::Error::other(format!("tipos de seguridad no soportados: {types:?}")));
    }
    link.send(CH_SCREEN, &[1])?;
    if u32_(&mut r)? != 0 {
        let len = u32_(&mut r)? as usize;
        skip(&mut r, len)?;
        return Err(io::Error::other("el servidor VNC rechazó la conexión"));
    }
    link.send(CH_SCREEN, &[1])?;                     // ClientInit: compartido
    let (mut w, mut h) = (u16_(&mut r)?, u16_(&mut r)?);
    skip(&mut r, 16)?;                               // formato de píxel (no se usa con H.264)
    let name_len = u32_(&mut r)? as usize;
    skip(&mut r, name_len)?;
    log::info!("RFB: escritorio {w}x{h}");
    video.configure(w as i32, h as i32);

    // Codificaciones: solo H.264 (+ cambio de tamaño)
    let encs = [ENC_H264, ENC_DESKTOP_SIZE];
    let mut m = vec![2u8, 0];
    m.extend_from_slice(&(encs.len() as u16).to_be_bytes());
    for e in encs { m.extend_from_slice(&e.to_be_bytes()); }
    link.send(CH_SCREEN, &m)?;
    update_request(&link, false, w, h)?;

    loop {
        match u8_(&mut r)? {
            0 => {
                // FramebufferUpdate
                skip(&mut r, 1)?;
                let rects = u16_(&mut r)?;
                // Se pide la siguiente ya: el servidor puede preparar la próxima imagen mientras esta
                // llega y se decodifica (ahorra un viaje de ida y vuelta por fotograma).
                update_request(&link, true, w, h)?;
                for _ in 0..rects {
                    let (_x, _y, rw, rh) = (u16_(&mut r)?, u16_(&mut r)?, u16_(&mut r)?, u16_(&mut r)?);
                    let enc = u32_(&mut r)? as i32;
                    match enc {
                        ENC_H264 => {
                            let len = u32_(&mut r)? as usize;
                            let flags = u32_(&mut r)?;
                            let mut data = vec![0u8; len];
                            r.read_exact(&mut data)?;
                            video.feed(&data, flags & 3 != 0);
                        }
                        ENC_DESKTOP_SIZE => {
                            w = rw;
                            h = rh;
                            log::info!("RFB: nuevo tamaño {w}x{h}");
                            video.configure(w as i32, h as i32);
                        }
                        0 => skip(&mut r, rw as usize * rh as usize * 4)?,   // Raw (no debería llegar)
                        other => return Err(io::Error::other(format!("codificación no soportada: {other}"))),
                    }
                }
            }
            1 => {
                // SetColourMapEntries
                skip(&mut r, 3)?;
                let n = u16_(&mut r)? as usize;
                skip(&mut r, n * 6)?;
            }
            2 => {} // Bell
            3 => {
                // ServerCutText (portapapeles; más adelante)
                skip(&mut r, 3)?;
                let n = u32_(&mut r)? as usize;
                skip(&mut r, n)?;
            }
            t => return Err(io::Error::other(format!("mensaje RFB desconocido: {t}"))),
        }
    }
}
