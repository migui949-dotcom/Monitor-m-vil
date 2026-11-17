//! Cámara del móvil → codificador H.264 por hardware → equipo (canal 4).
//!
//! Camera2 del NDK escribe directamente en la superficie de entrada del codificador (sin copias).
//! A diferencia del navegador, aquí se elige el rango de fps: se prefiere uno FIJO (p. ej. 15–15 en la
//! cámara delantera), que con poca luz da fluidez constante en vez de caer a 10 fps.

use std::ffi::{c_void, CStr};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ndk::media::media_codec::{DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use ndk_sys as sys;

use crate::link::Link;

pub const CH_CAM: u8 = 4;
const W: i32 = 1280;
const H: i32 = 720;

struct SendCodec(MediaCodec);
unsafe impl Send for SendCodec {}
unsafe impl Sync for SendCodec {}

pub struct Camera {
    mgr: *mut sys::ACameraManager,
    dev: *mut sys::ACameraDevice,
    session: *mut sys::ACameraCaptureSession,
    request: *mut sys::ACaptureRequest,
    target: *mut sys::ACameraOutputTarget,
    container: *mut sys::ACaptureSessionOutputContainer,
    sess_out: *mut sys::ACaptureSessionOutput,
    _window: NativeWindow,
    codec: Arc<SendCodec>,
    stop: Arc<AtomicBool>,
    pub facing: String,
}
unsafe impl Send for Camera {}

unsafe extern "C" fn dev_disconnected(_: *mut c_void, _: *mut sys::ACameraDevice) { log::warn!("cámara: desconectada"); }
unsafe extern "C" fn dev_error(_: *mut c_void, _: *mut sys::ACameraDevice, e: i32) { log::error!("cámara: error {e}"); }
unsafe extern "C" fn sess_noop(_: *mut c_void, _: *mut sys::ACameraCaptureSession) {}

static mut DEV_CB: sys::ACameraDevice_StateCallbacks = sys::ACameraDevice_StateCallbacks {
    context: null_mut(), onDisconnected: Some(dev_disconnected), onError: Some(dev_error) };
static mut SESS_CB: sys::ACameraCaptureSession_stateCallbacks = sys::ACameraCaptureSession_stateCallbacks {
    context: null_mut(), onClosed: Some(sess_noop), onReady: Some(sess_noop), onActive: Some(sess_noop) };

fn ok(st: sys::camera_status_t, what: &str) -> Option<()> {
    if st.0 == 0 { Some(()) } else { log::error!("cámara: {what} falló ({})", st.0); None }
}

impl Camera {
    /// facing: "user" (delantera) o "environment" (trasera)
    pub fn start(link: Arc<Link>, facing: &str) -> Option<Camera> {
        unsafe {
            let mgr = sys::ACameraManager_create();
            let mut list: *mut sys::ACameraIdList = null_mut();
            ok(sys::ACameraManager_getCameraIdList(mgr, &mut list), "lista de cámaras")?;
            let want_front = facing == "user";
            let mut chosen: Option<(std::ffi::CString, (i32, i32), i32)> = None;
            for i in 0..(*list).numCameras {
                let id = *(*list).cameraIds.add(i as usize);
                let mut meta: *mut sys::ACameraMetadata = null_mut();
                if sys::ACameraManager_getCameraCharacteristics(mgr, id, &mut meta).0 != 0 { continue; }
                let mut e: sys::ACameraMetadata_const_entry = std::mem::zeroed();
                let front = sys::ACameraMetadata_getConstEntry(meta, sys::acamera_metadata_tag::ACAMERA_LENS_FACING.0, &mut e).0 == 0
                    && *e.data.u8_ == 0;
                if front == want_front && chosen.is_none() {
                    // rangos de fps disponibles: se prefiere el fijo más alto (min == max), si no el de max más alto
                    let mut best = (15, 30);
                    let mut r: sys::ACameraMetadata_const_entry = std::mem::zeroed();
                    if sys::ACameraMetadata_getConstEntry(meta, sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES.0, &mut r).0 == 0 {
                        let v = std::slice::from_raw_parts(r.data.i32_, r.count as usize);
                        let ranges: Vec<(i32, i32)> = v.chunks_exact(2).map(|c| (c[0], c[1])).collect();
                        log::info!("cámara {}: rangos de fps {ranges:?}", CStr::from_ptr(id).to_string_lossy());
                        let fixed = ranges.iter().filter(|r| r.0 == r.1 && r.1 <= 30).max_by_key(|r| r.1);
                        best = *fixed.or(ranges.iter().filter(|r| r.1 <= 30).max_by_key(|r| (r.1, r.0))).unwrap_or(&best);
                    }
                    // cómo está montado el sensor (grados horarios respecto al móvil en vertical)
                    let mut so: sys::ACameraMetadata_const_entry = std::mem::zeroed();
                    let sensor = if sys::ACameraMetadata_getConstEntry(meta, sys::acamera_metadata_tag::ACAMERA_SENSOR_ORIENTATION.0, &mut so).0 == 0 {
                        *so.data.i32_
                    } else { 90 };
                    chosen = Some((CStr::from_ptr(id).to_owned(), best, sensor));
                }
                sys::ACameraMetadata_free(meta);
            }
            sys::ACameraManager_deleteCameraIdList(list);
            let Some((cam_id, fps, sensor)) = chosen else { log::error!("cámara: no hay {facing}"); sys::ACameraManager_delete(mgr); return None; };
            log::info!("cámara: usando {} a {}-{} fps, sensor a {sensor}°", cam_id.to_string_lossy(), fps.0, fps.1);

            // Codificador H.264 con superficie de entrada (la cámara escribe ahí)
            // Se prueba con las claves de poco retraso; si el codificador las rechaza, con las mínimas
            let base = |f: &mut MediaFormat| {
                f.set_str("mime", "video/avc");
                f.set_i32("width", W);
                f.set_i32("height", H);
                f.set_i32("color-format", 0x7F00_0789);   // COLOR_FormatSurface
                f.set_i32("bitrate", 3_000_000);
                f.set_i32("frame-rate", fps.1);
                f.set_i32("i-frame-interval", 1);
            };
            let mut codec = None;
            // 0: todo; 1: sin perfil/nivel/prioridad; 2: sin modo de tasa. El modo CBR es importante:
            // sin él, el codificador de Qualcomm ignoraba la tasa y mandaba ~110 Mbit/s (atascaba el wifi).
            for i in 0..3 {
                let Some(c) = MediaCodec::from_encoder_type("video/avc") else { break };
                let mut f = MediaFormat::new();
                base(&mut f);
                if i < 2 {
                    f.set_i32("bitrate-mode", 2);          // BITRATE_MODE_CBR
                }
                if i == 0 {
                    f.set_i32("profile", 1);               // Baseline: sin fotogramas B
                    f.set_i32("level", 0x200);             // AVCLevel31 (1280x720@30)
                    f.set_i32("priority", 0);
                }
                match c.configure(&f, None, MediaCodecDirection::Encoder) {
                    Ok(()) => { log::info!("codificador: configurado (intento {})", i + 1); codec = Some(c); break; }
                    Err(e) => log::warn!("codificador: intento {} rechazado ({e:?})", i + 1),
                }
            }
            let Some(codec) = codec else { log::error!("codificador: no acepta ninguna configuración"); return None; };
            let window = match codec.create_input_surface() { Ok(w) => w, Err(e) => { log::error!("superficie: {e:?}"); return None; } };
            if let Err(e) = codec.start() { log::error!("codificador start: {e:?}"); return None; }
            // El de Qualcomm (Snapdragon 845) no respeta del todo la tasa de configure (daba 34-110
            // Mbit/s): fijada en marcha sí la respeta
            {
                let mut p = MediaFormat::new();
                p.set_i32("video-bitrate", 3_000_000);
                if let Err(e) = codec.set_parameters(p) { log::warn!("codificador: video-bitrate: {e:?}"); }
            }
            let codec = Arc::new(SendCodec(codec));

            let mut dev: *mut sys::ACameraDevice = null_mut();
            ok(sys::ACameraManager_openCamera(mgr, cam_id.as_ptr(), &raw mut DEV_CB, &mut dev), "abrir")?;
            let anw = window.ptr().as_ptr() as *mut sys::ACameraWindowType;
            let mut container = null_mut();
            ok(sys::ACaptureSessionOutputContainer_create(&mut container), "contenedor")?;
            let mut sess_out = null_mut();
            ok(sys::ACaptureSessionOutput_create(anw, &mut sess_out), "salida")?;
            ok(sys::ACaptureSessionOutputContainer_add(container, sess_out), "añadir salida")?;
            let mut session = null_mut();
            ok(sys::ACameraDevice_createCaptureSession(dev, container, &raw const SESS_CB, &mut session), "sesión")?;
            let mut request = null_mut();
            ok(sys::ACameraDevice_createCaptureRequest(dev, sys::ACameraDevice_request_template::TEMPLATE_RECORD, &mut request), "petición")?;
            let mut target = null_mut();
            ok(sys::ACameraOutputTarget_create(anw, &mut target), "destino")?;
            ok(sys::ACaptureRequest_addTarget(request, target), "añadir destino")?;
            let range = [fps.0, fps.1];
            sys::ACaptureRequest_setEntry_i32(request, sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_TARGET_FPS_RANGE.0, 2, range.as_ptr());
            let mut reqs = [request];
            ok(sys::ACameraCaptureSession_setRepeatingRequest(session, null_mut(), 1, reqs.as_mut_ptr(), null_mut()), "repetir")?;

            // Hilo de salida: cada imagen comprimida → canal 4 (los SPS/PPS se anteponen a cada fotograma clave)
            let stop = Arc::new(AtomicBool::new(false));
            // Giro que necesita la imagen para salir derecha en el ordenador (como la orientación JPEG
            // de Android): se manda al empezar y cada vez que cambia la posición del móvil
            {
                let (link, stop) = (link.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut last = -1;
                    while !stop.load(Ordering::Relaxed) && !link.is_closed() {
                        #[cfg(target_os = "android")]
                        let dev = crate::android::device_orientation();
                        #[cfg(not(target_os = "android"))]
                        let dev = -1;
                        let dev = if dev < 0 { 90 } else { dev };        // sin dato: horizontal
                        let rot = if want_front { (sensor - dev + 360) % 360 } else { (sensor + dev) % 360 };
                        if rot != last {
                            last = rot;
                            log::info!("cámara: giro {rot}° (móvil a {dev}°)");
                            let _ = link.send_json(serde_json::json!({"t": "cam_rot", "deg": rot}));
                        }
                        std::thread::sleep(Duration::from_millis(300));
                    }
                });
            }
            {
                let (codec, stop) = (codec.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut config: Vec<u8> = Vec::new();
                    let mut n = 0u64;
                    let mut behind = false;
                    while !stop.load(Ordering::Relaxed) {
                        match codec.0.dequeue_output_buffer(Duration::from_millis(20)) {
                            Ok(DequeuedOutputBufferInfoResult::Buffer(b)) => {
                                let flags = b.info().flags();
                                let data = b.buffer().to_vec();
                                let _ = codec.0.release_output_buffer(b, false);
                                if flags & 2 != 0 { config = data; continue; }      // BUFFER_FLAG_CODEC_CONFIG
                                let pkt = if flags & 1 != 0 && !config.is_empty() { [config.as_slice(), &data].concat() } else { data };
                                if n % 300 == 0 { log::info!("cámara: imagen #{n} ({} bytes)", pkt.len()); }
                                n += 1;
                                // si el envío va atascado, se tiran imágenes hasta la siguiente completa
                                // (clave): mejor saltar que acumular retraso
                                let key = flags & 1 != 0;
                                if behind && !key { continue; }
                                let t0 = std::time::Instant::now();
                                if link.is_closed() || link.send(CH_CAM, &pkt).is_err() { break; }
                                behind = t0.elapsed() > Duration::from_millis(60);
                                if behind { log::warn!("cámara: envío lento ({:?}): se salta hasta la próxima clave", t0.elapsed()); }
                            }
                            Ok(_) => {}
                            Err(e) => { log::warn!("codificador: {e:?}"); std::thread::sleep(Duration::from_millis(20)); }
                        }
                    }
                });
            }
            Some(Camera { mgr, dev, session, request, target, container, sess_out, _window: window, codec, stop,
                          facing: facing.to_string() })
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        unsafe {
            sys::ACameraCaptureSession_stopRepeating(self.session);
            sys::ACameraCaptureSession_close(self.session);
            sys::ACameraDevice_close(self.dev);
            sys::ACaptureRequest_removeTarget(self.request, self.target);
            sys::ACameraOutputTarget_free(self.target);
            sys::ACaptureRequest_free(self.request);
            sys::ACaptureSessionOutputContainer_remove(self.container, self.sess_out);
            sys::ACaptureSessionOutput_free(self.sess_out);
            sys::ACaptureSessionOutputContainer_free(self.container);
            sys::ACameraManager_delete(self.mgr);
        }
        let _ = self.codec.0.stop();
        log::info!("cámara: parada");
    }
}
