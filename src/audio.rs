//! Linux: altavoces del móvil (canal 2) y micrófono del móvil (canal 3) con PipeWire/PulseAudio.
//! Solo usa `pactl` y `parec` (vienen con pipewire-pulse / pulseaudio-utils).
//!
//! · Crea una salida "Altavoces de <móvil>" (null-sink) por móvil y manda su monitor a ese móvil:
//!   PCM 48 kHz, s16le, estéreo, bloques de 10 ms (el formato que reproduce la app).
//! · Las ventanas que están en el monitor de un móvil (MOVIL, MOVIL-2…) suenan en ese móvil; al volver
//!   al PC, a la salida normal. Un solo enrutador para todos los móviles (si no, se pelearían).
//! · Micrófono: fuente "Micrófono del móvil" (module-pipe-source) alimentada con lo que llega por el canal 3;
//!   la del 1º siempre existe, la de los demás ("Micrófono de <móvil>") mientras están conectados.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;

const SINK: &str = "movil_altavoces";
const MIC: &str = "movil_microfono";

/// Salida de sonido de la ranura `s` (movil_altavoces, movil_altavoces_2…)
pub fn sink_name(s: u8) -> String {
    if s <= 1 { SINK.into() } else { format!("{SINK}_{s}") }
}

/// Fuente (micrófono) de la ranura `s` (movil_microfono, movil_microfono_2…)
pub fn mic_name(s: u8) -> String {
    if s <= 1 { MIC.into() } else { format!("{MIC}_{s}") }
}

fn mic_fifo(s: u8) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    if s <= 1 { format!("{dir}/movil-mic.fifo") } else { format!("{dir}/movil-mic-{s}.fifo") }
}

fn is_phone_sink(name: &str) -> bool {
    name == SINK || name.strip_prefix(SINK).and_then(|r| r.strip_prefix('_')).map(|r| r.parse::<u8>().is_ok()).unwrap_or(false)
}

/// Texto para pactl: comillas dobles del nombre fuera (rompen sink_properties)
fn label(device: &str) -> String {
    device.chars().filter(|c| *c != '"' && *c != '\\' && !c.is_control()).collect()
}
const CHUNK: usize = 48000 * 2 * 2 / 100;

fn sh(args: &[&str]) -> String {
    std::process::Command::new(args[0])
        .args(&args[1..])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn json(args: &[&str]) -> Vec<Value> {
    serde_json::from_str::<Value>(&sh(args)).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

pub fn available() -> bool {
    which("pactl") && which("parec")
}

fn which(p: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|d| d.join(p).is_file()))
        .unwrap_or(false)
}

fn load_module(args: &[&str]) -> Option<u32> {
    let mut a = vec!["pactl", "load-module"];
    a.extend_from_slice(args);
    sh(&a).trim().parse().ok()
}

fn find_module(needle: &str) -> Option<u32> {
    sh(&["pactl", "list", "short", "modules"]).lines().find(|l| l.contains(needle)).and_then(|l| l.split_whitespace().next()?.parse().ok())
}

/// Que la salida del móvil no se quede como salida por defecto del sistema.
/// WirePlumber recuerda la salida elegida y la vuelve a poner cada vez que reaparece: si alguna vez se
/// eligió el móvil (o el ecualizador de la barra lo tomó como salida "física"), todo el sonido acababa en
/// el móvil en cada conexión. Se apunta la salida normal y se recupera al empezar y al terminar.
fn guard_default() {
    let file = crate::util::state_dir().join("salida-normal");
    let cur = sh(&["pactl", "get-default-sink"]).trim().to_string();
    if !cur.is_empty() && !is_phone_sink(&cur) {
        let _ = std::fs::write(&file, &cur);
        return;
    }
    if !is_phone_sink(&cur) {
        return;
    }
    let sinks: Vec<String> = json(&["pactl", "-f", "json", "list", "sinks"]).iter()
        .filter_map(|s| s["name"].as_str().map(String::from)).filter(|n| !is_phone_sink(n)).collect();
    let saved = std::fs::read_to_string(&file).ok().map(|s| s.trim().to_string()).filter(|s| sinks.contains(s));
    if let Some(normal) = saved.or_else(|| sinks.first().cloned()) {
        sh(&["pactl", "set-default-sink", &normal]);
        crate::host::log(&format!("sonido: la salida por defecto era el móvil; vuelve a {normal}"));
    }
}

pub struct Speakers {
    slot: u8,
    sink: String,
    module: Option<u32>,
    task: tokio::task::JoinHandle<()>,
}

/// Monitores de móvil con su salida de sonido (lo que usa el enrutador)
static ROUTES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static ROUTER: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

impl Speakers {
    pub fn start(out: UnboundedSender<(u8, Vec<u8>)>, slot: u8, device: &str) -> Option<Self> {
        if !available() {
            crate::host::log("sonido: faltan pactl/parec");
            return None;
        }
        let sink = sink_name(slot);
        let module = module_with(&format!("sink_name={sink}")).or_else(|| {
            load_module(&["module-null-sink", &format!("sink_name={sink}"),
                          &format!("sink_properties=\"device.description=\\\"Altavoces de {}\\\"\"", label(device))])
        });
        // WirePlumber puede ponerla por defecto al aparecer (si la recordaba): se corrige
        std::thread::sleep(std::time::Duration::from_millis(700));
        guard_default();
        let task = tokio::spawn(capture(out, sink.clone()));
        ROUTES.lock().unwrap().push((crate::util::output_name(slot), sink.clone()));
        let mut r = ROUTER.lock().unwrap();
        if r.as_ref().map(|t| t.is_finished()).unwrap_or(true) {
            *r = Some(tokio::spawn(router()));
        }
        Some(Speakers { slot, sink, module, task })
    }
}

impl Drop for Speakers {
    fn drop(&mut self) {
        self.task.abort();
        {
            let mut routes = ROUTES.lock().unwrap();
            let out = crate::util::output_name(self.slot);
            routes.retain(|r| r.0 != out);
            if routes.is_empty() {
                if let Some(t) = ROUTER.lock().unwrap().take() {
                    t.abort();
                }
            }
        }
        guard_default();
        // devuelve a la salida normal lo que sonaba en este móvil y quita la salida
        let sink_idx: HashSet<u64> = json(&["pactl", "-f", "json", "list", "sinks"])
            .iter()
            .filter(|s| s["name"].as_str() == Some(self.sink.as_str()))
            .filter_map(|s| s["index"].as_u64())
            .collect();
        for si in json(&["pactl", "-f", "json", "list", "sink-inputs"]) {
            if si["sink"].as_u64().map(|s| sink_idx.contains(&s)).unwrap_or(false) {
                sh(&["pactl", "move-sink-input", &si["index"].to_string(), "@DEFAULT_SINK@"]);
            }
        }
        if let Some(m) = self.module {
            sh(&["pactl", "unload-module", &m.to_string()]);
        }
    }
}

async fn capture(out: UnboundedSender<(u8, Vec<u8>)>, sink: String) {
    loop {
        let child = tokio::process::Command::new("parec")
            .args(["-d", &format!("{sink}.monitor"), "--format=s16le", "--rate=48000", "--channels=2", "--latency-msec=10", "--raw"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            let mut stdout = child.stdout.take().unwrap();
            let mut buf = vec![0u8; CHUNK];
            let (mut sent, mut silent, mut peak, mut t) = (0u32, 0u32, 0i16, std::time::Instant::now());
            while stdout.read_exact(&mut buf).await.is_ok() {
                if crate::util::debug() {
                    for c in buf.chunks(2) {
                        peak = peak.max(i16::from_le_bytes([c[0], c[1]]).saturating_abs());
                    }
                    if t.elapsed() > Duration::from_secs(5) {
                        crate::dbg_log!("sonido: {sent} bloques enviados, {silent} en silencio, pico {peak}");
                        (sent, silent, peak, t) = (0, 0, 0, std::time::Instant::now());
                    }
                }
                if buf.iter().all(|b| *b == 0) {
                    silent += 1;
                    continue; // silencio absoluto: no se envía
                }
                sent += 1;
                if out.send((2, buf.clone())).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn ancestors(mut pid: u64) -> Vec<u64> {
    let mut v = vec![];
    for _ in 0..8 {
        v.push(pid);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { break };
        let Some(p) = stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1)).and_then(|x| x.parse().ok()) else { break };
        pid = p;
        if pid <= 1 {
            break;
        }
    }
    v
}

/// Cada 1,5 s: el sonido de las ventanas del monitor de cada móvil va a ese móvil.
async fn router() {
    let moved = Arc::new(Mutex::new(HashSet::<u64>::new()));
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let moved = moved.clone();
        let _ = tokio::task::spawn_blocking(move || route_once(&mut moved.lock().unwrap())).await;
    }
}

fn route_once(moved: &mut HashSet<u64>) {
    let routes = ROUTES.lock().unwrap().clone();
    if routes.is_empty() {
        return;
    }
    // id de monitor → salida de sonido del móvil (solo los monitores de móvil)
    let mon_sink: HashMap<i64, String> = json(&["hyprctl", "monitors", "-j"]).iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            let sink = routes.iter().find(|r| r.0 == name)?.1.clone();
            Some((m["id"].as_i64()?, sink))
        })
        .collect();
    let wins: Vec<Value> = json(&["hyprctl", "clients", "-j"]).into_iter().filter(|c| c["pid"].as_i64().unwrap_or(0) > 0).collect();
    // pid → salida del móvil donde tiene ventana (o ninguna = en el PC)
    let mut on_phone: HashMap<u64, String> = HashMap::new();
    let mut on_pc: HashSet<u64> = HashSet::new();
    for c in &wins {
        let Some(pid) = c["pid"].as_u64() else { continue };
        match c["monitor"].as_i64().and_then(|m| mon_sink.get(&m)) {
            Some(sink) => {
                on_phone.entry(pid).or_insert_with(|| sink.clone());
            }
            None => {
                on_pc.insert(pid);
            }
        }
    }
    let sinks = json(&["pactl", "-f", "json", "list", "sinks"]);
    let sink_by_idx: HashMap<u64, String> = sinks.iter().filter_map(|s| Some((s["index"].as_u64()?, s["name"].as_str()?.to_string()))).collect();
    let cpid: HashMap<u64, u64> = json(&["pactl", "-f", "json", "list", "clients"])
        .iter()
        .filter_map(|c| Some((c["index"].as_u64()?, c["properties"]["application.process.id"].as_str()?.parse().ok()?)))
        .collect();
    let inputs = json(&["pactl", "-f", "json", "list", "sink-inputs"]);
    for si in &inputs {
        let (Some(idx), Some(sink)) = (si["index"].as_u64(), si["sink"].as_u64()) else { continue };
        let cur = sink_by_idx.get(&sink).cloned().unwrap_or_default();
        let pid = si["properties"]["application.process.id"].as_str().and_then(|p| p.parse().ok()).or_else(|| si["client"].as_u64().and_then(|c| cpid.get(&c).copied())).unwrap_or(0);
        let anc = if pid > 0 { ancestors(pid) } else { vec![] };
        // el antepasado más cercano con ventana decide
        let target = anc.iter().find_map(|p| on_phone.get(p).cloned());
        let pc = target.is_none() && anc.iter().any(|p| on_pc.contains(p));
        match target {
            Some(t) if t != cur => {
                sh(&["pactl", "move-sink-input", &idx.to_string(), &t]);
                moved.insert(idx);
            }
            None if is_phone_sink(&cur) && (pc || moved.contains(&idx)) => {
                sh(&["pactl", "move-sink-input", &idx.to_string(), "@DEFAULT_SINK@"]);
                moved.remove(&idx);
            }
            _ => {}
        }
    }
    let live: HashSet<u64> = inputs.iter().filter_map(|s| s["index"].as_u64()).collect();
    moved.retain(|i| live.contains(i));
}

// ───────────── Micrófono ─────────────

pub struct Mic {
    module: Option<u32>,
    fifo: Option<std::fs::File>,
}

fn load_mic(slot: u8, description: &str) -> Option<u32> {
    load_module(&[
        "module-pipe-source",
        &format!("source_name={}", mic_name(slot)),
        &format!("file={}", mic_fifo(slot)),
        "format=s16le",
        "rate=48000",
        "channels=1",
        &format!("source_properties=\"device.description=\\\"{}\\\"\"", label(description)),
    ])
}

fn mic_module(slot: u8) -> Option<u32> {
    module_with(&format!("source_name={}", mic_name(slot)))
}

/// Módulo con ese argumento exacto ("source_name=movil_microfono" también está dentro de
/// "source_name=movil_microfono_2": se compara la palabra entera)
fn module_with(arg: &str) -> Option<u32> {
    sh(&["pactl", "list", "short", "modules"]).lines()
        .find(|l| l.split_whitespace().any(|w| w == arg))
        .and_then(|l| l.split_whitespace().next()?.parse().ok())
}

impl Mic {
    pub fn start(slot: u8, device: &str) -> Option<Self> {
        if !available() {
            return None;
        }
        // la fuente del 1º la crea devices al arrancar y se queda siempre (para que Meet/Discord la vean):
        // si ya está no es "nuestra" y no se quita al apagar el micro
        let module = if mic_module(slot).is_some() { None } else { load_mic(slot, &format!("Micrófono de {device}")) };
        let fifo = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(mic_fifo(slot)).ok()
        };
        Some(Mic { module, fifo })
    }

    pub fn write(&mut self, data: &[u8]) {
        if let Some(f) = &mut self.fifo {
            let _ = std::io::Write::write(f, data); // si PipeWire va lento se descarta (sin bloquear)
        }
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.fifo = None;
        if let Some(m) = self.module {
            sh(&["pactl", "unload-module", &m.to_string()]);
        }
    }
}

/// La fuente "Micrófono del móvil" (ranura 1) siempre presente (sin móvil da silencio)
pub fn ensure_mic_source() {
    if !available() || mic_module(1).is_some() {
        return;
    }
    load_mic(1, "Micrófono del móvil");
}

/// Micrófono de la ranura `slot` (≥2) creado ya al conectar ese móvil, para que Meet lo vea en la lista
/// aunque aún no se haya encendido; se quita al desconectar (`remove_mic_source`)
pub fn add_mic_source(slot: u8, device: &str) {
    if slot > 1 && available() && mic_module(slot).is_none() {
        load_mic(slot, &format!("Micrófono de {device}"));
    }
}

pub fn remove_mic_source(slot: u8) {
    if slot > 1 {
        while let Some(m) = mic_module(slot) {
            sh(&["pactl", "unload-module", &m.to_string()]);
        }
    }
}

/// ¿Alguna app graba del micrófono de la ranura `slot`? (sin contar medidores de nivel)
pub fn mic_in_use(slot: u8) -> bool {
    if !available() {
        return false;
    }
    let name = mic_name(slot);
    let srcs: HashSet<u64> = json(&["pactl", "-f", "json", "list", "sources"]).iter()
        .filter(|s| s["name"].as_str() == Some(name.as_str())).filter_map(|s| s["index"].as_u64()).collect();
    !srcs.is_empty() && json(&["pactl", "-f", "json", "list", "source-outputs"]).iter().any(|o| {
        let p = &o["properties"];
        o["source"].as_u64().map(|s| srcs.contains(&s)).unwrap_or(false)
            && p["stream.monitor"].as_str() != Some("true")
            && !p["media.name"].as_str().unwrap_or("").to_lowercase().contains("peak")
    })
}

/// Quita lo que pudiera quedar de una sesión anterior (cierre brusco).
pub fn cleanup() {
    if !available() {
        return;
    }
    // todas las de móviles (movil_altavoces, movil_altavoces_2…, movil_microfono…)
    for name in [format!("sink_name={SINK}"), format!("source_name={MIC}")] {
        while let Some(m) = find_module(&name) {
            sh(&["pactl", "unload-module", &m.to_string()]);
        }
    }
}
