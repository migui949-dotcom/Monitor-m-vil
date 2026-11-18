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

/// Mismos colores que la barra Quickshell (Catppuccin Mocha + cristal azul)
mod theme {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x15, 0x1b, 0x38);
    pub const TEXT: Color32 = Color32::from_rgb(0xcd, 0xd6, 0xf4);
    pub const SUBTEXT: Color32 = Color32::from_rgb(0xa6, 0xad, 0xc8);
    pub const BLUE: Color32 = Color32::from_rgb(0x89, 0xb4, 0xfa);
    pub const SKY: Color32 = Color32::from_rgb(0x89, 0xdc, 0xeb);
    pub const LAVENDER: Color32 = Color32::from_rgb(0xb4, 0xbe, 0xfe);
    pub const GREEN: Color32 = Color32::from_rgb(0xa6, 0xe3, 0xa1);
    pub const YELLOW: Color32 = Color32::from_rgb(0xf9, 0xe2, 0xaf);
    pub const PEACH: Color32 = Color32::from_rgb(0xfa, 0xb3, 0x87);
    pub const RED: Color32 = Color32::from_rgb(0xf3, 0x8b, 0xa8);
    pub fn alpha(c: Color32, a: f32) -> Color32 {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * 255.0) as u8)
    }
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

/// Un proceso hijo que sigue a su aire: alguien tiene que recoger su salida (wait) o queda
/// como zombi colgando del servicio
fn detach(c: std::io::Result<std::process::Child>) {
    if let Ok(mut c) = c {
        std::thread::spawn(move || {
            let _ = c.wait();
        });
    }
}

/// Abre el panel como proceso aparte (desde el servicio)
pub fn spawn() {
    let _ = std::fs::write(alive_path(), b"1"); // que no se abran dos a la vez
    if let Ok(me) = std::env::current_exe() {
        detach(std::process::Command::new(me)
            .arg("panel")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn());
    }
}

fn camera_alive_path(s: u64) -> std::path::PathBuf {
    crate::util::state_dir().join(if s <= 1 { "camara.vivo".to_string() } else { format!("camara-{s}.vivo") })
}

fn camera_window_open(s: u64) -> bool {
    std::fs::metadata(camera_alive_path(s)).and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < Duration::from_secs(3)).unwrap_or(false)).unwrap_or(false)
}

/// Saca la cámara del móvil `s` a su propia ventana (si no está ya abierta): `pantalla-host camara N`
pub fn spawn_camera(s: u64) {
    if camera_window_open(s) {
        return;
    }
    let _ = std::fs::write(camera_alive_path(s), b"1");
    if let Ok(me) = std::env::current_exe() {
        detach(std::process::Command::new(me).arg("camara").arg(s.to_string())
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn());
    }
}

fn get_bytes(port: u16, path: &str) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut r = Vec::new();
    s.read_to_end(&mut r).ok()?;
    let i = r.windows(4).position(|w| w == b"\r\n\r\n")?;
    if !r.starts_with(b"HTTP/1.0 200") && !r.starts_with(b"HTTP/1.1 200") {
        return None;
    }
    Some(r[i + 4..].to_vec())
}

fn get(port: u16, path: &str) -> Option<String> {
    get_bytes(port, path).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Orden al servicio en segundo plano (no bloquea la ventana)
fn send(port: u16, path: String) {
    std::thread::spawn(move || {
        let _ = get(port, &path);
    });
}

fn movil_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Móvil")
}

/// Espacio usado en la carpeta del móvil ("1.2 / 10 GB")
fn almacen(dir: &std::path::Path) -> String {
    let Ok(c) = std::ffi::CString::new(dir.to_string_lossy().as_bytes()) else { return String::new() };
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut v) } != 0 || v.f_blocks == 0 {
        return String::new();
    }
    let (size, free) = (v.f_blocks as f64 * v.f_frsize as f64, v.f_bfree as f64 * v.f_frsize as f64);
    format!("{:.1} / {} GB", (size - free) / 1e9, (size / 1e9).round())
}

/// Un móvil conectado. `s` es su ranura (1 = la de siempre: MOVIL, ~/Móvil, /dev/video42…)
#[derive(Clone)]
pub(crate) struct Sess {
    pub s: u64,
    /// la entrada de "sessions" de movild.json (peer, phone, standby, position, storage…)
    pub info: Value,
    /// cámara/micro de este móvil (cam, mic, facing, error…)
    pub media: Value,
}

impl Sess {
    pub fn device(&self) -> String {
        self.info["phone"]["device"].as_str().or(self.info["peer"]["device"].as_str()).unwrap_or("Móvil").to_string()
    }
    pub fn storage(&self) -> std::path::PathBuf {
        self.info["storage"].as_str().map(std::path::PathBuf::from).unwrap_or_else(movil_dir)
    }
}

/// Los móviles conectados según movild.json (+ /state del 6083). Con un servicio de los de antes
/// (sin "sessions") sale uno solo, el s=1, con los campos de siempre.
pub(crate) fn sessions(st: &Value, md: &Value) -> Vec<Sess> {
    if let Some(arr) = st["sessions"].as_array() {
        let mut v: Vec<Sess> = arr.iter().enumerate().map(|(i, x)| {
            let s = x["s"].as_u64().unwrap_or(i as u64 + 1);
            let media = if x["media"].is_object() {
                x["media"].clone()
            } else if let Some(m) = md["sessions"].as_array().and_then(|a| a.iter().find(|m| m["s"].as_u64() == Some(s)).or(a.get(i))) {
                m.clone()
            } else if i == 0 {
                md.clone()
            } else {
                Value::Null
            };
            Sess { s, info: x.clone(), media }
        }).collect();
        v.sort_by_key(|x| x.s);
        return v;
    }
    if st["connected"].as_bool().unwrap_or(false) {
        let info = serde_json::json!({ "peer": st["peer"], "phone": st["phone"], "standby": st["standby"] });
        return vec![Sess { s: 1, info, media: md.clone() }];
    }
    vec![]
}

/// Móviles esperando a que se les acepte ("pendings"; con el servicio viejo, "pending")
pub(crate) fn pendings(st: &Value) -> Vec<Value> {
    match st["pendings"].as_array() {
        Some(a) => a.clone(),
        None => st.get("pending").filter(|p| !p.is_null()).cloned().into_iter().collect(),
    }
}

/// Añade &s=N a una orden; solo si el servicio sabe de varios móviles (los viejos no lo entienden)
pub(crate) fn with_s(path: &str, s: u64, multi: bool) -> String {
    if !multi {
        return path.to_string();
    }
    format!("{path}{}s={s}", if path.contains('?') { '&' } else { '?' })
}

#[derive(Default)]
struct Data {
    status: Value,
    media: Value,
    /// carpeta → "usado / total"
    almacen: std::collections::HashMap<std::path::PathBuf, String>,
    /// móvil cuya cámara se enseña (lo elige la ventana)
    sel: u64,
    preview: Option<Vec<u8>>,
    preview_s: u64,
    preview_gen: u64,
    window: bool,
}

struct Panel {
    data: Arc<Mutex<Data>>,
    /// móvil elegido en las pestañas
    sel: u64,
    /// pantalla del móvil apagada (por ranura)
    dark: std::collections::HashMap<u64, bool>,
    height: f32,
    answer_port: u16,
    media_port: u16,
    tex: Option<egui::TextureHandle>,
    tex_gen: u64,
    bright: Option<(u64, f32)>,
    bright_sent: std::time::Instant,
}

impl Panel {
    fn new(cc: &eframe::CreationContext) -> Panel {
        let mut v = egui::Visuals::dark();
        v.panel_fill = theme::BG;
        v.window_fill = theme::BG;
        v.selection.bg_fill = theme::YELLOW;
        v.widgets.inactive.bg_fill = theme::alpha(theme::TEXT, 0.1);
        cc.egui_ctx.set_visuals(v);
        let data = Arc::new(Mutex::new(Data { sel: 1, ..Default::default() }));
        let (answer_port, media_port) = (crate::util::env_port("MOVILD_ANSWER_PORT", 47802), 6083);
        // cada segundo: estado del servicio (movild.json), cámara/micro y carpetas; y "estoy vivo"
        {
            let (data, ctx) = (data.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || loop {
                let _ = std::fs::write(alive_path(), b"1");
                let status = std::fs::read_to_string(crate::host::status_path())
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or(Value::Null);
                let media: Value = get(media_port, "/state").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
                let mut alm = std::collections::HashMap::new();
                for x in sessions(&status, &media) {
                    let dir = x.storage();
                    let a = almacen(&dir);
                    alm.insert(dir, a);
                }
                {
                    let mut d = data.lock().unwrap();
                    d.status = status;
                    d.media = media;
                    d.almacen = alm;
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(1));
            });
        }
        // vista de la cámara del móvil elegido (~25 imágenes/s), si está encendida y sin ventana propia
        {
            let (data, ctx) = (data.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || loop {
                let (on, s, multi) = {
                    let d = data.lock().unwrap();
                    let v = sessions(&d.status, &d.media);
                    let on = v.iter().find(|x| x.s == d.sel).map(|x| x.media["cam"].as_bool().unwrap_or(false)).unwrap_or(false);
                    (on, d.sel, d.status["sessions"].is_array())
                };
                let window = camera_window_open(s);
                data.lock().unwrap().window = window;
                if !on || window {
                    data.lock().unwrap().preview = None;
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                let px = get_bytes(media_port, &with_s("/preview", s, multi))
                    .filter(|p| p.len() == crate::devices::PREVIEW_W * crate::devices::PREVIEW_H * 3);
                {
                    let mut d = data.lock().unwrap();
                    d.preview = px;
                    d.preview_s = s;
                    d.preview_gen += 1;
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_millis(40));
            });
        }
        Panel {
            tex: None,
            tex_gen: 0,
            data,
            sel: 1,
            dark: Default::default(),
            height: 0.0,
            answer_port,
            media_port,
            bright: None,
            bright_sent: std::time::Instant::now(),
        }
    }
}

fn txt(t: impl Into<String>, size: f32, c: egui::Color32) -> egui::RichText {
    egui::RichText::new(t).size(size).color(c)
}

/// Botón redondo como los de la barra: icono + texto; encendido = fondo y borde del color
fn pill(ui: &mut egui::Ui, icon: &str, text: &str, active: bool, accent: egui::Color32) -> bool {
    let mut job = egui::text::LayoutJob::default();
    let font = |s: f32| egui::FontId::proportional(s);
    if !icon.is_empty() {
        job.append(icon, 0.0, egui::TextFormat::simple(font(14.0), if active { accent } else { theme::TEXT }));
    }
    job.append(text, if icon.is_empty() { 0.0 } else { 6.0 }, egui::TextFormat::simple(font(12.0), theme::TEXT));
    let fill = if active { theme::alpha(accent, 0.25) } else { theme::alpha(egui::Color32::WHITE, 0.05) };
    let b = egui::Button::new(job)
        .fill(fill)
        .stroke(if active { egui::Stroke::new(1.0, accent) } else { egui::Stroke::NONE })
        .corner_radius(16.0)
        .min_size(egui::vec2(0.0, 32.0));
    ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Recuadro de dato (Batería, Temperatura…)
fn tile(ui: &mut egui::Ui, w: f32, icon: &str, title: &str, value: &str, accent: egui::Color32) {
    egui::Frame::new()
        .fill(theme::alpha(egui::Color32::WHITE, 0.05))
        .stroke(egui::Stroke::new(1.0, theme::alpha(accent, 0.25)))
        .corner_radius(14.0)
        .inner_margin(egui::Margin { left: 12, right: 8, top: 10, bottom: 10 })
        .show(ui, |ui| {
            ui.set_width(w - 20.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.add(egui::Label::new(txt(format!("{icon}  {title}"), 11.0, theme::SUBTEXT)).truncate());
                ui.add(egui::Label::new(txt(value, 16.0, accent).strong()).truncate());
            });
        });
}

impl eframe::App for Panel {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (st, md, alm, preview, window) = {
            let mut d = self.data.lock().unwrap();
            d.sel = self.sel;
            let pv = if d.preview_gen != self.tex_gen {
                self.tex_gen = d.preview_gen;
                Some(if d.preview_s == self.sel { d.preview.take() } else { None })
            } else {
                None
            };
            (d.status.clone(), d.media.clone(), d.almacen.clone(), pv, d.window)
        };
        if let Some(Some(px)) = preview {
            let img = egui::ColorImage::from_rgb([crate::devices::PREVIEW_W, crate::devices::PREVIEW_H], &px);
            match self.tex.as_mut() {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.tex = Some(ui.ctx().load_texture("camara", img, egui::TextureOptions::LINEAR)),
            }
        }
        let ctx = ui.ctx().clone();
        let multi = st["sessions"].is_array();
        let list = sessions(&st, &md);
        let pend = pendings(&st);
        self.dark.retain(|s, _| list.iter().any(|x| x.s == *s));
        // si el elegido se ha ido, el primero que quede
        if !list.iter().any(|x| x.s == self.sel) {
            self.sel = list.first().map(|x| x.s).unwrap_or(1);
            self.tex = None;
        }
        let cur = list.iter().find(|x| x.s == self.sel).cloned();
        let (ap, mp, sel) = (self.answer_port, self.media_port, self.sel);
        let q = move |p: &str| with_s(p, sel, multi);

        let inner = egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(14.0)).show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
            ui.vertical(|ui| {
            // Pestañas: una por móvil (solo si hay más de uno)
            if list.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for x in &list {
                        let cam_on = x.media["cam"].as_bool().unwrap_or(false);
                        let icon = if cam_on { "\u{F0100}" } else { "\u{F011C}" };
                        if pill(ui, icon, &x.device(), x.s == self.sel, theme::BLUE) && x.s != self.sel {
                            self.sel = x.s;
                            self.tex = None;
                            self.bright = None;
                        }
                    }
                });
            }
            ui.vertical(|ui| {
                // Cabecera
                ui.spacing_mut().item_spacing.y = 2.0;
                match &cur {
                    Some(x) => {
                        ui.label(txt(x.device(), 15.0, theme::TEXT).strong());
                        let via = if x.info["peer"]["via"].as_str() == Some("cable") { "por cable" } else { "por wifi" };
                        let extra = match (x.info["output"].as_str(), list.len() > 1) {
                            (Some(o), true) => format!(" · {o}"),
                            _ => String::new(),
                        };
                        ui.label(txt(format!("Conectado {via} · cifrado{extra}"), 11.0, theme::GREEN));
                    }
                    None => {
                        ui.label(txt("Móvil", 15.0, theme::TEXT).strong());
                        ui.label(txt("Sin conectar · ábrelo desde la app «Escritorio» del móvil", 11.0, theme::SUBTEXT));
                    }
                }
            });

            // móviles que piden conectarse
            for p in &pend {
                egui::Frame::new()
                    .fill(theme::alpha(theme::YELLOW, 0.08))
                    .stroke(egui::Stroke::new(1.0, theme::alpha(theme::YELLOW, 0.35)))
                    .corner_radius(14.0)
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(txt(format!("{} quiere conectarse", p["device"].as_str().unwrap_or("Un móvil")), 14.0, theme::YELLOW).strong());
                        ui.label(txt(format!("Huella {} · por {}", p["fp"].as_str().unwrap_or("?"), p["via"].as_str().unwrap_or("?")), 11.0, theme::SUBTEXT));
                        ui.horizontal(|ui| {
                            let id = p["id"].as_str().unwrap_or("").to_string();
                            if pill(ui, "\u{F012C}", "Aceptar", true, theme::GREEN) {
                                send(ap, format!("/answer?id={id}&ok=1"));
                            }
                            if pill(ui, "\u{F0156}", "Rechazar", true, theme::RED) {
                                send(ap, format!("/answer?id={id}&ok=0"));
                            }
                        });
                    });
            }
            let Some(x) = cur.as_ref() else { return };
            let (ph, md) = (&x.info["phone"], &x.media);
            let (cam, mic) = (md["cam"].as_bool().unwrap_or(false), md["mic"].as_bool().unwrap_or(false));
            let dark = self.dark.get(&x.s).copied().unwrap_or(false);

            // Datos
            let w = (ui.available_width() - 8.0) / 2.0;
            let level = ph["level"].as_i64().filter(|l| *l >= 0);
            let charging = ph["charging"].as_bool().unwrap_or(false);
            ui.horizontal(|ui| {
                tile(ui, w, if charging { "\u{F0084}" } else { "\u{F0079}" }, "Batería",
                     &level.map(|l| format!("{l}%{}", if charging { " ⚡" } else { "" })).unwrap_or("—".into()),
                     if level.unwrap_or(100) < 15 && !charging { theme::RED } else { theme::GREEN });
                let t = ph["cpu_temp"].as_f64().filter(|t| *t > 0.0).or(ph["btemp"].as_f64());
                tile(ui, w, "\u{F050F}", "Temperatura", &t.map(|t| format!("{} °C", t.round())).unwrap_or("—".into()), theme::SKY);
            });
            ui.horizontal(|ui| {
                let (tot, free) = (ph["stotal"].as_f64().unwrap_or(0.0), ph["sfree"].as_f64().unwrap_or(0.0));
                let used = if tot > 0.0 { format!("{}% usado", ((tot - free) / tot * 100.0).round()) } else { "—".into() };
                tile(ui, w, "\u{F02CA}", "Almacenamiento", &used, theme::LAVENDER);
                let dir = x.storage();
                let name = format!("~/{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                let a = alm.get(&dir).cloned().unwrap_or_default();
                tile(ui, w, "\u{F024B}", &name, if a.is_empty() { "—" } else { &a }, theme::YELLOW);
            });

            // Brillo
            ui.horizontal(|ui| {
                ui.label(txt("\u{F00DE}", 15.0, theme::YELLOW));
                let phone_b = ph["brightness"].as_f64().filter(|b| *b >= 0.0).unwrap_or(0.6) as f32;
                let mut b = self.bright.filter(|(s, _)| *s == x.s).map(|(_, b)| b).unwrap_or(phone_b);
                ui.spacing_mut().slider_width = ui.available_width() - 8.0;
                let r = ui.add(egui::Slider::new(&mut b, 0.02..=1.0).show_value(false).trailing_fill(true));
                if r.changed() {
                    self.bright = Some((x.s, b));
                    if self.bright_sent.elapsed() > Duration::from_millis(80) {
                        self.bright_sent = std::time::Instant::now();
                        send(ap, q(&format!("/brightness?v={b:.2}")));
                    }
                }
                if r.drag_stopped() {
                    send(ap, q(&format!("/brightness?v={b:.2}")));
                }
                if !r.dragged() && !r.changed() && (phone_b - b).abs() < 0.02 {
                    self.bright = None;
                }
            });

            // Pantalla / stand-by / archivos
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if pill(ui, "\u{F0425}", if dark { "Apagada" } else { "Pantalla" }, !dark, theme::YELLOW) {
                    self.dark.insert(x.s, !dark);
                    send(ap, q(&format!("/screen?on={}", if dark { 1 } else { 0 })));
                }
                if pill(ui, "\u{F04B2}", "Stand-by", x.info["standby"].as_bool().unwrap_or(false), theme::LAVENDER) {
                    send(ap, q("/standby?on=toggle"));
                }
                // teclado del móvil al pulsar un campo de texto (con el ratón también saldría): sí / no
                let kb = st["teclado"].as_bool().unwrap_or(true);
                if pill(ui, "\u{F030C}", if kb { "Teclado: sale solo" } else { "Teclado: no sale" }, kb, theme::BLUE) {
                    send(ap, "/teclado?on=toggle".into());
                }
                if pill(ui, "\u{F024B}", "Archivos", false, theme::YELLOW) {
                    detach(std::process::Command::new("xdg-open").arg(x.storage()).spawn());
                }
            });

            // Cámara y micrófono
            let cam_label = md["cam_label"].as_str().map(|l| format!("Cámara y micrófono · «{l}»")).unwrap_or("Cámara y micrófono".into());
            ui.add(egui::Label::new(txt(cam_label, 12.0, theme::SUBTEXT)).truncate());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if pill(ui, if cam { "\u{F0100}" } else { "\u{F05DF}" }, "Cámara", cam, theme::RED) {
                    send(mp, q(&format!("/set?cam={}", if cam { "off" } else { "on" })));
                }
                let trasera = md["facing"].as_str() == Some("environment");
                if pill(ui, "\u{F0D19}", if trasera { "Trasera" } else { "Frontal" }, false, theme::BLUE) {
                    send(mp, q("/set?facing=flip"));
                }
                if pill(ui, if mic { "\u{F036C}" } else { "\u{F036D}" }, "Micrófono", mic, theme::RED) {
                    send(mp, q(&format!("/set?mic={}", if mic { "off" } else { "on" })));
                }
            });
            // lo que ve la cámara: aquí, o en su propia ventana (botón "Ventana")
            if cam && !window {
                let size = egui::vec2(ui.available_width(), ui.available_width() * 9.0 / 16.0);
                match &self.tex {
                    Some(t) => {
                        ui.add(egui::Image::new(t).fit_to_exact_size(size).corner_radius(12.0));
                    }
                    None => {
                        egui::Frame::new().fill(theme::alpha(egui::Color32::WHITE, 0.05)).corner_radius(12.0).show(ui, |ui| {
                            ui.set_min_size(size);
                            ui.centered_and_justified(|ui| ui.label(txt("Encendiendo la cámara…", 12.0, theme::SUBTEXT)));
                        });
                    }
                }
                if pill(ui, "\u{F05AF}", "Abrir en una ventana", false, theme::BLUE) {
                    self.tex = None;
                    spawn_camera(x.s);
                }
            } else if cam && window {
                ui.label(txt("\u{F05AF}  La cámara está en su propia ventana", 11.0, theme::SUBTEXT));
            }
            if !cam || window {
                self.tex = None;
            }

            // Posición del monitor del móvil: por ese lado pasa el ratón de una pantalla a otra
            ui.label(txt("Posición del móvil", 12.0, theme::SUBTEXT));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let key = if x.s <= 1 { "POSITION".to_string() } else { format!("POSITION_{}", x.s) };
                let pos = x.info["position"].as_str().map(String::from)
                    .or_else(|| crate::util::conf(&key))
                    .unwrap_or_else(|| "auto-right".into());
                for (p, icon, name) in [("left", "\u{F004D}", "Izq."), ("up", "\u{F005D}", "Arriba"), ("down", "\u{F0045}", "Abajo"), ("right", "\u{F0054}", "Der.")] {
                    if pill(ui, icon, name, pos == format!("auto-{p}"), theme::SKY) {
                        // el servicio nuevo guarda POSITION_n él solo; con el viejo lo guarda el panel
                        if !multi {
                            let _ = crate::util::set_conf("POSITION", &format!("auto-{p}"));
                        }
                        send(ap, q(&format!("/position?p={p}")));
                    }
                }
            });
            if let Some(e) = md["error"].as_str().filter(|e| !e.is_empty()) {
                ui.label(txt(e, 11.0, theme::PEACH));
            }

            let bye = if list.len() > 1 { format!("Desconectar {}", x.device()) } else { "Desconectar".into() };
            if pill(ui, "\u{F0156}", &bye, false, theme::RED) {
                send(ap, q("/disconnect"));
            }
            }).response.rect.height()
        });
        // la ventana se ajusta a lo que enseña
        let content_h = (inner.inner + 28.0).round();
        if (content_h - self.height).abs() > 1.0 {
            self.height = content_h;
            // tamaño fijo (mínimo = máximo): así los gestores en mosaico (Hyprland, Sway…) la dejan
            // flotando como un desplegable en vez de ocupar toda la pantalla
            let sz = egui::vec2(W, content_h);
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(sz));
            ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(sz));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(sz));
        }
    }
}

/// Ventana "Cámara del móvil": lo que ve la cámara, en una ventana normal (se mueve y se cambia de
/// tamaño como cualquier otra). Se cierra sola si la cámara se apaga.
struct Camera {
    frame: Arc<Mutex<(u64, Option<Vec<u8>>, bool)>>,
    tex: Option<egui::TextureHandle>,
    seen: u64,
}

impl eframe::App for Camera {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let (gen, px, on) = {
            let mut f = self.frame.lock().unwrap();
            let px = if f.0 != self.seen { f.1.take() } else { None };
            (f.0, px, f.2)
        };
        if !on {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if gen != self.seen {
            self.seen = gen;
            if let Some(px) = px {
                let img = egui::ColorImage::from_rgb([crate::devices::PREVIEW_W, crate::devices::PREVIEW_H], &px);
                match self.tex.as_mut() {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.tex = Some(ctx.load_texture("camara", img, egui::TextureOptions::LINEAR)),
                }
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ui, |ui| {
            let avail = ui.available_size();
            let ratio = crate::devices::PREVIEW_W as f32 / crate::devices::PREVIEW_H as f32;
            let size = if avail.x / avail.y > ratio { egui::vec2(avail.y * ratio, avail.y) } else { egui::vec2(avail.x, avail.x / ratio) };
            ui.centered_and_justified(|ui| match &self.tex {
                Some(t) => {
                    ui.add(egui::Image::new(t).fit_to_exact_size(size));
                }
                None => {
                    ui.label(txt("Encendiendo la cámara…", 14.0, theme::SUBTEXT));
                }
            });
        });
    }
}

pub fn run_camera() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    crate::linux::ensure_env();
    // `pantalla-host camara [N]`: N = ranura del móvil (1 si no se dice)
    let s: u64 = std::env::args().skip_while(|a| a != "camara" && a != "cámara").nth(1)
        .and_then(|a| a.parse().ok()).unwrap_or(1);
    let read_status = || -> Value {
        std::fs::read_to_string(crate::host::status_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
    };
    let st0 = read_status();
    let multi = st0["sessions"].is_array();
    let title = match sessions(&st0, &Value::Null).into_iter().find(|x| x.s == s) {
        Some(x) if multi => format!("Cámara de {}", x.device()),
        _ => "Cámara del móvil".to_string(),
    };
    let frame = Arc::new(Mutex::new((0u64, None::<Vec<u8>>, true)));
    {
        let frame = frame.clone();
        std::thread::spawn(move || {
            let mut off_since: Option<std::time::Instant> = None;
            let mut last_state = std::time::Instant::now() - Duration::from_secs(5);
            let mut ctx_wait = 0;
            loop {
                let _ = std::fs::write(camera_alive_path(s), b"1");
                // ¿sigue encendida? (cada segundo; se cierra a los 3 s de apagarse)
                if last_state.elapsed() > Duration::from_secs(1) {
                    last_state = std::time::Instant::now();
                    let md = get(6083, "/state").and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(Value::Null);
                    let on = sessions(&read_status(), &md).into_iter().find(|x| x.s == s)
                        .map(|x| x.media["cam"].as_bool().unwrap_or(false)).unwrap_or(false);
                    off_since = if on { None } else { off_since.or(Some(std::time::Instant::now())) };
                    if off_since.map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(false) {
                        frame.lock().unwrap().2 = false;
                    }
                }
                let px = get_bytes(6083, &with_s("/preview", s, multi))
                    .filter(|p| p.len() == crate::devices::PREVIEW_W * crate::devices::PREVIEW_H * 3);
                {
                    let mut f = frame.lock().unwrap();
                    f.0 += 1;
                    f.1 = px;
                }
                ctx_wait += 1;
                std::thread::sleep(Duration::from_millis(40));
                let _ = ctx_wait;
            }
        });
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_app_id("pantalla-host-camara")
            .with_inner_size([640.0, 360.0])
            .with_min_inner_size([160.0, 90.0])
            .with_resizable(true),
        ..Default::default()
    };
    let r = eframe::run_native(&title, opts, Box::new(move |cc| {
        // repintar al ritmo de las imágenes
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || loop {
            ctx.request_repaint();
            std::thread::sleep(Duration::from_millis(40));
        });
        Ok(Box::new(Camera { frame, tex: None, seen: 0 }))
    }));
    let _ = std::fs::remove_file(camera_alive_path(s));
    r.map_err(|e| e.to_string())
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
            .with_inner_size([W, 200.0])
            .with_min_inner_size([W, 200.0])
            .with_max_inner_size([W, 200.0])
            .with_resizable(true),
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
