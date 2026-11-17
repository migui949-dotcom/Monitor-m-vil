//! `pantalla-host panel`: ventanita "Escritorio" para ordenadores sin la barra Quickshell.
//! Enseña el móvil conectado y tiene los mismos botones que la barra (cámara, micro, stand-by,
//! pantalla del móvil, desconectar) y la tarjeta para aceptar un móvil nuevo. Se puede mover y
//! encoger a una barrita. Habla con el servicio por las mismas URL locales que usa Quickshell.

use eframe::egui;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const W: f32 = 330.0;
const MINI: [f32; 2] = [210.0, 52.0];

mod theme {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x26);
    pub const SURFACE: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
    pub const FG: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
    pub const MUTED: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
    pub const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
    pub const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
    pub const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
}

/// Archivo que el panel toca cada segundo: el servicio sabe así que hay quien enseñe la tarjeta
pub fn alive_path() -> std::path::PathBuf {
    crate::util::state_dir().join("panel.vivo")
}

/// ¿Hay un panel abierto? (tocado hace menos de 3 s)
pub fn alive() -> bool {
    std::fs::metadata(alive_path())
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < Duration::from_secs(3)).unwrap_or(false))
        .unwrap_or(false)
}

/// Abre el panel como proceso aparte (desde el servicio)
pub fn spawn() {
    if let Ok(me) = std::env::current_exe() {
        let _ = std::process::Command::new(me)
            .arg("panel")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

fn get(port: u16, path: &str) -> Option<String> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut r = String::new();
    s.read_to_string(&mut r).ok()?;
    Some(r.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

/// Orden al servicio en segundo plano (no bloquea la ventana)
fn send(port: u16, path: String) {
    std::thread::spawn(move || {
        let _ = get(port, &path);
    });
}

#[derive(Default)]
struct Data {
    status: Value,
    media: Value,
}

struct Panel {
    data: Arc<Mutex<Data>>,
    mini: bool,
    screen_off: bool,
    size: Option<egui::Vec2>,
    answer_port: u16,
    media_port: u16,
}

impl Panel {
    fn new(cc: &eframe::CreationContext) -> Panel {
        let mut v = egui::Visuals::dark();
        v.panel_fill = theme::BG;
        v.window_fill = theme::BG;
        cc.egui_ctx.set_visuals(v);
        let data = Arc::new(Mutex::new(Data::default()));
        let (answer_port, media_port) = (crate::util::env_port("MOVILD_ANSWER_PORT", 47802), 6083);
        // cada segundo: estado del servicio (movild.json) y de cámara/micro; y "estoy vivo"
        {
            let (data, ctx) = (data.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || loop {
                let _ = std::fs::write(alive_path(), b"1");
                let status = std::fs::read_to_string(crate::host::status_path())
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or(Value::Null);
                let media = get(media_port, "/state").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
                *data.lock().unwrap() = Data { status, media };
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(1));
            });
        }
        Panel { data, mini: false, screen_off: false, size: None, answer_port, media_port }
    }

    /// Tamaño de la ventana según lo que enseña (solo se pide cuando cambia)
    fn fit(&mut self, ctx: &egui::Context, connected: bool, pending: bool) {
        let size = if self.mini {
            egui::Vec2::from(MINI)
        } else {
            let h = 60.0 + if pending { 110.0 } else { 0.0 } + if connected { 190.0 } else if pending { 0.0 } else { 60.0 };
            egui::vec2(W, h)
        };
        if self.size != Some(size) {
            self.size = Some(size);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
    }
}

fn txt(t: impl Into<String>, size: f32, c: egui::Color32) -> egui::RichText {
    egui::RichText::new(t).size(size).color(c)
}

fn toggle(ui: &mut egui::Ui, label: &str, on: bool, w: f32) -> bool {
    let b = egui::Button::new(txt(label, 14.0, if on { theme::BG } else { theme::FG }))
        .fill(if on { theme::GREEN } else { theme::SURFACE })
        .corner_radius(10.0)
        .min_size(egui::vec2(w, 36.0));
    ui.add(b).clicked()
}

impl eframe::App for Panel {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (st, md) = {
            let d = self.data.lock().unwrap();
            (d.status.clone(), d.media.clone())
        };
        let connected = st["connected"].as_bool().unwrap_or(false);
        let (cam, mic) = (md["cam"].as_bool().unwrap_or(false), md["mic"].as_bool().unwrap_or(false));
        let ctx = ui.ctx().clone();
        let pending = st.get("pending").map(|p| !p.is_null()).unwrap_or(false);
        self.fit(&ctx, connected, pending);

        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(12.0)).show(ui, |ui| {
            if self.mini {
                ui.horizontal(|ui| {
                    let dot = if connected { theme::GREEN } else { theme::MUTED };
                    ui.label(txt("\u{F011C}", 18.0, dot));
                    if connected {
                        if toggle(ui, "\u{F0100}", cam, 36.0) {
                            send(self.media_port, "/set?cam=toggle".into());
                        }
                        if toggle(ui, "\u{F036C}", mic, 36.0) {
                            send(self.media_port, "/set?mic=toggle".into());
                        }
                    } else {
                        ui.label(txt("Escritorio", 14.0, theme::FG));
                    }
                    if ui.add(egui::Button::new(txt("\u{F0140}", 16.0, theme::FG)).fill(theme::SURFACE).min_size(egui::vec2(32.0, 32.0))).clicked() {
                        self.mini = false;
                    }
                });
                return;
            }

            ui.horizontal(|ui| {
                ui.label(txt("\u{F011C}  Escritorio", 18.0, theme::FG));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(txt("\u{F0143}", 16.0, theme::FG)).fill(theme::SURFACE)).on_hover_text("Encoger").clicked() {
                        self.mini = true;
                    }
                });
            });
            ui.add_space(8.0);

            // móvil que pide conectarse
            if let Some(p) = st.get("pending").filter(|p| !p.is_null()) {
                egui::Frame::new().fill(theme::SURFACE).corner_radius(12.0).inner_margin(10.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(txt(format!("{} quiere conectarse", p["device"].as_str().unwrap_or("Un móvil")), 15.0, theme::YELLOW));
                    ui.label(txt(format!("Huella {} · por {}", p["fp"].as_str().unwrap_or("?"), p["via"].as_str().unwrap_or("?")), 13.0, theme::MUTED));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let id = p["id"].as_str().unwrap_or("").to_string();
                        if ui.add(egui::Button::new(txt("Aceptar", 14.0, theme::BG)).fill(theme::GREEN).min_size(egui::vec2(120.0, 34.0))).clicked() {
                            send(self.answer_port, format!("/answer?id={id}&ok=1"));
                        }
                        if ui.add(egui::Button::new(txt("Rechazar", 14.0, theme::BG)).fill(theme::RED).min_size(egui::vec2(120.0, 34.0))).clicked() {
                            send(self.answer_port, format!("/answer?id={id}&ok=0"));
                        }
                    });
                });
                ui.add_space(8.0);
            }

            if !connected && pending {
                return;
            }
            if !connected {
                ui.label(txt("Sin móvil conectado", 15.0, theme::MUTED));
                ui.add_space(4.0);
                ui.label(txt("Abre la app Escritorio en el móvil y elige este ordenador.", 13.0, theme::MUTED));
                return;
            }

            let peer = &st["peer"];
            let ph = &st["phone"];
            ui.label(txt(format!("{} · por {}", peer["device"].as_str().unwrap_or("Móvil"), peer["via"].as_str().unwrap_or("?")), 15.0, theme::GREEN));
            if let Some(l) = ph["level"].as_i64() {
                let carga = if ph["charging"].as_bool().unwrap_or(false) { " · cargando" } else { "" };
                ui.label(txt(format!("Batería {l} %{carga}"), 13.0, theme::MUTED));
            }
            ui.add_space(10.0);
            let w = (ui.available_width() - 8.0) / 2.0;
            ui.horizontal(|ui| {
                if toggle(ui, &format!("\u{F0100}  Cámara{}", if cam { " sí" } else { "" }), cam, w) {
                    send(self.media_port, "/set?cam=toggle".into());
                }
                if toggle(ui, "\u{F0450}  Girar", false, w) {
                    send(self.media_port, "/set?facing=flip".into());
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if toggle(ui, &format!("\u{F036C}  Micro{}", if mic { " sí" } else { "" }), mic, w) {
                    send(self.media_port, "/set?mic=toggle".into());
                }
                let sb = st["standby"].as_bool().unwrap_or(false);
                if toggle(ui, "\u{F0594}  Stand-by", sb, w) {
                    send(self.answer_port, "/standby?on=toggle".into());
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if toggle(ui, "\u{F0425}  Pantalla", false, w) {
                    // alterna apagada/encendida (el servicio no guarda el estado: se apaga, y otro clic enciende)
                    self.screen_off = !self.screen_off;
                    let off = self.screen_off;
                    send(self.answer_port, format!("/screen?on={}", if off { 0 } else { 1 }));
                }
                let b = egui::Button::new(txt("Desconectar", 14.0, theme::BG)).fill(theme::RED).corner_radius(10.0).min_size(egui::vec2(w, 36.0));
                if ui.add(b).clicked() {
                    send(self.answer_port, "/disconnect".into());
                }
            });
            if let Some(e) = md["error"].as_str().filter(|e| !e.is_empty()) {
                ui.add_space(8.0);
                ui.label(txt(e, 12.0, theme::YELLOW));
            }
        });
    }
}

pub fn run() -> Result<(), String> {
    // si ya hay uno abierto, no se abre otro
    if alive() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    crate::linux::ensure_env();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Escritorio")
            .with_app_id("pantalla-host")
            .with_inner_size([W, 120.0])
            .with_resizable(false),
        ..Default::default()
    };
    let r = eframe::run_native("Escritorio", opts, Box::new(|cc| {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("nf".into(), Arc::new(egui::FontData::from_static(include_bytes!("../app/assets/nf.ttf"))));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("nf".into());
        }
        cc.egui_ctx.set_fonts(fonts);
        Ok(Box::new(Panel::new(cc)))
    }));
    let _ = std::fs::remove_file(alive_path());
    r.map_err(|e| e.to_string())
}
