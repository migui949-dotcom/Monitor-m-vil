//! Bench del camino de vídeo actual, sin servicio ni móvil: imágenes reales que se desplazan
//! (como al hacer scroll) → BGRX→I420 + OpenH264 (lo mismo que video.rs).
//!   bench <imagen.bgra> <ancho> <alto> [imágenes=300] [px por imagen=6] [auto|software|vaapi|nvenc]
//! La imagen de entrada es BGRX de ancho × (alto*2) (se recorre en vertical).

#[path = "../video.rs"]
mod video;
#[path = "../hwenc.rs"]
mod hwenc;

/// Lo poco que video.rs/hwenc.rs usan del resto del programa
mod host {
    pub fn log(m: &str) {
        eprintln!("[bench] {m}");
    }
}
mod util {
    pub fn conf(_: &str) -> Option<String> {
        std::env::args().nth(6)
    }
}

mod display {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PixelOrder {
        Bgrx,
        #[allow(dead_code)]
        Rgbx,
    }
    pub struct Frame {
        pub width: usize,
        pub height: usize,
        pub stride: usize,
        pub order: PixelOrder,
        pub data: Vec<u8>,
        #[allow(dead_code)]
        pub seq: u64,
        #[allow(dead_code)]
        pub at: std::time::Instant,
    }
}

use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let src = std::fs::read(&a[1]).expect("imagen");
    let (w, h): (usize, usize) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let n: usize = a.get(4).and_then(|v| v.parse().ok()).unwrap_or(300);
    let step: usize = a.get(5).and_then(|v| v.parse().ok()).unwrap_or(6);
    assert!(src.len() >= w * h * 2 * 4, "la imagen debe ser {w}x{} BGRX", h * 2);
    let mut enc = video::Encoder::new(w, h, 60.0).unwrap();
    let name = enc.name();
    let (mut t_conv_enc, mut sizes) = (Vec::new(), Vec::new());
    let cpu0 = cpu_time();
    let t0 = Instant::now();
    for i in 0..n {
        let off = (i * step) % h;
        let data = src[off * w * 4..(off + h) * w * 4].to_vec(); // la copia que hace la captura
        let f = display::Frame { width: w, height: h, stride: w * 4, order: display::PixelOrder::Bgrx, data, seq: i as u64, at: Instant::now() };
        let t = Instant::now();
        let out = enc.encode(&f, i == 0).unwrap();
        if i == 0 {
            // el arranque (ffmpeg abriendo el hardware) no cuenta
            t_conv_enc.clear();
            sizes.clear();
            continue;
        }
        t_conv_enc.push(t.elapsed().as_secs_f64() * 1000.0);
        sizes.push(out.len());
    }
    drop(enc); // espera a ffmpeg para poder contar su CPU
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_time() - cpu0 + children_cpu();
    let n = t_conv_enc.len();
    t_conv_enc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = t_conv_enc.iter().sum::<f64>() / n as f64;
    let p95 = t_conv_enc[(n as f64 * 0.95) as usize - 1];
    let kb = sizes.iter().sum::<usize>() as f64 / n as f64 / 1024.0;
    // CPU a 60 imágenes/s = tiempo de CPU por imagen × 60 (puede pasar de 100 %: el codificador usa 2 hilos)
    println!("{name} {w}x{h}: {mean:.2} ms/imagen (p95 {p95:.2}) · {kb:.1} kB/imagen · {:.0} fps máx · CPU a 60 fps: {:.0}% de un núcleo",
             n as f64 / wall, cpu / n as f64 * 60.0 * 100.0);
}

/// CPU de los hijos ya terminados (ffmpeg): se cierra el codificador antes de medir
fn children_cpu() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 / 1e6 + ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 / 1e6
}

/// Tiempo de CPU del proceso (s)
fn cpu_time() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = s.rsplit_once(')').map(|x| x.1).unwrap_or("").split_whitespace().collect();
    let tck = 100.0;
    (f.get(11).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) + f.get(12).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)) / tck
}
