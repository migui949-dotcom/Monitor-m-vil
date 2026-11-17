//! Cliente de prueba que se comporta como la app: descubre, se conecta con Noise, pide la pantalla,
//! decodifica el H.264 y guarda una captura (PPM). También mueve el ratón y pulsa una tecla.
//!   prueba <host:puerto> <salida.ppm> [segundos]

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn rf(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut l = [0u8; 2];
    s.read_exact(&mut l).ok()?;
    let mut b = vec![0u8; u16::from_be_bytes(l) as usize];
    s.read_exact(&mut b).unwrap();
    Some(b)
}

fn wf(s: &mut TcpStream, d: &[u8]) {
    let mut m = (d.len() as u16).to_be_bytes().to_vec();
    m.extend_from_slice(d);
    s.write_all(&m).unwrap();
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let addr = &a[1];
    let out = &a[2];
    let secs: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);

    // modo información
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"I").unwrap();
    let mut info = String::new();
    s.read_to_string(&mut info).unwrap();
    println!("info: {info}");

    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"N").unwrap();
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).unwrap();
    let mut hs = snow::Builder::new("Noise_XX_25519_ChaChaPoly_BLAKE2s".parse().unwrap()).local_private_key(&key).build_initiator().unwrap();
    let mut buf = vec![0u8; 65535];
    let n = hs.write_message(&[], &mut buf).unwrap();
    wf(&mut s, &buf[..n]);
    hs.read_message(&rf(&mut s).unwrap(), &mut buf).unwrap();
    let n = hs.write_message(&[], &mut buf).unwrap();
    wf(&mut s, &buf[..n]);
    let mut t = hs.into_transport_mode().unwrap();

    let mut send = |s: &mut TcpStream, t: &mut snow::TransportState, ch: u8, d: &[u8]| {
        let mut p = vec![ch];
        p.extend_from_slice(d);
        let n = t.write_message(&p, &mut buf).unwrap();
        let c = buf[..n].to_vec();
        wf(s, &c);
    };
    // PRUEBA_DEVICE / PRUEBA_WANT (p. ej. "screen,audio") para simular varios móviles distintos
    let device = std::env::var("PRUEBA_DEVICE").unwrap_or_else(|_| "Prueba".into());
    let want: Vec<String> = std::env::var("PRUEBA_WANT").unwrap_or_else(|_| "screen".into()).split(',').map(|w| format!("\"{w}\"")).collect();
    let hello = format!(r#"{{"t":"hello","device":"{device}","want":[{}]}}"#, want.join(","));
    send(&mut s, &mut t, 0, hello.as_bytes());

    let mut dec = Decoder::new().unwrap();
    let mut rfb: Vec<u8> = vec![];
    let mut stage = 0;
    let mut frames = 0;
    let mut bytes = 0usize;
    let mut last_img: Option<(usize, usize, Vec<u8>)> = None;
    let t0 = Instant::now();
    let mut started: Option<Instant> = None;
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut pbuf = vec![0u8; 65535];
    let mut moved = false;
    loop {
        if started.map(|x| x.elapsed().as_secs() >= secs).unwrap_or(false) {
            break;
        }
        // pantalla quieta (sin imágenes nuevas): se termina con lo que haya
        let Some(c) = rf(&mut s) else { println!("(sin imágenes nuevas en 10 s)"); break };
        let n = t.read_message(&c, &mut pbuf).unwrap();
        let m = &pbuf[..n];
        if m[0] == 0 {
            println!("control: {}", String::from_utf8_lossy(&m[1..]));
            continue;
        }
        if m[0] != 1 {
            continue;
        }
        rfb.extend_from_slice(&m[1..]);
        loop {
            match stage {
                0 if rfb.len() >= 12 => {
                    rfb.drain(..12);
                    send(&mut s, &mut t, 1, b"RFB 003.008\n");
                    stage = 1;
                }
                1 if rfb.len() >= 2 => {
                    rfb.drain(..2);
                    send(&mut s, &mut t, 1, &[1]);
                    stage = 2;
                }
                2 if rfb.len() >= 4 => {
                    rfb.drain(..4);
                    send(&mut s, &mut t, 1, &[1]);
                    stage = 3;
                }
                3 if rfb.len() >= 24 => {
                    let nl = u32::from_be_bytes(rfb[20..24].try_into().unwrap()) as usize;
                    if rfb.len() < 24 + nl {
                        break;
                    }
                    let w = u16::from_be_bytes([rfb[0], rfb[1]]);
                    let h = u16::from_be_bytes([rfb[2], rfb[3]]);
                    println!("ServerInit {w}x{h} «{}»", String::from_utf8_lossy(&rfb[24..24 + nl]));
                    rfb.drain(..24 + nl);
                    send(&mut s, &mut t, 1, &[2, 0, 0, 2, 0, 0, 0, 0x32, 0xff, 0xff, 0xff, 0x21]);
                    let mut req = vec![3, 0, 0, 0, 0, 0];
                    req.extend_from_slice(&w.to_be_bytes());
                    req.extend_from_slice(&h.to_be_bytes());
                    send(&mut s, &mut t, 1, &req);
                    stage = 4;
                }
                4 if rfb.len() >= 4 + 12 + 8 => {
                    assert_eq!(rfb[0], 0);
                    let enc = i32::from_be_bytes(rfb[12..16].try_into().unwrap());
                    let len = u32::from_be_bytes(rfb[16..20].try_into().unwrap()) as usize;
                    if rfb.len() < 24 + len {
                        break;
                    }
                    assert_eq!(enc, 50);
                    let data: Vec<u8> = rfb[24..24 + len].to_vec();
                    rfb.drain(..24 + len);
                    frames += 1;
                    bytes += len;
                    if started.is_none() {
                        started = Some(Instant::now());
                        println!("primer fotograma a los {:?} ({} bytes)", t0.elapsed(), len);
                    }
                    for nal in openh264::nal_units(&data) {
                        if let Ok(Some(img)) = dec.decode(nal) {
                            let (w, h) = img.dimensions();
                            let mut rgb = vec![0u8; w * h * 3];
                            img.write_rgb8(&mut rgb);
                            last_img = Some((w, h, rgb));
                        }
                    }
                    // pide el siguiente (incremental)
                    send(&mut s, &mut t, 1, &[3, 1, 0, 0, 0, 0, 0x05, 0xa0, 0x02, 0xd0]);
                    if !moved {
                        moved = true;
                        // mueve el ratón al centro del monitor del móvil
                        send(&mut s, &mut t, 1, &[5, 0, 0x02, 0xd0, 0x01, 0x68]);
                    }
                }
                _ => break,
            }
        }
    }
    let dt = started.map(|x| x.elapsed().as_secs_f64()).unwrap_or(1.0);
    println!("{frames} fotogramas en {dt:.1} s ({:.1} fps, {:.0} kB/s)", frames as f64 / dt, bytes as f64 / dt / 1000.0);
    send(&mut s, &mut t, 0, br#"{"t":"bye"}"#);
    if let Some((w, h, rgb)) = last_img {
        let mut f = std::fs::File::create(out).unwrap();
        write!(f, "P6\n{w} {h}\n255\n").unwrap();
        f.write_all(&rgb).unwrap();
        println!("captura: {out} ({w}x{h})");
    }
}
