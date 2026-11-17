//! Vídeo del escritorio: H.264 → MediaCodec (chip de vídeo del móvil) → ImageReader → GPU sin copias.
//!
//! El decodificador entrega cada imagen en un AHardwareBuffer; en el hilo de dibujo de egui se
//! convierte en EGLImage, se enlaza a una textura externa (OES) y se pinta en un rectángulo.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glow::HasContext;
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;

// ───────────── FFI de EGL / GLES (extensiones de Android) ─────────────
type EGLDisplay = *mut c_void;
type EGLImage = *mut c_void;
const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
const EGL_NONE: i32 = 0x3038;
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetCurrentDisplay() -> EGLDisplay;
    fn eglGetNativeClientBufferANDROID(buffer: *const c_void) -> *mut c_void;
    fn eglCreateImageKHR(dpy: EGLDisplay, ctx: *mut c_void, target: u32, buffer: *mut c_void, attrib: *const i32) -> EGLImage;
    fn eglDestroyImageKHR(dpy: EGLDisplay, image: EGLImage) -> u32;
}
#[link(name = "GLESv2")]
unsafe extern "C" {
    fn glEGLImageTargetTexture2DOES(target: u32, image: EGLImage);
}

// Los objetos del NDK no se declaran Send/Sync, pero MediaCodec e ImageReader se pueden usar
// desde varios hilos (entrada/salida del códec en hilos distintos es lo habitual).
struct SendCodec(MediaCodec);
unsafe impl Send for SendCodec {}
unsafe impl Sync for SendCodec {}
pub struct SendReader(ImageReader);
unsafe impl Send for SendReader {}
unsafe impl Sync for SendReader {}

struct Decoder {
    codec: Arc<SendCodec>,
    reader: Arc<SendReader>,
    stop: Arc<AtomicBool>,
    pts: u64,
}

pub struct Video {
    dec: Mutex<Option<Decoder>>,
    pub size: Mutex<(i32, i32)>,
    pub frames: Arc<AtomicU64>,
    pub ctx: Mutex<Option<eframe::egui::Context>>,
}

impl Video {
    pub fn new() -> Video {
        Video { dec: Mutex::new(None), size: Mutex::new((0, 0)), frames: Arc::new(AtomicU64::new(0)), ctx: Mutex::new(None) }
    }

    /// (Re)crea el decodificador para imágenes de w×h
    pub fn configure(&self, w: i32, h: i32) {
        self.stop();
        *self.size.lock().unwrap() = (w, h);
        let reader = match ImageReader::new_with_usage(w, h, ImageFormat::PRIVATE, HardwareBufferUsage::GPU_SAMPLED_IMAGE, 4) {
            Ok(r) => Arc::new(SendReader(r)),
            Err(e) => { log::error!("ImageReader: {e:?}"); return; }
        };
        let window = match reader.0.window() { Ok(w) => w, Err(e) => { log::error!("ventana: {e:?}"); return; } };
        let Some(codec) = MediaCodec::from_decoder_type("video/avc") else { log::error!("sin decodificador H.264"); return; };
        let mut f = MediaFormat::new();
        f.set_str("mime", "video/avc");
        f.set_i32("width", w);
        f.set_i32("height", h);
        f.set_i32("max-input-size", 4 << 20);
        // Poco retraso: claves estándar (Android 11+) y las de Qualcomm (Snapdragon 845)
        f.set_i32("low-latency", 1);
        f.set_i32("priority", 0);
        f.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        f.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        if let Err(e) = codec.configure(&f, Some(&window), MediaCodecDirection::Decoder) { log::error!("configure: {e:?}"); return; }
        if let Err(e) = codec.start() { log::error!("start: {e:?}"); return; }
        let codec = Arc::new(SendCodec(codec));
        let stop = Arc::new(AtomicBool::new(false));

        // Hilo de salida: cada imagen decodificada se "renderiza" al ImageReader y se pide repintar
        {
            let (codec, stop) = (codec.clone(), stop.clone());
            let ctx = self.ctx.lock().unwrap().clone();
            let frames = self.frames.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match codec.0.dequeue_output_buffer(Duration::from_millis(20)) {
                        Ok(DequeuedOutputBufferInfoResult::Buffer(b)) => {
                            let n = frames.load(Ordering::Relaxed);
                            if n < 3 || n % 300 == 0 { log::info!("decodificada imagen #{n}"); }
                            let _ = codec.0.release_output_buffer(b, true);
                            frames.fetch_add(1, Ordering::Relaxed);
                            if let Some(c) = &ctx { c.request_repaint(); }
                        }
                        Ok(_) => {}
                        Err(e) => { log::warn!("salida del códec: {e:?}"); std::thread::sleep(Duration::from_millis(10)); }
                    }
                }
                let _ = codec.0.stop();
            });
        }
        *self.dec.lock().unwrap() = Some(Decoder { codec, reader, stop, pts: 0 });
    }

    /// Mete un trozo de flujo H.264 (una actualización de wayvnc) en el decodificador
    pub fn feed(&self, data: &[u8], _reset: bool) {
        static F: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let f = F.fetch_add(1, Ordering::Relaxed);
        if f < 5 || f % 300 == 0 { log::info!("feed #{f}: {} bytes", data.len()); }
        let mut g = self.dec.lock().unwrap();
        let Some(d) = g.as_mut() else { return };
        match d.codec.0.dequeue_input_buffer(Duration::from_millis(40)) {
            Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                let dst = buf.buffer_mut();
                let n = data.len().min(dst.len());
                for (o, i) in dst[..n].iter_mut().zip(&data[..n]) { o.write(*i); }
                d.pts += 16_666;
                if let Err(e) = d.codec.0.queue_input_buffer(buf, 0, n, d.pts, 0) { log::warn!("queue: {e:?}"); }
            }
            Ok(DequeuedInputBufferResult::TryAgainLater) => log::warn!("decodificador lleno: imagen descartada"),
            Err(e) => log::warn!("entrada del códec: {e:?}"),
        }
    }

    pub fn reader(&self) -> Option<Arc<SendReader>> {
        self.dec.lock().unwrap().as_ref().map(|d| d.reader.clone())
    }

    pub fn stop(&self) {
        if let Some(d) = self.dec.lock().unwrap().take() {
            d.stop.store(true, Ordering::Relaxed);
        }
    }
}

// ───────────── Dibujo (solo en el hilo de GL de egui) ─────────────
struct Frame {
    _image: Image,
    egl: EGLImage,
    _reader: Arc<SendReader>,       // el ImageReader no se libera mientras se muestre su imagen
}

pub struct Renderer {
    pub gl_id: usize,
    program: glow::Program,
    vao: glow::VertexArray,
    tex: glow::Texture,
    frame: Option<Frame>,
}
// Solo se usa dentro del callback de pintado (hilo de GL); el Mutex que lo envuelve exige Send.
unsafe impl Send for Renderer {}

const VS: &str = r#"#version 300 es
out vec2 uv;
void main() {
    vec2 p = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}"#;
const FS: &str = r#"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
in vec2 uv;
uniform samplerExternalOES tex;
out vec4 color;
void main() { color = texture(tex, uv); }"#;

impl Renderer {
    pub fn new(gl: &glow::Context, gl_id: usize) -> Renderer {
        log::info!("renderer: creando");
        unsafe {
            let program = gl.create_program().unwrap();
            for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
                let s = gl.create_shader(kind).unwrap();
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) { log::error!("shader: {}", gl.get_shader_info_log(s)); }
                gl.attach_shader(program, s);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) { log::error!("programa: {}", gl.get_program_info_log(program)); }
            let vao = gl.create_vertex_array().unwrap();
            let tex = gl.create_texture().unwrap();
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(tex));
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
            log::info!("renderer: creado");
            Renderer { gl_id, program, vao, tex, frame: None }
        }
    }

    pub fn paint(&mut self, gl: &glow::Context, video: &Video) {
        static P: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let p = P.fetch_add(1, Ordering::Relaxed);
        if p < 5 || p % 300 == 0 {
            log::info!("paint llamada #{p}: lector={} imagen_actual={} imágenes decodificadas={}",
                       video.reader().is_some(), self.frame.is_some(), video.frames.load(Ordering::Relaxed));
        }
        unsafe {
            // ¿Imagen nueva del decodificador?
            if let Some(reader) = video.reader() {
                let acq = reader.0.acquire_latest_image();
                static K: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let k = K.fetch_add(1, Ordering::Relaxed);
                if k < 5 || k % 300 == 0 { log::info!("paint #{k}: acquire = {:?}", acq.as_ref().map(|r| matches!(r, AcquireResult::Image(_)))); }
                if let Ok(AcquireResult::Image(img)) = acq {
                    if let Ok(hb) = img.hardware_buffer() {
                        let dpy = eglGetCurrentDisplay();
                        let client = eglGetNativeClientBufferANDROID(hb.as_ptr() as *const c_void);
                        let attrs = [EGL_IMAGE_PRESERVED_KHR, 1, EGL_NONE];
                        let egl = eglCreateImageKHR(dpy, std::ptr::null_mut(), EGL_NATIVE_BUFFER_ANDROID, client, attrs.as_ptr());
                        if k < 5 { log::info!("paint: EGLImage {:?}", egl); }
                        if !egl.is_null() {
                            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.tex));
                            glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, egl);
                            if let Some(old) = self.frame.take() { eglDestroyImageKHR(dpy, old.egl); }
                            self.frame = Some(Frame { _image: img, egl, _reader: reader.clone() });
                        }
                    }
                }
            }
            if self.frame.is_none() { return; }
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.tex));
            if let Some(loc) = gl.get_uniform_location(self.program, "tex") { gl.uniform_1_i32(Some(&loc), 0); }
            gl.disable(glow::BLEND);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
            gl.bind_vertex_array(None);
        }
    }

    /// Suelta la imagen actual (al desconectar)
    pub fn clear(&mut self) {
        if let Some(old) = self.frame.take() {
            unsafe { eglDestroyImageKHR(eglGetCurrentDisplay(), old.egl); }
        }
    }
}
