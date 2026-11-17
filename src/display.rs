//! Lo que cada sistema tiene que dar: un monitor virtual del que se sacan fotogramas
//! y al que se mandan ratón y teclado.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    /// bytes B,G,R,X (wl_shm XRGB8888/ARGB8888, DXGI B8G8R8A8)
    Bgrx,
    /// bytes R,G,B,X (wl_shm XBGR8888/ABGR8888)
    #[allow(dead_code)] // solo Linux
    Rgbx,
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub order: PixelOrder,
    pub data: Vec<u8>,
    pub seq: u64,
}

pub trait Display: Send + Sync {
    fn size(&self) -> (u32, u32);
    /// Espera un fotograma con `seq > after` (o devuelve None al pasar `timeout`).
    fn wait_frame(&self, after: u64, timeout: Duration) -> Option<Arc<Frame>>;
    /// Posición absoluta dentro del monitor del móvil + botones al estilo RFB
    /// (bit0 izq, bit1 centro, bit2 der, bit3/4 rueda arriba/abajo, bit5/6 rueda izq/der).
    fn pointer(&self, x: u16, y: u16, mask: u8);
    /// Tecla por keysym X11 (lo que manda RFB).
    fn key(&self, keysym: u32, down: bool);
}

/// Hueco del último fotograma, compartido entre la captura y el codificador.
#[derive(Default)]
pub struct FrameSlot {
    inner: Mutex<Option<Arc<Frame>>>,
    cv: Condvar,
}

impl FrameSlot {
    pub fn publish(&self, mut f: Frame) {
        let mut g = self.inner.lock().unwrap();
        f.seq = g.as_ref().map(|p| p.seq + 1).unwrap_or(1);
        *g = Some(Arc::new(f));
        self.cv.notify_all();
    }

    pub fn wait(&self, after: u64, timeout: Duration) -> Option<Arc<Frame>> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(f) = g.as_ref() {
                if f.seq > after {
                    return Some(f.clone());
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            g = self.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
}

/// Abre el monitor virtual del móvil en este sistema.
pub fn open(width: u32, height: u32) -> Result<Arc<dyn Display>, String> {
    #[cfg(target_os = "linux")]
    {
        return crate::linux::open(width, height);
    }
    #[cfg(windows)]
    {
        return crate::windows::open(width, height);
    }
    #[allow(unreachable_code)]
    Err(format!("sistema no soportado ({width}x{height})"))
}
