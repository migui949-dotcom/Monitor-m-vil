//! Imágenes de la cámara del móvil (común a Linux y Windows): trozos H.264 → imagen completa,
//! giro y encaje en 1280x720 YUV420.

pub const W: usize = 1280;
pub const H: usize = 720;
pub const FRAME: usize = W * H * 3 / 2;
pub const CHUNK: usize = 60_000; // la app parte cada imagen en trozos de este tamaño

pub fn has_idr(b: &[u8]) -> bool {
    openh264::nal_units(b).any(|n| {
        let i = n.iter().position(|x| *x != 0).map(|p| p + 1).unwrap_or(0); // tras 00 00 (00) 01
        n.get(i).map(|h| matches!(h & 0x1f, 5 | 7)).unwrap_or(false)
    })
}

/// Plano w×h (con stride) → girado `rot` grados horarios → encajado en PW×PH (bandas de `fill`)
#[allow(clippy::too_many_arguments)]
fn plane(src: &[u8], stride: usize, w: usize, h: usize, rot: i32, pw: usize, ph: usize, fill: u8, out: &mut Vec<u8>) {
    let (rw, rh) = if rot == 90 || rot == 270 { (h, w) } else { (w, h) };
    // escala para encajar (sin deformar), vecino más próximo
    let sc = (pw as f32 / rw as f32).min(ph as f32 / rh as f32);
    let (nw, nh) = (((rw as f32 * sc) as usize).max(1).min(pw), ((rh as f32 * sc) as usize).max(1).min(ph));
    let (x0, y0) = ((pw - nw) / 2, (ph - nh) / 2);
    let start = out.len();
    out.resize(start + pw * ph, fill);
    let o = &mut out[start..];
    let xs: Vec<usize> = (0..nw).map(|x| x * rw / nw).collect();
    for y in 0..nh {
        let ry = y * rh / nh;
        let row = &mut o[(y0 + y) * pw + x0..(y0 + y) * pw + x0 + nw];
        for (x, px) in row.iter_mut().enumerate() {
            let rx = xs[x];
            // (rx, ry) en la imagen girada → (sx, sy) en la original
            let (sx, sy) = match rot {
                90 => (ry, h - 1 - rx),
                180 => (w - 1 - rx, h - 1 - ry),
                270 => (w - 1 - ry, rx),
                _ => (rx, ry),
            };
            *px = src[sy * stride + sx];
        }
    }
}

pub fn to_webcam(y: &[u8], u: &[u8], v: &[u8], s: (usize, usize, usize), w: usize, h: usize, rot: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME);
    if rot == 0 && (w, h) == (W, H) {
        // lo normal: solo quitar el relleno de las filas
        for (p, st, pw, ph) in [(y, s.0, W, H), (u, s.1, W / 2, H / 2), (v, s.2, W / 2, H / 2)] {
            for r in 0..ph {
                out.extend_from_slice(&p[r * st..r * st + pw]);
            }
        }
        return out;
    }
    plane(y, s.0, w, h, rot, W, H, 16, &mut out);
    plane(u, s.1, w / 2, h / 2, rot, W / 2, H / 2, 128, &mut out);
    plane(v, s.2, w / 2, h / 2, rot, W / 2, H / 2, 128, &mut out);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn giro_y_encaje() {
        // 4x2 con un punto arriba a la izquierda; girado 90° horario → arriba a la derecha
        let src = [200u8, 0, 0, 0, 0, 0, 0, 0];
        let mut out = vec![];
        super::plane(&src, 4, 4, 2, 90, 2, 4, 9, &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(out[1], 200); // fila 0, columna derecha
        let mut o2 = vec![];
        super::plane(&src, 4, 4, 2, 0, 8, 2, 9, &mut o2); // 4x2 en 8x2: bandas a los lados
        assert_eq!(&o2[0..2], &[9, 9]);
        assert_eq!(o2[2], 200);
    }
}
