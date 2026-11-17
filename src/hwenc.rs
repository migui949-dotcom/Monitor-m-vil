//! Compresión H.264 por hardware con el `ffmpeg` del sistema (VAAPI en Intel/AMD, NVENC en NVIDIA).
//!
//! ffmpeg va como proceso aparte: le llega cada imagen BGRX cruda por stdin y devuelve el H.264 por
//! RTP a un puerto UDP local. RTP marca el último paquete de cada imagen ("marker"), así se sabe dónde
//! acaba sin esperar a la siguiente (con un flujo Annex B por tubería habría que esperar al siguiente
//! inicio y eso añade una imagen de retardo). Con `-async_depth 1` / `-zerolatency` cada imagen sale
//! antes de mandar la siguiente: medido en el PC (N95), ~12 ms y ~31 % de un núcleo a 60 fps, frente a
//! ~21 ms y ~130 % con OpenH264.
//!
//! Si no hay ffmpeg, ni hardware, o algo falla, quien lo usa (video::Encoder) vuelve a OpenH264.

use std::io::Write;
use std::net::UdpSocket;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Nvenc,
    VaapiLp,
    Vaapi,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Nvenc => "NVENC",
            Kind::VaapiLp => "VAAPI (bajo consumo)",
            Kind::Vaapi => "VAAPI",
        }
    }
}

/// Dispositivo VAAPI: el primer renderD* de Intel o AMD (en equipos con NVIDIA + Intel, la Intel)
fn vaapi_device() -> Option<String> {
    let mut v: Vec<_> = std::fs::read_dir("/dev/dri").ok()?.flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    v.sort();
    v.into_iter().find(|n| {
        let drv = std::fs::read_link(format!("/sys/class/drm/{n}/device/driver"))
            .map(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())
            .unwrap_or_default();
        matches!(drv.as_str(), "i915" | "xe" | "amdgpu" | "radeon")
    }).map(|n| format!("/dev/dri/{n}"))
}

fn has_ffmpeg() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("ffmpeg").is_file()))
        .unwrap_or(false)
}

/// Qué probar, en orden (ENCODER= en host.conf: auto | software | nvenc | vaapi)
pub fn candidates(pref: &str) -> Vec<Kind> {
    if pref == "software" || !has_ffmpeg() {
        return vec![];
    }
    let nvidia = std::path::Path::new("/dev/nvidiactl").exists();
    let vaapi = vaapi_device().is_some();
    let mut v = vec![];
    if nvidia && matches!(pref, "auto" | "nvenc") {
        v.push(Kind::Nvenc);
    }
    if vaapi && matches!(pref, "auto" | "vaapi") {
        v.push(Kind::VaapiLp); // Intel: bloque de bajo consumo (más rápido y gasta menos)
        v.push(Kind::Vaapi); // AMD o Intel sin él
    }
    v
}

pub struct HwEnc {
    kind: Kind,
    w: usize,
    h: usize,
    fps: f32,
    bps: u32,
    child: Option<Child>,
    sock: UdpSocket,
    sent: u64,
    buf: Vec<u8>,
}

impl HwEnc {
    pub fn new(kind: Kind, w: usize, h: usize, fps: f32, bps: u32) -> Result<Self, String> {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).map_err(|e| format!("udp: {e}"))?;
        set_rcvbuf(&sock, 8 << 20);
        let mut e = HwEnc { kind, w, h, fps, bps, child: None, sock, sent: 0, buf: Vec::new() };
        e.spawn()?;
        Ok(e)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    fn spawn(&mut self) -> Result<(), String> {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // lo que quedara por llegar del proceso anterior
        let _ = self.sock.set_nonblocking(true);
        let mut junk = [0u8; 2048];
        while self.sock.recv(&mut junk).is_ok() {}
        let _ = self.sock.set_nonblocking(false);

        let port = self.sock.local_addr().map_err(|e| e.to_string())?.port();
        let (w, h, fps, bps) = (self.w, self.h, self.fps, self.bps);
        let mut a: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostdin".into()];
        if matches!(self.kind, Kind::Vaapi | Kind::VaapiLp) {
            a.extend(["-vaapi_device".into(), vaapi_device().ok_or("sin dispositivo VAAPI")?]);
        }
        a.extend([
            "-f", "rawvideo", "-pix_fmt", "bgr0", "-s", &format!("{w}x{h}"), "-framerate", &format!("{fps}"), "-i", "pipe:0",
        ].map(String::from));
        let rate = [format!("{bps}"), format!("{}", bps / 2)];
        match self.kind {
            Kind::Vaapi | Kind::VaapiLp => a.extend([
                "-vf", "format=bgr0,hwupload,scale_vaapi=format=nv12:out_color_matrix=smpte170m:out_range=tv",
                "-c:v", "h264_vaapi", "-low_power", if self.kind == Kind::VaapiLp { "1" } else { "0" },
                "-async_depth", "1", "-profile:v", "constrained_baseline", "-bf", "0", "-g", "100000",
                "-rc_mode", "CBR", "-b:v", &rate[0], "-bufsize", &rate[1],
            ].map(String::from)),
            Kind::Nvenc => a.extend([
                "-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ull", "-zerolatency", "1", "-delay", "0",
                "-profile:v", "baseline", "-bf", "0", "-g", "100000", "-forced-idr", "1",
                "-rc", "cbr", "-b:v", &rate[0], "-bufsize", &rate[1],
            ].map(String::from)),
        }
        a.extend(["-flush_packets", "1", "-f", "rtp", &format!("rtp://127.0.0.1:{port}?pkt_size=1400")].map(String::from));
        let child = Command::new("ffmpeg")
            .args(&a)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        if let Some(stdin) = child.stdin.as_ref() {
            use std::os::fd::AsRawFd;
            // tubería grande: menos cambios de contexto al pasar cada imagen (4 MB)
            unsafe { libc::fcntl(stdin.as_raw_fd(), 1031 /* F_SETPIPE_SZ */, 1 << 20) };
        }
        self.child = Some(child);
        self.sent = 0;
        Ok(())
    }

    /// Una imagen BGRX (con su stride) → una imagen H.264 Annex B. `idr`: hace falta una imagen
    /// completa (cliente nuevo o que lo pide): se arranca ffmpeg de nuevo (empieza siempre por una).
    pub fn encode(&mut self, data: &[u8], stride: usize, idr: bool) -> Result<Vec<u8>, String> {
        if idr && self.sent > 0 {
            self.spawn()?;
        }
        let row = self.w * 4;
        let stdin = self.child.as_mut().and_then(|c| c.stdin.as_mut()).ok_or("ffmpeg sin entrada")?;
        let res = if stride == row && data.len() >= row * self.h {
            stdin.write_all(&data[..row * self.h])
        } else {
            self.buf.clear();
            for y in 0..self.h {
                self.buf.extend_from_slice(&data[y * stride..y * stride + row]);
            }
            stdin.write_all(&self.buf)
        };
        res.map_err(|e| format!("ffmpeg se ha cerrado ({e})"))?;
        // el primer paquete tarda más (ffmpeg arrancando y abriendo el hardware)
        let wait = if self.sent == 0 { Duration::from_secs(5) } else { Duration::from_millis(1000) };
        let out = self.read_frame(wait)?;
        self.sent += 1;
        Ok(out)
    }

    /// Junta los paquetes RTP (RFC 6184: NAL suelta, STAP-A, FU-A) hasta el "marker" → Annex B
    fn read_frame(&mut self, wait: Duration) -> Result<Vec<u8>, String> {
        let _ = self.sock.set_read_timeout(Some(wait));
        let mut out = Vec::with_capacity(64 * 1024);
        let mut pkt = vec![0u8; 65536];
        loop {
            let n = match self.sock.recv(&mut pkt) {
                Ok(n) => n,
                Err(_) => {
                    let dead = self.child.as_mut().map(|c| matches!(c.try_wait(), Ok(Some(_)))).unwrap_or(true);
                    return Err(if dead { "ffmpeg ha terminado (¿sin soporte de hardware?)".into() } else { "ffmpeg no responde".into() });
                }
            };
            let p = &pkt[..n];
            if n < 13 || p[0] >> 6 != 2 {
                continue;
            }
            let mut hdr = 12 + 4 * (p[0] & 0x0f) as usize;
            if p[0] & 0x10 != 0 && n >= hdr + 4 {
                hdr += 4 + 4 * u16::from_be_bytes([p[hdr + 2], p[hdr + 3]]) as usize;
            }
            let end = if p[0] & 0x20 != 0 { n.saturating_sub(p[n - 1] as usize) } else { n };
            if hdr >= end {
                continue;
            }
            let pl = &p[hdr..end];
            match pl[0] & 0x1f {
                1..=23 => {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(pl);
                }
                24 => {
                    let mut i = 1;
                    while i + 2 <= pl.len() {
                        let sz = u16::from_be_bytes([pl[i], pl[i + 1]]) as usize;
                        i += 2;
                        if i + sz > pl.len() {
                            break;
                        }
                        out.extend_from_slice(&[0, 0, 0, 1]);
                        out.extend_from_slice(&pl[i..i + sz]);
                        i += sz;
                    }
                }
                28 if pl.len() > 2 => {
                    if pl[1] & 0x80 != 0 {
                        out.extend_from_slice(&[0, 0, 0, 1, (pl[0] & 0xe0) | (pl[1] & 0x1f)]);
                    }
                    out.extend_from_slice(&pl[2..]);
                }
                _ => {}
            }
            if p[1] & 0x80 != 0 {
                return Ok(out);
            }
        }
    }
}

impl Drop for HwEnc {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            drop(c.stdin.take());
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn set_rcvbuf(s: &UdpSocket, size: i32) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, &size as *const i32 as *const libc::c_void, 4);
    }
}
