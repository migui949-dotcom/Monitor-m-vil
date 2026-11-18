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
    /// cuándo se capturó (para medir cuánto tarda en salir hacia el móvil)
    pub at: Instant,
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
    /// Pellizco con dos dedos en el móvil: `fase` 0 empieza (en x, y), 1 sigue, 2 acaba;
    /// `escala` = distancia actual entre los dedos / la del principio.
    fn pinch(&self, _fase: u8, _x: u16, _y: u16, _escala: f32) {}
    /// Texto del teclado del móvil ('\n' = Intro, '\u{8}' = borrar, '\u{7f}' = Supr).
    fn text(&self, s: &str) {
        type_keys(self, s);
    }
    /// Mover con dos dedos la ventana que hay en (x, y): fase 0 la coge, 1 la lleva, 2 la suelta.
    /// (En Linux no hace falta: la app usa Super + arrastrar.)
    fn window_drag(&self, _fase: u8, _x: u16, _y: u16) {}
    /// Toque con dos dedos en Windows: maximizar / restaurar la ventana que hay en (x, y).
    fn window_toggle(&self, _x: u16, _y: u16) {}
    /// Avisar (true/false) cuando una app del monitor del móvil quiere o deja de querer teclado.
    fn on_keyboard(&self, _f: Box<dyn Fn(bool) + Send + Sync>) {}
}

/// keysym X11 de un carácter del teclado del móvil
pub fn keysym_of(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\u{8}' => 0xff08,
        '\t' => 0xff09,
        '\u{7f}' => 0xffff,
        c if (c as u32) >= 0x20 && (c as u32) <= 0xff => c as u32,
        c => 0x0100_0000 + c as u32,
    }
}

/// Escribe `s` pulsando teclas (lo que se hace si no hay otra forma de mandar texto)
pub fn type_keys<D: Display + ?Sized>(d: &D, s: &str) {
    for c in s.chars() {
        let k = keysym_of(c);
        d.key(k, true);
        d.key(k, false);
    }
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

/// Abre el monitor virtual del móvil de la ranura `slot` en este sistema.
pub fn open(width: u32, height: u32, slot: u8) -> Result<Arc<dyn Display>, String> {
    #[cfg(target_os = "linux")]
    {
        return crate::linux::open(width, height, slot);
    }
    #[cfg(windows)]
    {
        let _ = slot;
        return crate::windows::open(width, height);
    }
    #[allow(unreachable_code)]
    Err(format!("sistema no soportado ({width}x{height}, {slot})"))
}
