//! `pantalla-host menu`: las órdenes del móvil en el lanzador (fuzzel, rofi, wofi, tofi, bemenu, dmenu).
//! El lanzador de aplicaciones enseña "Escritorio"; al abrirlo sale este menú con lo que se puede hacer
//! en ese momento. Usa las mismas URL locales que la barra y el panel (vale con movild y con pantalla-host).

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

const ANSWER: u16 = 47802;
const MEDIA: u16 = 6083;

fn get(port: u16, path: &str) -> Option<String> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut r = String::new();
    s.read_to_string(&mut r).ok()?;
    Some(r.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

/// El lanzador para elegir: MENU= en host.conf, o el primero que haya
fn launcher() -> Option<Vec<String>> {
    let has = |c: &str| {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(c).is_file()))
            .unwrap_or(false)
    };
    if let Some(m) = crate::util::conf("MENU") {
        return Some(m.split_whitespace().map(String::from).collect());
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let opts: &[(&str, &[&str])] = &[
        ("fuzzel", &["--dmenu", "--prompt", "Escritorio › "]),
        ("rofi", &["-dmenu", "-i", "-p", "Escritorio"]),
        ("wofi", &["--dmenu", "--insensitive", "--prompt", "Escritorio"]),
        ("tofi", &["--prompt-text", "Escritorio: "]),
        ("bemenu", &["-i", "-p", "Escritorio"]),
        ("dmenu", &["-i", "-p", "Escritorio"]),
    ];
    opts.iter()
        .filter(|(c, _)| wayland || !matches!(*c, "fuzzel" | "wofi" | "tofi"))
        .find(|(c, _)| has(c))
        .map(|(c, a)| std::iter::once(c.to_string()).chain(a.iter().map(|x| x.to_string())).collect())
}

fn choose(items: &[String]) -> Option<String> {
    let cmd = launcher()?;
    let mut p = Command::new(&cmd[0]).args(&cmd[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    p.stdin.take()?.write_all(items.join("\n").as_bytes()).ok()?;
    let out = p.wait_with_output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn run() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    crate::linux::ensure_env();
    let st: Value = std::fs::read_to_string(crate::host::status_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let md: Value = get(MEDIA, "/state").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let connected = st["connected"].as_bool().unwrap_or(false);
    let (cam, mic) = (md["cam"].as_bool().unwrap_or(false), md["mic"].as_bool().unwrap_or(false));
    let sb = st["standby"].as_bool().unwrap_or(false);

    // (texto, acción)
    let mut items: Vec<(String, Box<dyn Fn()>)> = vec![];
    if let Some(p) = st.get("pending").filter(|p| !p.is_null()) {
        let id = p["id"].as_str().unwrap_or("").to_string();
        let dev = p["device"].as_str().unwrap_or("el móvil").to_string();
        let fp = p["fp"].as_str().unwrap_or("").to_string();
        let id2 = id.clone();
        items.push((format!("✔ Aceptar {dev} (huella {fp})"), Box::new(move || { get(ANSWER, &format!("/answer?id={id}&ok=1")); })));
        items.push((format!("✘ Rechazar {dev}"), Box::new(move || { get(ANSWER, &format!("/answer?id={id2}&ok=0")); })));
    }
    if connected {
        let dev = st["peer"]["device"].as_str().unwrap_or("móvil").to_string();
        items.push((if cam { "Cámara: apagar" } else { "Cámara: encender" }.into(),
                    Box::new(move || { get(MEDIA, &format!("/set?cam={}", if cam { 0 } else { 1 })); })));
        items.push(("Cámara: cambiar delantera / trasera".into(), Box::new(|| { get(MEDIA, "/set?facing=flip"); })));
        items.push((if mic { "Micrófono: apagar" } else { "Micrófono: encender" }.into(),
                    Box::new(move || { get(MEDIA, &format!("/set?mic={}", if mic { 0 } else { 1 })); })));
        for (t, v) in [("100 %", "1"), ("75 %", "0.75"), ("50 %", "0.5"), ("25 %", "0.25"), ("mínimo", "0.01"), ("automático", "-1")] {
            items.push((format!("Brillo del móvil: {t}"), Box::new(move || { get(ANSWER, &format!("/brightness?v={v}")); })));
        }
        items.push(("Pantalla del móvil: apagar".into(), Box::new(|| { get(ANSWER, "/screen?on=0"); })));
        items.push(("Pantalla del móvil: encender".into(), Box::new(|| { get(ANSWER, "/screen?on=1"); })));
        items.push((if sb { "Stand-by: quitar" } else { "Stand-by: poner" }.into(),
                    Box::new(move || { get(ANSWER, &format!("/standby?on={}", if sb { 0 } else { 1 })); })));
        items.push((format!("Desconectar {dev}"), Box::new(|| { get(ANSWER, "/disconnect"); })));
    } else if items.is_empty() {
        items.push(("Sin móvil conectado: abre la app Escritorio en el móvil".into(), Box::new(|| {})));
    }
    items.push(("Abrir el panel".into(), Box::new(|| {
        if let Ok(me) = std::env::current_exe() {
            let _ = Command::new(me).arg("panel").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
    })));

    let labels: Vec<String> = items.iter().map(|(t, _)| t.clone()).collect();
    let Some(pick) = (match launcher() {
        Some(_) => choose(&labels),
        None => {
            // sin lanzador: el panel
            (items.last().unwrap().1)();
            return Ok(());
        }
    }) else {
        return Ok(());
    };
    if let Some((_, f)) = items.iter().find(|(t, _)| *t == pick) {
        f();
    }
    Ok(())
}
