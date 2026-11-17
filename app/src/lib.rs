//! Móvil como pantalla — app nativa.
//!
//! Fase 1: al abrirla busca los equipos (por cable y por wifi), se elige uno, ese equipo pide
//! permiso con una notificación y se abre una sesión cifrada (Noise XX). Las siguientes fases
//! añaden el escritorio, el sonido, la cámara, el micrófono, el almacenamiento y el stand-by.

mod android;
mod applog;
mod audio;
mod camera;
mod standby;
mod standby_bg;
mod storage;
mod link;
mod net;
mod rfb;
mod video;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use net::{Host, Identity, Status};

/// Paleta Tokyo Night (la misma de Quickshell)
mod theme {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x26);
    pub const SURFACE: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
    pub const FG: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
    pub const MUTED: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
    pub const BLUE: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
    pub const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
    pub const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
    pub const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
}

struct App {
    state: net::State,
    ident: Arc<Identity>,
    device: String,
    renderer: Arc<Mutex<Option<video::Renderer>>>,
    /// gestos táctiles sobre el escritorio
    touch: Touch,
    /// menú de la app (tres dedos)
    menu: bool,
    /// dedo que está pulsando el botón del menú
    btn_touch: Option<u64>,
    standby: standby::Standby,
    standby_was: bool,
    #[cfg(target_os = "android")]
    android: Option<winit::platform::android::activity::AndroidApp>,
    keep_on: bool,
    data_dir: std::path::PathBuf,
    /// pantalla de permisos (la primera vez, o desde el botón "Permisos")
    setup: bool,
    /// estado de permisos/cortafuegos leído por JNI (se refresca cada segundo)
    perms: Perms,
}

#[derive(Default, Clone, Copy)]
struct Perms { media: bool, firewall: bool, battery: bool, at: Option<std::time::Instant> }

impl App {
    fn new(data_dir: std::path::PathBuf, debug_dir: Option<std::path::PathBuf>) -> App {
        let setup = !data_dir.join("permisos-vistos").exists();
        let state = Arc::new(Mutex::new(net::Shared {
            hosts: HashMap::new(),
            status: Status::Idle,
            link: None,
            video: None,
            dark: false,
            standby: false,
            sb: serde_json::Value::Null,
            disconnect: false,
            screen_error: None,
            cam_on: false,
            mic_on: false,
            facing: "user".into(),
            aviso: None,
        }));
        net::discovery(state.clone());
        let ident = Arc::new(Identity::load(data_dir.clone()));
        // Depuración: si existe files/depuracion.json (solo lo puede escribir adb) con
        // {"conectar": "127.0.0.1:47901"}, se conecta sola a esa instancia de pruebas de movild.
        if let Some(dir) = debug_dir {
            if let Ok(d) = std::fs::read(dir.join("depuracion.json")) {
                if let Some(addr) = serde_json::from_slice::<serde_json::Value>(&d).ok()
                    .and_then(|v| v["conectar"].as_str().map(String::from)) {
                    log::warn!("DEPURACIÓN: conexión automática a {addr}");
                    let (st, id, dev) = (state.clone(), ident.clone(), net::device_name());
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        match net::probe(&addr) {
                            Some(h) => net::connect(st, id, h, dev),
                            None => log::error!("DEPURACIÓN: {addr} no responde"),
                        }
                    });
                }
            }
        }
        App {
            state, ident, device: net::device_name(),
            renderer: Arc::new(Mutex::new(None)), touch: Touch::default(), menu: false, btn_touch: None, standby: standby::Standby::new(), standby_was: false,
            #[cfg(target_os = "android")]
            android: None,
            keep_on: false,
            data_dir, setup, perms: Perms::default(),
        }
    }

    /// Estado de permisos y cortafuegos (JNI, como mucho una vez por segundo)
    fn perms(&mut self) -> Perms {
        let stale = self.perms.at.map(|t| t.elapsed() > Duration::from_secs(1)).unwrap_or(true);
        if stale {
            self.perms = Perms {
                media: android::has_media_permissions(),
                firewall: android::firewall_state() == 1,
                battery: android::battery_exempt(),
                at: Some(std::time::Instant::now()),
            };
        }
        self.perms
    }

    /// Pantalla de permisos: cámara/micro, cortafuegos propio y que Android no cierre la app
    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        let p = self.perms();
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(36.0)).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(big("Antes de empezar", 34.0, theme::FG));
                ui.label(big("Toca cada botón y acepta lo que pregunte Android", 17.0, theme::MUTED));
                ui.add_space(22.0);
            });
            let row = |ui: &mut egui::Ui, ok: bool, title: &str, desc: &str, action: &str| -> bool {
                let mut clicked = false;
                egui::Frame::new().fill(theme::SURFACE).corner_radius(18.0).inner_margin(16.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(big(if ok { "\u{F012C}" } else { "\u{F0130}" }, 30.0, if ok { theme::GREEN } else { theme::MUTED }));
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(big(title, 21.0, theme::FG));
                            ui.label(big(desc, 15.0, theme::MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (t, c) = if ok { ("Listo", theme::SURFACE) } else { (action, theme::BLUE) };
                            let b = egui::Button::new(big(t, 18.0, if ok { theme::GREEN } else { theme::BG })).fill(c)
                                .corner_radius(14.0).min_size(egui::vec2(170.0, 52.0));
                            if ui.add(b).clicked() { clicked = true; }
                        });
                    });
                });
                ui.add_space(12.0);
                clicked
            };
            if row(ui, p.media, "Cámara y micrófono", "Para usar el móvil como webcam y micro del ordenador", "Permitir") && !p.media {
                android::ask_media_permissions();
            }
            if row(ui, p.firewall, "Bloquear internet",
                   "Ninguna app sale a internet; Escritorio solo habla con los equipos de casa (sustituye a NetGuard)", "Activar") {
                android::set_firewall(!p.firewall);
            }
            if row(ui, p.battery, "Que Android no cierre la app",
                   "Sin ahorro de batería para Escritorio. En MIUI activa también \"Inicio automático\"", "Abrir") {
                if p.battery { android::open_autostart() } else { android::ask_battery() }
            }
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                let b = egui::Button::new(big("Continuar", 22.0, theme::BG)).fill(theme::GREEN)
                    .corner_radius(18.0).min_size(egui::vec2(280.0, 64.0));
                if ui.add(b).clicked() {
                    let _ = std::fs::write(self.data_dir.join("permisos-vistos"), b"1");
                    self.setup = false;
                }
            });
        });
        self.perms.at = None;   // tras tocar algo, releer pronto
    }

    /// Mantener la pantalla encendida mientras haya sesión
    fn keep_screen_on(&mut self, on: bool) {
        if self.keep_on == on { return; }
        self.keep_on = on;

        #[cfg(target_os = "android")]
        if let Some(a) = &self.android {
            // Wake lock por JNI (cambiar los flags de la ventana la recreaba y bloqueaba la app)
            let a = a.clone();
            std::thread::spawn(move || android::keep_screen_on(&a, on));
        }
    }

    /// Escritorio a pantalla completa + toques → ratón
    fn desktop(&mut self, ui: &mut egui::Ui, link: Arc<link::Link>, video: Arc<video::Video>) {
        *video.ctx.lock().unwrap() = Some(ui.ctx().clone());
        let (w, h) = *video.size.lock().unwrap();
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, egui::Color32::BLACK);
        // pantalla "apagada" desde el equipo: solo negro (ni vídeo ni toques)
        if self.state.lock().unwrap().dark {
            let _ = ui.interact(full, ui.scope_id().with("apagada"), egui::Sense::click_and_drag());
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            return;
        }
        if w <= 0 || h <= 0 {
            ui.painter().text(full.center(), egui::Align2::CENTER_CENTER, "Abriendo el escritorio…",
                              egui::FontId::proportional(22.0), theme::MUTED);
            return;
        }
        // encaja w×h en la pantalla manteniendo la proporción
        let scale = (full.width() / w as f32).min(full.height() / h as f32);
        let size = egui::vec2(w as f32 * scale, h as f32 * scale);
        let rect = egui::Rect::from_center_size(full.center(), size);
        let (renderer, v) = (self.renderer.clone(), video.clone());
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 { log::info!("escritorio: dibujando {w}x{h} en {rect:?}"); }
        ui.painter().add(egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                let mut r = renderer.lock().unwrap();
                let gl_id = std::sync::Arc::as_ptr(painter.gl()) as usize;
                if r.as_ref().map(|x| x.gl_id != gl_id).unwrap_or(true) {
                    log::info!("contexto GL nuevo: se crea el renderizador");
                    *r = Some(video::Renderer::new(painter.gl(), gl_id));
                }
                r.as_mut().unwrap().paint(painter.gl(), &v);
            })),
        });
        let _ = ui.interact(full, ui.scope_id().with("escritorio"), egui::Sense::click_and_drag());
        let to_remote = |p: egui::Pos2| -> (u16, u16) {
            let x = ((p.x - rect.min.x) / scale).clamp(0.0, (w - 1) as f32);
            let y = ((p.y - rect.min.y) / scale).clamp(0.0, (h - 1) as f32);
            (x as u16, y as u16)
        };
        // Botón siempre visible (abajo a la derecha) que abre el menú con "Desconectar":
        // los toques que empiezan en él no llegan al escritorio.
        let btn = egui::Rect::from_center_size(full.right_bottom() - egui::vec2(40.0, 40.0), egui::vec2(56.0, 56.0));
        let p = ui.painter();
        p.circle_filled(btn.center(), 26.0, egui::Color32::from_black_alpha(if self.menu { 200 } else { 110 }));
        p.text(btn.center(), egui::Align2::CENTER_CENTER, "\u{F0425}", egui::FontId::proportional(26.0),
               egui::Color32::from_white_alpha(170));
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Touch { id, phase, pos, .. } = ev {
                if phase == egui::TouchPhase::Start && btn.contains(pos) { self.btn_touch = Some(id.0); }
                if self.btn_touch == Some(id.0) {
                    if matches!(phase, egui::TouchPhase::End | egui::TouchPhase::Cancel) {
                        self.btn_touch = None;
                        if phase == egui::TouchPhase::End && btn.expand(20.0).contains(pos) { self.menu = !self.menu; }
                    }
                    continue;
                }
                let (x, y) = to_remote(pos);
                if let Some(action) = self.touch.event(id.0, phase, pos, (x, y)) {
                    match action {
                        Gesture::Menu => self.menu = !self.menu,
                        // con el menú abierto los toques son para el menú, no para el escritorio
                        _ if self.menu => {}
                        g => send_gesture(&link, g),
                    }
                }
            }
        }
        // mantener pulsado sin mover → clic derecho
        if let Some(g) = self.touch.tick() { if !self.menu { send_gesture(&link, g); } }
        if self.menu { self.menu_ui(ui, &link); }
        ui.ctx().request_repaint();
    }
}

fn big(text: impl Into<String>, size: f32, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(text).size(size).color(color)
}

impl App {
    /// Botones de cámara, micrófono y stand-by: se piden al equipo, que monta su parte
    /// (cámara virtual / micrófono virtual) y contesta con el estado real.
    fn media_controls(&mut self, ui: &mut egui::Ui, link: &Arc<link::Link>, width: f32) {
        let (cam, mic, sb, facing) = {
            let st = self.state.lock().unwrap();
            (st.cam_on, st.mic_on, st.standby, st.facing.clone())
        };
        let btn = |t: String, on: bool| egui::Button::new(big(t, 18.0, if on { theme::BG } else { theme::FG }))
            .fill(if on { theme::GREEN } else { theme::SURFACE }).corner_radius(14.0).min_size(egui::vec2(width, 52.0));
        ui.horizontal(|ui| {
            if ui.add(btn(format!("\u{F0100}  Cámara {}", if cam { "encendida" } else { "apagada" }), cam)).clicked() {
                let _ = link.send_json(serde_json::json!({"t": "media_set", "cam": !cam}));
            }
            let flip = egui::Button::new(big(format!("\u{F0450}  {}", if facing == "environment" { "Trasera" } else { "Delantera" }), 18.0, theme::FG))
                .fill(theme::SURFACE).corner_radius(14.0).min_size(egui::vec2(width * 0.6, 52.0));
            if ui.add(flip).clicked() {
                let _ = link.send_json(serde_json::json!({"t": "media_set", "facing": "flip"}));
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.add(btn(format!("\u{F036C}  Micrófono {}", if mic { "encendido" } else { "apagado" }), mic)).clicked() {
                let _ = link.send_json(serde_json::json!({"t": "media_set", "mic": !mic}));
            }
            let s = egui::Button::new(big("\u{F0594}  Stand-by", 18.0, if sb { theme::BG } else { theme::FG }))
                .fill(if sb { theme::BLUE } else { theme::SURFACE }).corner_radius(14.0).min_size(egui::vec2(width * 0.6, 52.0));
            if ui.add(s).clicked() {
                let _ = link.send_json(serde_json::json!({"t": "standby_set", "on": !sb}));
                self.menu = false;
            }
        });
    }

    /// Aviso corto del equipo (p. ej. "la cámara aún no funciona aquí"), 5 s abajo
    fn aviso_ui(&mut self, ctx: &egui::Context) {
        let a = self.state.lock().unwrap().aviso.clone();
        if let Some((t, at)) = a {
            if at.elapsed() > Duration::from_secs(5) { self.state.lock().unwrap().aviso = None; return; }
            egui::Area::new(egui::Id::new("aviso")).anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -40.0)).show(ctx, |ui| {
                egui::Frame::new().fill(theme::SURFACE).corner_radius(14.0).inner_margin(14.0).show(ui, |ui| {
                    ui.label(big(t, 18.0, theme::YELLOW));
                });
            });
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Menú de la app sobre el escritorio (se abre con tres dedos)
    fn menu_ui(&mut self, ui: &mut egui::Ui, link: &Arc<link::Link>) {
        egui::Area::new(egui::Id::new("menu")).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::new().fill(theme::SURFACE).corner_radius(24.0).inner_margin(28.0).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(big("Escritorio", 26.0, theme::FG));
                        ui.add_space(14.0);
                        let b = |t: &str, c: egui::Color32| egui::Button::new(big(t, 20.0, theme::BG)).fill(c)
                            .corner_radius(16.0).min_size(egui::vec2(280.0, 60.0));
                        if ui.add(b("Desconectar", theme::RED)).clicked() {
                            self.state.lock().unwrap().disconnect = true;
                            link.close();
                            self.menu = false;
                        }
                        ui.add_space(16.0);
                        self.media_controls(ui, link, 300.0);
                        ui.add_space(16.0);
                        if ui.add(b("Volver al escritorio", theme::BLUE)).clicked() {
                            self.menu = false;
                            let mut st = self.state.lock().unwrap();
                            if st.standby {
                                st.standby = false;
                                let _ = link.send_json(serde_json::json!({"t": "standby_off"}));
                            }
                        }
                    });
                });
            });
    }
}

/// Lo que se manda al equipo por un gesto
#[derive(Debug, Clone, Copy)]
enum Gesture {
    Move(u16, u16),
    Press(u16, u16),
    Release(u16, u16),
    Click(u16, u16),
    RightClick(u16, u16),
    Scroll(u16, u16, i32),
    Menu,
}

fn send_gesture(link: &link::Link, g: Gesture) {
    match g {
        Gesture::Move(x, y) => rfb::pointer(link, 0, x, y),
        Gesture::Press(x, y) => rfb::pointer(link, 1, x, y),
        Gesture::Release(x, y) => rfb::pointer(link, 0, x, y),
        Gesture::Click(x, y) => { rfb::pointer(link, 0, x, y); rfb::pointer(link, 1, x, y); rfb::pointer(link, 0, x, y); }
        Gesture::RightClick(x, y) => { rfb::pointer(link, 0, x, y); rfb::pointer(link, 4, x, y); rfb::pointer(link, 0, x, y); }
        Gesture::Scroll(x, y, steps) => {
            // rueda: botón 4 (arriba) o 5 (abajo), un clic por paso
            let bit = if steps > 0 { 8 } else { 16 };
            for _ in 0..steps.abs() { rfb::pointer(link, bit, x, y); rfb::pointer(link, 0, x, y); }
        }
        Gesture::Menu => {}
    }
}

/// Reconocedor de gestos: tocar = clic, arrastrar = clic mantenido, mantener = clic derecho,
/// dos dedos = rueda, tres dedos = menú de la app.
#[derive(Default)]
struct Touch {
    fingers: HashMap<u64, egui::Pos2>,
    start: Option<(std::time::Instant, egui::Pos2, (u16, u16))>,
    last: (u16, u16),
    dragging: bool,
    moved: bool,
    long_done: bool,
    multi: bool,
    scroll_acc: f32,
    scroll_y: Option<f32>,
}

impl Touch {
    fn event(&mut self, id: u64, phase: egui::TouchPhase, pos: egui::Pos2, remote: (u16, u16)) -> Option<Gesture> {
        use egui::TouchPhase::*;
        match phase {
            Start => {
                self.fingers.insert(id, pos);
                match self.fingers.len() {
                    1 => {
                        self.start = Some((std::time::Instant::now(), pos, remote));
                        self.last = remote;
                        self.dragging = false; self.moved = false; self.long_done = false; self.multi = false;
                        return Some(Gesture::Move(remote.0, remote.1));
                    }
                    2 => {
                        self.multi = true;
                        self.scroll_y = Some(self.centroid().y);
                        self.scroll_acc = 0.0;
                        if self.dragging { self.dragging = false; return Some(Gesture::Release(self.last.0, self.last.1)); }
                    }
                    3 => { self.multi = true; return Some(Gesture::Menu); }
                    _ => {}
                }
                None
            }
            Move => {
                self.fingers.insert(id, pos);
                if self.fingers.len() == 2 {
                    let y = self.centroid().y;
                    if let Some(y0) = self.scroll_y {
                        self.scroll_acc += y - y0;
                        self.scroll_y = Some(y);
                        let steps = (self.scroll_acc / 30.0) as i32;
                        if steps != 0 {
                            self.scroll_acc -= steps as f32 * 30.0;
                            // dedos hacia abajo = contenido hacia abajo (rueda arriba), como en el móvil
                            return Some(Gesture::Scroll(self.last.0, self.last.1, steps));
                        }
                    }
                    return None;
                }
                if self.multi || self.fingers.len() != 1 { return None; }
                let (_, p0, r0) = self.start?;
                self.last = remote;
                if !self.moved && (pos - p0).length() > 10.0 { self.moved = true; }
                if self.moved && !self.long_done {
                    if !self.dragging { self.dragging = true; return Some(Gesture::Press(r0.0, r0.1)); }
                    return Some(Gesture::Move(remote.0, remote.1));
                }
                None
            }
            End | Cancel => {
                self.fingers.remove(&id);
                if !self.fingers.is_empty() { return None; }
                let was_multi = self.multi;
                self.multi = false;
                self.scroll_y = None;
                if self.dragging { self.dragging = false; return Some(Gesture::Release(remote.0, remote.1)); }
                if was_multi || self.long_done || self.moved { return None; }
                let (_, _, r0) = self.start.take()?;
                Some(Gesture::Click(r0.0, r0.1))
            }
        }
    }

    /// Se llama en cada fotograma: mantener pulsado ~0,55 s sin mover = clic derecho
    fn tick(&mut self) -> Option<Gesture> {
        if self.fingers.len() != 1 || self.moved || self.long_done || self.multi { return None; }
        let (t0, _, r0) = self.start?;
        if t0.elapsed() > std::time::Duration::from_millis(550) {
            self.long_done = true;
            return Some(Gesture::RightClick(r0.0, r0.1));
        }
        None
    }

    fn centroid(&self) -> egui::Pos2 {
        let n = self.fingers.len().max(1) as f32;
        let s = self.fingers.values().fold(egui::Vec2::ZERO, |a, p| a + p.to_vec2());
        (s / n).to_pos2()
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.aviso_ui(ui.ctx());
        ui.ctx().request_repaint_after(Duration::from_millis(400));
        let (mut hosts, status, link, video) = {
            let st = self.state.lock().unwrap();
            (st.hosts.values().cloned().collect::<Vec<Host>>(), st.status.clone(), st.link.clone(), st.video.clone())
        };
        let connected = matches!(status, Status::Connected { .. });
        self.keep_screen_on(connected);
        let (sb_on, sb) = { let st = self.state.lock().unwrap(); (st.standby, st.sb.clone()) };
        if sb_on && !self.standby_was { self.standby.reset(); }
        self.standby_was = connected && sb_on;
        if connected && sb_on {
            if let Some(l) = link.clone() {
                egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ui, |ui| {
                    self.standby.draw(ui, &sb);
                    let _ = ui.interact(ui.max_rect(), ui.scope_id().with("standby"), egui::Sense::click_and_drag());
                    let events = ui.input(|i| i.events.clone());
                    for ev in events {
                        if let egui::Event::Touch { id, phase, pos, .. } = ev {
                            match self.touch.event(id.0, phase, pos, (0, 0)) {
                                Some(Gesture::Click(..)) => self.standby.next(&sb),
                                Some(Gesture::Menu) => self.menu = !self.menu,
                                _ => {}
                            }
                        }
                    }
                    if self.menu { self.menu_ui(ui, &l); }
                });
                return;
            }
        }
        if let (true, Some(l), Some(v)) = (connected, link.clone(), video.clone()) {
            if v.size.lock().unwrap().0 > 0 {
                egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK))
                    .show(ui, |ui| self.desktop(ui, l, v));
                return;
            }
        }
        if self.setup && !connected { self.setup_ui(ui); return; }
        let perms = self.perms();
        hosts.sort_by(|a, b| b.cable.cmp(&a.cable).then(a.name.cmp(&b.name)));
        let my_fp = net::fingerprint(&self.ident.public);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(40.0))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(10.0);
                    match &status {
                        Status::Connected { name, cable } => {
                            ui.add_space(60.0);
                            ui.label(big(format!("Conectado a {name}"), 38.0, theme::GREEN));
                            ui.add_space(8.0);
                            ui.label(big(if *cable { "Por cable · cifrado" } else { "Por wifi · cifrado" }, 20.0, theme::MUTED));
                            let err = self.state.lock().unwrap().screen_error.clone();
                            if let Some(e) = err {
                                ui.add_space(14.0);
                                ui.label(big("Sin escritorio:", 18.0, theme::YELLOW));
                                ui.add(egui::Label::new(big(e, 16.0, theme::FG)).wrap());
                            }
                            ui.add_space(40.0);
                            let b = egui::Button::new(big("Desconectar", 22.0, theme::BG)).fill(theme::RED)
                                .corner_radius(18.0).min_size(egui::vec2(260.0, 70.0));
                            if ui.add(b).clicked() {
                                self.state.lock().unwrap().disconnect = true;
                                if let Some(l) = &link { l.close(); }
                            }
                            if let Some(l) = &link {
                                ui.add_space(30.0);
                                ui.scope(|ui| {
                                    ui.set_max_width(560.0);
                                    self.media_controls(ui, l, 300.0);
                                });
                            }
                            return;
                        }
                        _ => {}
                    }

                    ui.label(big("¿A qué equipo te conectas?", 34.0, theme::FG));
                    ui.add_space(6.0);
                    let (sub, col) = match &status {
                        Status::Idle => ("Toca uno: ese equipo te pedirá permiso con una notificación".to_string(), theme::MUTED),
                        Status::Connecting(n) => (format!("Conectando con {n}…"), theme::YELLOW),
                        Status::Waiting(n) => (format!("Acepta la notificación en {n} (huella del móvil {my_fp})"), theme::YELLOW),
                        Status::Failed(e) => (e.clone(), theme::RED),
                        Status::Connected { .. } => (String::new(), theme::MUTED),
                    };
                    ui.label(big(sub, 18.0, col));
                    ui.add_space(30.0);

                    if hosts.is_empty() {
                        ui.add_space(40.0);
                        ui.label(big("Buscando equipos…", 24.0, theme::MUTED));
                        ui.label(big("Enchufa el cable o conecta el wifi de casa", 16.0, theme::MUTED));
                    }
                    let busy_ui = matches!(status, Status::Connecting(_) | Status::Waiting(_));
                    ui.horizontal(|ui| {
                        let w = 290.0;
                        let n = hosts.len().max(1) as f32;
                        ui.add_space(((ui.available_width() - (w * n + 24.0 * (n - 1.0))) / 2.0).max(0.0));
                        for h in &hosts {
                            let via = if h.cable { "Por cable" } else { "Por wifi" };
                            let extra = if h.busy { " · ocupado" } else { "" };
                            // icono del sistema (Nerd Font): Windows / Linux
                            let icon = match h.platform.as_str() { "windows" => "\u{F17A} ", "linux" => "\u{F17C} ", _ => "" };
                            let text = big(format!("{icon}{}\n{} · {via}{extra}", h.name, h.os), 24.0, theme::FG);
                            let btn = egui::Button::new(text).fill(theme::SURFACE).corner_radius(22.0)
                                .min_size(egui::vec2(w, 150.0));
                            if ui.add_enabled(!busy_ui, btn).clicked() {
                                net::connect(self.state.clone(), self.ident.clone(), h.clone(), self.device.clone());
                            }
                            ui.add_space(24.0);
                        }
                    });

                    ui.add_space(30.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 2.0 * 300.0 - 16.0).max(0.0) / 2.0);
                        let (t, c) = if perms.firewall { ("\u{F0483}  Internet bloqueado", theme::GREEN) }
                                     else { ("\u{F0484}  Internet abierto", theme::YELLOW) };
                        let b = egui::Button::new(big(t, 17.0, c)).fill(theme::SURFACE).corner_radius(14.0)
                            .min_size(egui::vec2(300.0, 48.0));
                        if ui.add(b).clicked() { android::set_firewall(!perms.firewall); self.perms.at = None; }
                        ui.add_space(16.0);
                        let b = egui::Button::new(big("\u{F0493}  Permisos", 17.0, theme::FG)).fill(theme::SURFACE)
                            .corner_radius(14.0).min_size(egui::vec2(300.0, 48.0));
                        if ui.add(b).clicked() { self.setup = true; }
                    });
                    ui.add_space(14.0);
                    ui.label(big(format!("{} · huella {my_fp}", self.device), 15.0, theme::BLUE));
                });
            });
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    applog::init(app.external_data_path());
    android::init(&app);
    log::info!("app arrancada");
    let data_dir = app.internal_data_path().unwrap_or_else(|| "/data/data/org.movilpantalla.app/files".into());
    let debug_dir = app.external_data_path();
    let app2 = app.clone();
    let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
    if let Err(e) = eframe::run_native("Móvil", options, Box::new(move |cc| {
        cc.egui_ctx.set_pixels_per_point(2.0);
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("nf".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/nf.ttf"))));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("nf".into());
        }
        cc.egui_ctx.set_fonts(fonts);
        let mut a = App::new(data_dir, debug_dir);
        a.android = Some(app2);
        Ok(Box::new(a))
    })) {
        log::error!("eframe: {e}");
    }
    // Android deja vivo el proceso al cerrar la actividad, pero winit no puede crear otro bucle de
    // eventos en el mismo proceso ("EventLoop can't be recreated"): se termina para empezar limpio.
    log::info!("actividad cerrada: fin del proceso");
    std::process::exit(0);
}
