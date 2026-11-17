//! Enlace cifrado ya establecido (Noise, modo transporte) compartido por varios canales.
//!
//! Cada mensaje: longitud u16 BE + Noise( [canal u8][datos] ). Canal 0 = control (JSON),
//! 1 = escritorio (RFB). Los envíos se serializan con un candado para respetar el orden de los
//! contadores de Noise; la lectura la hace un único hilo.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const CH_CONTROL: u8 = 0;
pub const CH_SCREEN: u8 = 1;
const CHUNK: usize = 60_000;

pub struct Link {
    t: Mutex<snow::TransportState>,
    w: Mutex<TcpStream>,
    closed: AtomicBool,
}

impl Link {
    pub fn new(t: snow::TransportState, w: TcpStream) -> Link {
        Link { t: Mutex::new(t), w: Mutex::new(w), closed: AtomicBool::new(false) }
    }

    pub fn send(&self, ch: u8, data: &[u8]) -> io::Result<()> {
        let mut t = self.t.lock().unwrap();
        let mut out = Vec::with_capacity(data.len() + 64);
        let mut buf = vec![0u8; CHUNK + 64];
        let mut chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        for c in chunks {
            let mut plain = Vec::with_capacity(c.len() + 1);
            plain.push(ch);
            plain.extend_from_slice(c);
            let n = t.write_message(&plain, &mut buf).map_err(|e| io::Error::other(e.to_string()))?;
            out.extend_from_slice(&(n as u16).to_be_bytes());
            out.extend_from_slice(&buf[..n]);
        }
        // se escribe con el candado de Noise cogido: el orden en el cable = orden de los contadores
        self.w.lock().unwrap().write_all(&out)
    }

    pub fn send_json(&self, v: serde_json::Value) -> io::Result<()> {
        self.send(CH_CONTROL, &serde_json::to_vec(&v).unwrap())
    }

    /// Lee y descifra el siguiente mensaje (solo desde el hilo lector)
    pub fn recv(&self, r: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
        let mut len = [0u8; 2];
        r.read_exact(&mut len)?;
        let mut frame = vec![0u8; u16::from_be_bytes(len) as usize];
        r.read_exact(&mut frame)?;
        let mut buf = vec![0u8; frame.len()];
        let n = self.t.lock().unwrap().read_message(&frame, &mut buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "mensaje cifrado inválido"))?;
        if n == 0 {
            return Ok((CH_CONTROL, Vec::new()));
        }
        Ok((buf[0], buf[1..n].to_vec()))
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.send_json(serde_json::json!({"t": "bye"}));
            let _ = self.w.lock().unwrap().shutdown(Shutdown::Both);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}
