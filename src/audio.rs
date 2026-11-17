//! Linux: altavoces del móvil (canal 2) y micrófono del móvil (canal 3) con PipeWire/PulseAudio.
//! Solo usa `pactl` y `parec` (vienen con pipewire-pulse / pulseaudio-utils).
//!
//! · Crea la salida "Altavoces del móvil" (null-sink) y manda su monitor al móvil: PCM 48 kHz,
//!   s16le, estéreo, bloques de 10 ms (el formato que reproduce la app).
//! · Las ventanas que están en el monitor MOVIL suenan en el móvil; al volver al PC, a la salida normal.
//! · Micrófono: fuente "Micrófono del móvil" (module-pipe-source) alimentada con lo que llega por el canal 3.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;

const SINK: &str = "movil_altavoces";
const MIC: &str = "movil_microfono";
const MONITOR: &str = "MOVIL";
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
    if !cur.is_empty() && cur != SINK {
        let _ = std::fs::write(&file, &cur);
        return;
    }
    if cur != SINK {
        return;
    }
    let sinks: Vec<String> = json(&["pactl", "-f", "json", "list", "sinks"]).iter()
        .filter_map(|s| s["name"].as_str().map(String::from)).filter(|n| n != SINK).collect();
    let saved = std::fs::read_to_string(&file).ok().map(|s| s.trim().to_string()).filter(|s| sinks.contains(s));
    if let Some(normal) = saved.or_else(|| sinks.first().cloned()) {
        sh(&["pactl", "set-default-sink", &normal]);
        crate::host::log(&format!("sonido: la salida por defecto era el móvil; vuelve a {normal}"));
    }
}

pub struct Speakers {
    module: Option<u32>,
    task: tokio::task::JoinHandle<()>,
}

impl Speakers {
    pub fn start(out: UnboundedSender<(u8, Vec<u8>)>) -> Option<Self> {
        if !available() {
            crate::host::log("sonido: faltan pactl/parec");
            return None;
        }
        let module = find_module(&format!("sink_name={SINK}")).or_else(|| {
            load_module(&["module-null-sink", &format!("sink_name={SINK}"), "sink_properties=\"device.description=\\\"Altavoces del móvil\\\"\""])
        });
        // WirePlumber puede ponerla por defecto al aparecer (si la recordaba): se corrige
        std::thread::sleep(std::time::Duration::from_millis(700));
        guard_default();
        let task = tokio::spawn(async move {
            let cap = tokio::spawn(capture(out));
            router().await;
            cap.abort();
        });
        Some(Speakers { module, task })
    }
}

impl Drop for Speakers {
    fn drop(&mut self) {
        self.task.abort();
        guard_default();
        // devuelve a la salida normal lo que sonaba en el móvil y quita la salida
        let sink_idx: HashSet<u64> = json(&["pactl", "-f", "json", "list", "sinks"])
            .iter()
            .filter(|s| s["name"] == SINK)
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

async fn capture(out: UnboundedSender<(u8, Vec<u8>)>) {
    loop {
        let child = tokio::process::Command::new("parec")
            .args(["-d", &format!("{SINK}.monitor"), "--format=s16le", "--rate=48000", "--channels=2", "--latency-msec=10", "--raw"])
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

/// Cada 1,5 s: el sonido de las ventanas del monitor del móvil va al móvil.
async fn router() {
    let moved = Arc::new(Mutex::new(HashSet::<u64>::new()));
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let moved = moved.clone();
        let _ = tokio::task::spawn_blocking(move || route_once(&mut moved.lock().unwrap())).await;
    }
}

fn route_once(moved: &mut HashSet<u64>) {
    let mon_id = json(&["hyprctl", "monitors", "-j"]).iter().find(|m| m["name"] == MONITOR).and_then(|m| m["id"].as_i64());
    let wins: Vec<Value> = json(&["hyprctl", "clients", "-j"]).into_iter().filter(|c| c["pid"].as_i64().unwrap_or(0) > 0).collect();
    let pids: HashSet<u64> = wins.iter().filter(|c| mon_id.is_some() && c["monitor"].as_i64() == mon_id).filter_map(|c| c["pid"].as_u64()).collect();
    let all: HashSet<u64> = wins.iter().filter_map(|c| c["pid"].as_u64()).collect();
    let elsewhere: HashSet<u64> = all.difference(&pids).copied().collect();
    let Some(target) = json(&["pactl", "-f", "json", "list", "sinks"]).iter().find(|s| s["name"] == SINK).and_then(|s| s["index"].as_u64()) else { return };
    let cpid: HashMap<u64, u64> = json(&["pactl", "-f", "json", "list", "clients"])
        .iter()
        .filter_map(|c| Some((c["index"].as_u64()?, c["properties"]["application.process.id"].as_str()?.parse().ok()?)))
        .collect();
    let inputs = json(&["pactl", "-f", "json", "list", "sink-inputs"]);
    for si in &inputs {
        let (Some(idx), Some(sink)) = (si["index"].as_u64(), si["sink"].as_u64()) else { continue };
        let pid = si["properties"]["application.process.id"].as_str().and_then(|p| p.parse().ok()).or_else(|| si["client"].as_u64().and_then(|c| cpid.get(&c).copied())).unwrap_or(0);
        let anc = if pid > 0 { ancestors(pid) } else { vec![] };
        let on_phone = anc.iter().any(|p| pids.contains(p));
        let on_pc = !on_phone && anc.iter().any(|p| elsewhere.contains(p));
        if on_phone && sink != target {
            sh(&["pactl", "move-sink-input", &idx.to_string(), SINK]);
            moved.insert(idx);
        } else if sink == target && (on_pc || (moved.contains(&idx) && !on_phone)) {
            sh(&["pactl", "move-sink-input", &idx.to_string(), "@DEFAULT_SINK@"]);
            moved.remove(&idx);
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

impl Mic {
    pub fn start() -> Option<Self> {
        if !available() {
            return None;
        }
        let path = format!("{}/movil-mic.fifo", std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()));
        // la fuente la crea devices al arrancar y se queda siempre (para que Meet/Discord la vean):
        // si ya está no es "nuestra" y no se quita al apagar el micro
        let module = if find_module(&format!("source_name={MIC}")).is_some() { None } else { load_module(&[
                "module-pipe-source",
                &format!("source_name={MIC}"),
                &format!("file={path}"),
                "format=s16le",
                "rate=48000",
                "channels=1",
                "source_properties=\"device.description=\\\"Micrófono del móvil\\\"\"",
            ]) };
        let fifo = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&path).ok()
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

/// La fuente "Micrófono del móvil" siempre presente (sin móvil da silencio)
pub fn ensure_mic_source() {
    if !available() || find_module(&format!("source_name={MIC}")).is_some() {
        return;
    }
    let path = format!("{}/movil-mic.fifo", std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()));
    load_module(&[
        "module-pipe-source",
        &format!("source_name={MIC}"),
        &format!("file={path}"),
        "format=s16le",
        "rate=48000",
        "channels=1",
        "source_properties=\"device.description=\\\"Micrófono del móvil\\\"\"",
    ]);
}

/// ¿Alguna app graba del micrófono del móvil? (sin contar medidores de nivel)
pub fn mic_in_use() -> bool {
    if !available() {
        return false;
    }
    let srcs: HashSet<u64> = json(&["pactl", "-f", "json", "list", "sources"]).iter()
        .filter(|s| s["name"] == MIC).filter_map(|s| s["index"].as_u64()).collect();
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
    for name in [format!("sink_name={SINK}"), format!("source_name={MIC}")] {
        while let Some(m) = find_module(&name) {
            sh(&["pactl", "unload-module", &m.to_string()]);
        }
    }
}
