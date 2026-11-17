//! Stand-by nativo: escenas (reloj, tiempo, próximos días, recordatorios, música, PC, firma) con
//! fundidos, sobre el fondo animado. Los datos los manda movild cada 4 s ({"t": "sb", ...}).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke};
use serde_json::Value;

use crate::standby_bg::Background;

const BG_EVERY: Duration = Duration::from_secs(180);
const BG_FADE: f32 = 5.0;
const FADE_OUT: f32 = 0.7;
const FADE_IN: f32 = 0.9;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Scene { Clock, Weather, Forecast, Agenda, Music, Pc, Claude }
const ORDER: [Scene; 7] = [Scene::Clock, Scene::Weather, Scene::Agenda, Scene::Music, Scene::Forecast, Scene::Pc, Scene::Claude];

fn dur(s: Scene) -> Duration {
    Duration::from_secs(match s { Scene::Clock => 20, Scene::Weather | Scene::Agenda => 14, Scene::Music => 16,
                                  Scene::Forecast => 12, Scene::Pc => 11, Scene::Claude => 9 })
}

// Paleta Tokyo Night
const FG: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
const BRIGHT: Color32 = Color32::from_rgb(0xe4, 0xe9, 0xff);
const DIM: Color32 = Color32::from_rgb(0xa9, 0xb1, 0xd6);
const MUTED: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
const BLUE: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
const CYAN: Color32 = Color32::from_rgb(0x7d, 0xcf, 0xff);
const PURPLE: Color32 = Color32::from_rgb(0xbb, 0x9a, 0xf7);
const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
const ORANGE: Color32 = Color32::from_rgb(0xff, 0x9e, 0x64);
const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
const CORAL: Color32 = Color32::from_rgb(0xd9, 0x77, 0x57);

pub struct Standby {
    scene: Scene,
    since: Instant,
    prev: Option<(Scene, Instant)>,
    t0: Instant,
    bg: Arc<Mutex<Option<Background>>>,
    bg_a: f32,
    bg_b: f32,
    bg_switch: Instant,
}

/// Hora local (España, invierno UTC+1): (año, mes, día, hora, min, seg, ms, día de la semana 0=domingo)
fn local() -> (i64, i64, i64, u64, u64, u64, u32, u64) {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = t.as_secs() + 3600;
    let (days, rem) = (s / 86400, s % 86400);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m, d, rem / 3600, (rem / 60) % 60, rem % 60, t.subsec_millis(), (days + 4) % 7)
}

const DAYS: [&str; 7] = ["domingo", "lunes", "martes", "miércoles", "jueves", "viernes", "sábado"];
const MONTHS: [&str; 12] = ["enero", "febrero", "marzo", "abril", "mayo", "junio", "julio", "agosto",
                             "septiembre", "octubre", "noviembre", "diciembre"];

fn cap(s: &str) -> String { let mut c = s.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() }

fn w_icon(code: i64, day: bool) -> &'static str {
    match code {
        0 | 1 => if day { "\u{F0599}" } else { "\u{F0594}" },
        2 => if day { "\u{F0595}" } else { "\u{F0F31}" },
        3 => "\u{F0590}",
        45 | 48 => "\u{F0591}",
        51..=57 | 61 | 63 | 80 | 81 => "\u{F0597}",
        65 | 82 => "\u{F0596}",
        66 | 67 => "\u{F067F}",
        71..=77 | 85 | 86 => "\u{F0598}",
        95 => "\u{F067E}",
        96 | 99 => "\u{F0592}",
        _ => "\u{F0590}",
    }
}
fn w_color(code: i64, day: bool) -> Color32 {
    match code {
        0 | 1 => if day { YELLOW } else { PURPLE },
        2 => if day { ORANGE } else { PURPLE },
        3 | 45 | 48 => DIM,
        71..=77 | 85 | 86 => FG,
        95..=99 => PURPLE,
        _ => BLUE,
    }
}
fn w_desc(code: i64) -> &'static str {
    match code {
        0 => "Despejado", 1 => "Mayormente despejado", 2 => "Parcialmente nublado", 3 => "Nublado",
        45 | 48 => "Niebla", 51..=55 => "Llovizna", 56 | 57 => "Llovizna helada", 61 => "Lluvia débil", 63 => "Lluvia",
        65 => "Lluvia fuerte", 66 | 67 => "Lluvia helada", 71 => "Nieve débil", 73 => "Nieve", 75 => "Nieve fuerte",
        77 => "Granizo fino", 80 | 81 => "Chubascos", 82 => "Chubascos fuertes", 85 | 86 => "Chubascos de nieve",
        95 => "Tormenta", 96 | 99 => "Tormenta con granizo", _ => "—",
    }
}

/// Pintor con opacidad (para los fundidos)
struct P<'a> { p: &'a egui::Painter, a: f32 }
impl P<'_> {
    fn c(&self, c: Color32) -> Color32 { c.gamma_multiply(self.a) }
    fn text(&self, pos: Pos2, al: Align2, s: impl ToString, size: f32, c: Color32) -> Rect {
        self.p.text(pos, al, s.to_string(), FontId::proportional(size), self.c(c))
    }
    fn bar(&self, r: Rect, frac: f32, c: Color32) {
        self.p.rect_filled(r, r.height() / 2.0, self.c(Color32::from_rgba_unmultiplied(192, 202, 245, 20)));
        let mut f = r; f.set_width(r.width() * frac.clamp(0.0, 1.0));
        self.p.rect_filled(f, r.height() / 2.0, self.c(c));
    }
    fn arc(&self, c: Pos2, r: f32, frac: f32, w: f32, col: Color32) {
        let n = 64;
        let pts = |f: f32| (0..=n).map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * f * i as f32 / n as f32;
            Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
        }).collect::<Vec<_>>();
        self.p.add(egui::Shape::line(pts(1.0), Stroke::new(w, self.c(Color32::from_rgba_unmultiplied(192, 202, 245, 20)))));
        if frac > 0.001 { self.p.add(egui::Shape::line(pts(frac.min(1.0)), Stroke::new(w, self.c(col)))); }
    }
}

impl Standby {
    pub fn new() -> Standby {
        Standby { scene: Scene::Clock, since: Instant::now(), prev: None, t0: Instant::now(),
                  bg: Arc::new(Mutex::new(None)), bg_a: 0.0, bg_b: 0.0, bg_switch: Instant::now() }
    }

    fn available(d: &Value, s: Scene) -> bool {
        match s {
            Scene::Clock | Scene::Claude => true,
            Scene::Weather => d["weather"].is_object(),
            Scene::Forecast => d["weather"]["days"].as_array().map(|a| a.len() > 1).unwrap_or(false),
            Scene::Agenda => d["reminders"]["upcoming"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
                || d["reminders"]["today"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
            Scene::Music => d["music"]["playing"].as_bool().unwrap_or(false),
            Scene::Pc => d["pc"].is_object(),
        }
    }

    /// Al entrar en el stand-by: empieza por el reloj
    pub fn reset(&mut self) {
        self.scene = Scene::Clock;
        self.since = Instant::now();
        self.prev = None;
    }

    pub fn next(&mut self, d: &Value) {
        let i = ORDER.iter().position(|s| *s == self.scene).unwrap_or(0);
        for k in 1..=ORDER.len() {
            let s = ORDER[(i + k) % ORDER.len()];
            if Self::available(d, s) {
                if s != self.scene { self.prev = Some((self.scene, Instant::now())); }
                self.scene = s;
                self.since = Instant::now();
                return;
            }
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, d: &Value) {
        let full = ui.max_rect();
        let t = self.t0.elapsed().as_secs_f32();

        // fondo animado (cambia cada 3 min con fundido)
        if self.bg_switch.elapsed() > BG_EVERY && self.bg_a == self.bg_b {
            self.bg_b = ((self.bg_a as i32 + 1 + (t as i32 % 2)) % 3) as f32;
            self.bg_switch = Instant::now();
        }
        let mut m = 0.0;
        if self.bg_a != self.bg_b {
            m = (self.bg_switch.elapsed().as_secs_f32() / BG_FADE).min(1.0);
            m = m * m * (3.0 - 2.0 * m);
            if m >= 1.0 { self.bg_a = self.bg_b; m = 0.0; }
        }
        let (bg, a, b) = (self.bg.clone(), self.bg_a, self.bg_b);
        ui.painter().add(egui::PaintCallback {
            rect: full,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                let gl_id = Arc::as_ptr(painter.gl()) as usize;
                let mut g = bg.lock().unwrap();
                if g.as_ref().map(|x| x.gl_id != gl_id).unwrap_or(true) { *g = Some(Background::new(painter.gl(), gl_id)); }
                let vp = info.viewport_in_pixels();
                g.as_mut().unwrap().paint(painter.gl(), [vp.left_px as f32, vp.from_bottom_px as f32, vp.width_px as f32, vp.height_px as f32],
                                          t % 3600.0, a, b, m);
            })),
        });

        // cambio de escena por tiempo (y si la actual deja de tener datos)
        if self.since.elapsed() > dur(self.scene) || !Self::available(d, self.scene) { self.next(d); }

        let painter = ui.painter_at(full);
        if let Some((ps, t0)) = self.prev {
            let k = t0.elapsed().as_secs_f32() / FADE_OUT;
            if k < 1.0 { self.scene_ui(&P { p: &painter, a: 1.0 - ease(k) }, full, ps, d, t, -k * 12.0); } else { self.prev = None; }
        }
        let k = (self.since.elapsed().as_secs_f32() - if self.prev.is_some() { FADE_OUT * 0.5 } else { 0.0 }) / FADE_IN;
        let k = k.clamp(0.0, 1.0);
        self.scene_ui(&P { p: &painter, a: ease(k) }, full, self.scene, d, t, (1.0 - ease(k)) * 14.0);

        // hora pequeña arriba (salvo en el reloj) y puntos de escena abajo
        let pp = P { p: &painter, a: 1.0 };
        let (_, _, _, hh, mm, _, _, _) = local();
        if self.scene != Scene::Clock {
            pp.text(full.left_top() + egui::vec2(36.0, 26.0), Align2::LEFT_TOP, format!("{hh:02}:{mm:02}"), 18.0, DIM);
        }
        let av: Vec<Scene> = ORDER.iter().copied().filter(|s| Self::available(d, *s)).collect();
        let total = av.len() as f32 * 14.0;
        for (i, s) in av.iter().enumerate() {
            let on = *s == self.scene;
            let x = full.center().x - total / 2.0 + i as f32 * 14.0;
            let r = Rect::from_center_size(Pos2::new(x + if on { 6.0 } else { 0.0 }, full.bottom() - 18.0), egui::vec2(if on { 20.0 } else { 6.0 }, 6.0));
            painter.rect_filled(r, 3.0, Color32::from_rgba_unmultiplied(192, 202, 245, if on { 140 } else { 45 }));
        }
        // de 0:30 a 7:30, más oscuro
        let mins = hh * 60 + mm;
        if (30..450).contains(&mins) { painter.rect_filled(full, 0.0, Color32::from_black_alpha(130)); }
        ui.ctx().request_repaint();
    }

    fn scene_ui(&self, p: &P, r: Rect, s: Scene, d: &Value, t: f32, dy: f32) {
        let c = r.center() + egui::vec2(0.0, dy);
        let h = r.height();
        match s {
            Scene::Clock => {
                let (y, mo, da, hh, mm, ss, ms, wd) = local();
                let _ = y;
                p.text(c - egui::vec2(0.0, h * 0.09), Align2::CENTER_CENTER, format!("{hh:02}:{mm:02}"), h * 0.36, BRIGHT);
                let bar = Rect::from_center_size(c + egui::vec2(0.0, h * 0.13), egui::vec2(r.width() * 0.34, 3.0));
                p.bar(bar, (ss as f32 + ms as f32 / 1000.0) / 60.0, BLUE);
                p.text(c + egui::vec2(0.0, h * 0.21), Align2::CENTER_CENTER,
                       format!("{}, {} de {}", cap(DAYS[wd as usize]), da, MONTHS[(mo - 1) as usize]), h * 0.065, DIM);
                if let Some(w) = d["weather"].as_object() {
                    let code = w["code"].as_i64().unwrap_or(3);
                    let day = w["day"].as_bool().unwrap_or(true);
                    p.text(c + egui::vec2(0.0, h * 0.30), Align2::CENTER_CENTER,
                           format!("{}  {}° · {}", w_icon(code, day), w["temp"].as_f64().unwrap_or(0.0).round(), w_desc(code)), h * 0.045, MUTED);
                }
            }
            Scene::Weather => {
                let w = &d["weather"];
                let code = w["code"].as_i64().unwrap_or(3);
                let day = w["day"].as_bool().unwrap_or(true);
                let left = Pos2::new(r.left() + r.width() * 0.12, c.y - h * 0.14);
                p.text(left, Align2::LEFT_CENTER, w_icon(code, day), h * 0.24, w_color(code, day));
                p.text(left + egui::vec2(h * 0.30, 0.0), Align2::LEFT_CENTER, format!("{}°", w["temp"].as_f64().unwrap_or(0.0).round()), h * 0.24, BRIGHT);
                p.text(left + egui::vec2(0.0, h * 0.16), Align2::LEFT_CENTER, w_desc(code), h * 0.065, FG);
                let today = &w["days"][0];
                p.text(left + egui::vec2(0.0, h * 0.235), Align2::LEFT_CENTER,
                       format!("{} · máx {}° mín {}°", w["city"].as_str().unwrap_or(""), today["max"].as_f64().unwrap_or(0.0).round(),
                               today["min"].as_f64().unwrap_or(0.0).round()), h * 0.042, MUTED);
                let right = Pos2::new(r.left() + r.width() * 0.62, c.y - h * 0.22);
                let lines = [("\u{F050F}", ORANGE, format!("Sensación {}°", w["feels"].as_f64().unwrap_or(0.0).round())),
                             ("\u{F058E}", CYAN, format!("Humedad {}%", w["humidity"].as_i64().unwrap_or(0))),
                             ("\u{F059D}", DIM, format!("Viento {} km/h", w["wind"].as_f64().unwrap_or(0.0).round())),
                             ("\u{F054A}", BLUE, format!("Lluvia {}%", today["rain"].as_i64().unwrap_or(0)))];
                for (i, (ic, col, txt)) in lines.iter().enumerate() {
                    let y = right + egui::vec2(0.0, i as f32 * h * 0.085);
                    p.text(y, Align2::LEFT_CENTER, *ic, h * 0.055, *col);
                    p.text(y + egui::vec2(h * 0.08, 0.0), Align2::LEFT_CENTER, txt, h * 0.048, DIM);
                }
                if let Some(hours) = w["hours"].as_array() {
                    let n = hours.len().max(1) as f32;
                    let x0 = r.left() + r.width() * 0.08;
                    let step = r.width() * 0.84 / n;
                    for (i, hr) in hours.iter().enumerate() {
                        let x = x0 + step * (i as f32 + 0.5);
                        let y = r.top() + h * 0.80 + dy;
                        let code = hr["code"].as_i64().unwrap_or(3);
                        let day = hr["day"].as_bool().unwrap_or(true);
                        p.text(Pos2::new(x, y - h * 0.07), Align2::CENTER_CENTER,
                               if i == 0 { "Ahora".to_string() } else { format!("{}h", hr["hour"].as_str().unwrap_or("")) }, h * 0.036, MUTED);
                        p.text(Pos2::new(x, y), Align2::CENTER_CENTER, w_icon(code, day), h * 0.06, w_color(code, day));
                        p.text(Pos2::new(x, y + h * 0.07), Align2::CENTER_CENTER, format!("{}°", hr["temp"].as_f64().unwrap_or(0.0).round()), h * 0.042, FG);
                    }
                }
            }
            Scene::Forecast => {
                p.text(Pos2::new(c.x, r.top() + h * 0.14 + dy), Align2::CENTER_CENTER, "PRÓXIMOS DÍAS", h * 0.035, MUTED);
                let days = d["weather"]["days"].as_array().cloned().unwrap_or_default();
                let days: Vec<&Value> = days.iter().take(5).collect();
                let lo = days.iter().filter_map(|x| x["min"].as_f64()).fold(f64::MAX, f64::min);
                let hi = days.iter().filter_map(|x| x["max"].as_f64()).fold(f64::MIN, f64::max);
                let span = (hi - lo).max(1.0) as f32;
                let n = days.len().max(1) as f32;
                let cw = r.width() * 0.15;
                let gap = r.width() * 0.025;
                let x0 = c.x - (cw * n + gap * (n - 1.0)) / 2.0;
                for (i, dd) in days.iter().enumerate() {
                    let x = x0 + i as f32 * (cw + gap);
                    let card = Rect::from_min_size(Pos2::new(x, c.y - h * 0.28), egui::vec2(cw, h * 0.62));
                    p.p.rect_filled(card, 22.0, p.c(Color32::from_rgba_unmultiplied(36, 40, 59, 90)));
                    let cx = card.center().x;
                    let name = match i { 0 => "Hoy".to_string(), 1 => "Mañana".to_string(), _ => {
                        let date = dd["date"].as_str().unwrap_or("");
                        let wd = weekday_of(date);
                        cap(DAYS[wd])
                    } };
                    let code = dd["code"].as_i64().unwrap_or(3);
                    p.text(Pos2::new(cx, card.top() + h * 0.07), Align2::CENTER_CENTER, name, h * 0.04, DIM);
                    p.text(Pos2::new(cx, card.top() + h * 0.19), Align2::CENTER_CENTER, w_icon(code, true), h * 0.10, w_color(code, true));
                    p.text(Pos2::new(cx, card.top() + h * 0.31), Align2::CENTER_CENTER, format!("{}°", dd["max"].as_f64().unwrap_or(0.0).round()), h * 0.06, FG);
                    let (mn, mx) = (dd["min"].as_f64().unwrap_or(lo) as f32, dd["max"].as_f64().unwrap_or(hi) as f32);
                    let bar = Rect::from_center_size(Pos2::new(cx, card.top() + h * 0.39), egui::vec2(cw * 0.6, 4.0));
                    p.bar(bar, 1.0, Color32::TRANSPARENT);
                    let seg = Rect::from_min_max(Pos2::new(bar.left() + bar.width() * (mn - lo as f32) / span, bar.top()),
                                                 Pos2::new(bar.left() + bar.width() * (mx - lo as f32) / span, bar.bottom()));
                    p.p.rect_filled(seg, 2.0, p.c(ORANGE));
                    p.text(Pos2::new(cx, card.top() + h * 0.46), Align2::CENTER_CENTER, format!("{}°", mn.round()), h * 0.04, MUTED);
                    let rain = dd["rain"].as_i64().unwrap_or(0);
                    if rain >= 20 { p.text(Pos2::new(cx, card.top() + h * 0.54), Align2::CENTER_CENTER, format!("\u{F054A} {rain}%"), h * 0.034, BLUE); }
                }
            }
            Scene::Agenda => {
                let x = r.left() + r.width() * 0.12;
                let up = d["reminders"]["upcoming"].as_array().cloned().unwrap_or_default();
                let today = d["reminders"]["today"].as_array().cloned().unwrap_or_default();
                if let Some(first) = up.first() {
                    p.text(Pos2::new(x, c.y - h * 0.24), Align2::LEFT_CENTER, "PRÓXIMO RECORDATORIO", h * 0.035, MUTED);
                    p.text(Pos2::new(x, c.y - h * 0.12), Align2::LEFT_CENTER, first["text"].as_str().unwrap_or(""), h * 0.09, FG);
                    p.text(Pos2::new(x, c.y), Align2::LEFT_CENTER, format!("\u{F0954}  {} · {}", when(first), first["time"].as_str().unwrap_or("")), h * 0.05, YELLOW);
                    for (i, e) in up.iter().skip(1).take(3).enumerate() {
                        p.text(Pos2::new(x, c.y + h * (0.12 + 0.07 * i as f32)), Align2::LEFT_CENTER,
                               format!("{}   {}", when(e), e["text"].as_str().unwrap_or("")), h * 0.042, DIM);
                    }
                } else if let Some(first) = today.first() {
                    p.text(Pos2::new(x, c.y - h * 0.18), Align2::LEFT_CENTER, "NOTAS DE HOY", h * 0.035, MUTED);
                    p.text(Pos2::new(x, c.y - h * 0.06), Align2::LEFT_CENTER, first["text"].as_str().unwrap_or(""), h * 0.09, FG);
                    for (i, e) in today.iter().skip(1).take(3).enumerate() {
                        p.text(Pos2::new(x, c.y + h * (0.08 + 0.07 * i as f32)), Align2::LEFT_CENTER, format!("· {}", e["text"].as_str().unwrap_or("")), h * 0.042, DIM);
                    }
                }
            }
            Scene::Music => {
                let m = &d["music"];
                let x = r.left() + r.width() * 0.12;
                p.text(Pos2::new(x, c.y - h * 0.22), Align2::LEFT_CENTER, format!("\u{F075A}  SONANDO · {}", m["player"].as_str().unwrap_or("").to_uppercase()), h * 0.035, PURPLE);
                p.text(Pos2::new(x, c.y - h * 0.10), Align2::LEFT_CENTER, m["title"].as_str().unwrap_or(""), h * 0.085, FG);
                p.text(Pos2::new(x, c.y + h * 0.01), Align2::LEFT_CENTER, m["artist"].as_str().unwrap_or(""), h * 0.055, DIM);
                let (pos, len) = (m["pos"].as_f64().unwrap_or(0.0) as f32, m["len"].as_f64().unwrap_or(0.0) as f32);
                let bar = Rect::from_min_size(Pos2::new(x, c.y + h * 0.13), egui::vec2(r.width() * 0.76, 5.0));
                p.bar(bar, if len > 0.0 { pos / len } else { 0.0 }, FG);
                let fmt = |s: f32| format!("{}:{:02}", (s as u32) / 60, (s as u32) % 60);
                p.text(bar.left_bottom() + egui::vec2(0.0, 18.0), Align2::LEFT_CENTER, fmt(pos), h * 0.035, MUTED);
                if len > 0.0 { p.text(bar.right_bottom() + egui::vec2(0.0, 18.0), Align2::RIGHT_CENTER, fmt(len), h * 0.035, MUTED); }
            }
            Scene::Pc => {
                let pc = &d["pc"];
                p.text(Pos2::new(c.x, r.top() + h * 0.14 + dy), Align2::CENTER_CENTER, "TU PC", h * 0.035, MUTED);
                let mut rings = vec![("CPU", pc["cpu"].as_f64().unwrap_or(0.0) as f32, BLUE,
                                      pc["cpuTemp"].as_f64().map(|t| format!("{} °C", t.round())).unwrap_or_default()),
                                     ("RAM", pc["ram"].as_f64().unwrap_or(0.0) as f32, PURPLE,
                                      format!("{:.1} / {} GiB", pc["ramUsed"].as_f64().unwrap_or(0.0), pc["ramTotal"].as_f64().unwrap_or(0.0).round()))];
                if pc["gpu"].is_object() {
                    rings.push(("GPU", pc["gpu"]["gpu"].as_f64().unwrap_or(0.0) as f32, GREEN, format!("{} °C", pc["gpu"]["temp"].as_f64().unwrap_or(0.0).round())));
                }
                let n = rings.len() as f32;
                let rad = h * 0.16;
                let gap = r.width() * 0.06;
                let x0 = c.x - (n * rad * 2.0 + (n - 1.0) * gap) / 2.0 + rad;
                for (i, (lbl, v, col, det)) in rings.iter().enumerate() {
                    let cc = Pos2::new(x0 + i as f32 * (rad * 2.0 + gap), c.y + h * 0.02);
                    p.arc(cc, rad, *v, 7.0, *col);
                    p.text(cc - egui::vec2(0.0, h * 0.03), Align2::CENTER_CENTER, format!("{}%", (v * 100.0).round()), h * 0.075, FG);
                    p.text(cc + egui::vec2(0.0, h * 0.045), Align2::CENTER_CENTER, *lbl, h * 0.03, MUTED);
                    p.text(cc + egui::vec2(0.0, h * 0.09), Align2::CENTER_CENTER, det, h * 0.03, DIM);
                }
            }
            Scene::Claude => {
                // destello de rayos que respiran y giran (firma)
                let cc = c - egui::vec2(0.0, h * 0.08);
                let rot = t * 0.1;
                for i in 0..12 {
                    let a = i as f32 / 12.0 * std::f32::consts::TAU + rot + (i % 3) as f32 * 0.05;
                    let base = h * (0.11 + ((i * 37) % 5) as f32 * 0.013);
                    let len = base * (0.82 + 0.18 * (t * (0.6 + (i % 4) as f32 * 0.15) * 1.6 + i as f32 * 1.7).sin());
                    let (s, co) = a.sin_cos();
                    let p0 = cc + egui::vec2(co, s) * h * 0.02;
                    let p1 = cc + egui::vec2(co, s) * len;
                    p.p.line_segment([p0, p1], Stroke::new(if i % 2 == 1 { 8.0 } else { 11.0 }, p.c(CORAL)));
                }
                p.text(c + egui::vec2(0.0, h * 0.18), Align2::CENTER_CENTER, "Claude", h * 0.09, Color32::from_rgb(0xf0, 0xd6, 0xcc));
                p.text(c + egui::vec2(0.0, h * 0.27), Align2::CENTER_CENTER, "hecho contigo · noviembre 2026", h * 0.034, MUTED);
            }
        }
        let _ = RED;
    }
}

fn ease(k: f32) -> f32 { let k = k.clamp(0.0, 1.0); k * k * (3.0 - 2.0 * k) }

fn weekday_of(date: &str) -> usize {
    // "AAAA-MM-DD" → día de la semana (0 = domingo), algoritmo de Sakamoto
    let p: Vec<i64> = date.split('-').filter_map(|x| x.parse().ok()).collect();
    if p.len() != 3 { return 0; }
    let t = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = p[0];
    if p[1] < 3 { y -= 1; }
    ((y + y / 4 - y / 100 + y / 400 + t[(p[1] - 1) as usize] + p[2]) % 7) as usize
}

fn when(e: &Value) -> String {
    let s = e["in"].as_i64().unwrap_or(0);
    if s < 3600 { return format!("En {} min", (s / 60).max(1)); }
    if s < 6 * 3600 { let (h, m) = (s / 3600, (s % 3600) / 60); return if m > 0 { format!("En {h} h {m} min") } else { format!("En {h} h") }; }
    cap(&e["date"].as_str().map(|d| DAYS[weekday_of(d)]).unwrap_or("").to_string())
}
