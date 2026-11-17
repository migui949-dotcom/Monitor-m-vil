//! Almacenamiento: cada ordenador tiene 10 GB reservados en el móvil, en su propia carpeta
//! (/sdcard/Android/data/org.movilpantalla.app/files/almacen/<id del equipo>). El equipo la monta
//! como ~/Móvil y le pide operaciones por el canal 5:
//!   petición  = u32 BE (longitud del JSON) | JSON {id, op, path, …} | datos (escrituras)
//!   respuesta = u32 BE | JSON {id, ok, err?, …} | datos (lecturas)
//! Las rutas se limpian: no se puede salir de la carpeta del equipo.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::link::Link;

pub const CH_STORE: u8 = 5;
pub const QUOTA: u64 = 10 * 1024 * 1024 * 1024;     // 10 GB por equipo
const MAX_READ: u64 = 56 * 1024;                      // cabe en un mensaje del enlace

pub struct Store {
    root: PathBuf,
    used: u64,
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => total += dir_size(&e.path()),
                Ok(m) => total += m.len(),
                _ => {}
            }
        }
    }
    total
}

fn attrs(m: &fs::Metadata) -> Value {
    let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    json!({"dir": m.is_dir(), "size": m.len(), "mtime": mtime})
}

impl Store {
    pub fn open(base: &Path, host_id: &str) -> Store {
        let safe: String = host_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let root = base.join("almacen").join(if safe.is_empty() { "equipo" } else { &safe });
        let _ = fs::create_dir_all(&root);
        let used = dir_size(&root);
        log::info!("almacén: {} ({:.1} GB usados de 10)", root.display(), used as f64 / 1e9);
        Store { root, used }
    }

    /// Ruta dentro de la carpeta del equipo (sin "..", sin absolutas)
    fn path(&self, p: &str) -> Option<PathBuf> {
        let mut out = self.root.clone();
        for c in Path::new(p.trim_start_matches('/')).components() {
            match c {
                Component::Normal(x) => out.push(x),
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(out)
    }

    fn handle(&mut self, req: &Value, data: &[u8]) -> Result<(Value, Vec<u8>), String> {
        let op = req["op"].as_str().unwrap_or("");
        let p = self.path(req["path"].as_str().unwrap_or("/")).ok_or("ruta no válida")?;
        let io = |e: std::io::Error| match e.kind() {
            std::io::ErrorKind::NotFound => "ENOENT".to_string(),
            std::io::ErrorKind::AlreadyExists => "EEXIST".to_string(),
            std::io::ErrorKind::PermissionDenied => "EACCES".to_string(),
            _ if e.raw_os_error() == Some(39) => "ENOTEMPTY".to_string(),
            _ => format!("EIO:{e}"),
        };
        match op {
            "statfs" => Ok((json!({"total": QUOTA, "free": QUOTA.saturating_sub(self.used)}), vec![])),
            "getattr" => Ok((attrs(&fs::metadata(&p).map_err(io)?), vec![])),
            "readdir" => {
                let mut entries = vec![];
                for e in fs::read_dir(&p).map_err(io)?.flatten() {
                    if let Ok(m) = e.metadata() {
                        let mut a = attrs(&m);
                        a["name"] = json!(e.file_name().to_string_lossy());
                        entries.push(a);
                    }
                }
                Ok((json!({"entries": entries}), vec![]))
            }
            "read" => {
                let mut f = fs::File::open(&p).map_err(io)?;
                f.seek(SeekFrom::Start(req["off"].as_u64().unwrap_or(0))).map_err(io)?;
                let mut buf = vec![0u8; req["len"].as_u64().unwrap_or(0).min(MAX_READ) as usize];
                let n = f.read(&mut buf).map_err(io)?;
                buf.truncate(n);
                Ok((json!({}), buf))
            }
            "write" => {
                let off = req["off"].as_u64().unwrap_or(0);
                let old = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                let new_len = old.max(off + data.len() as u64);
                if self.used + (new_len - old) > QUOTA { return Err("ENOSPC".into()); }
                let mut f = fs::OpenOptions::new().write(true).create(true).truncate(false).open(&p).map_err(io)?;
                f.seek(SeekFrom::Start(off)).map_err(io)?;
                f.write_all(data).map_err(io)?;
                self.used += new_len - old;
                Ok((json!({"n": data.len()}), vec![]))
            }
            "create" => { fs::OpenOptions::new().write(true).create(true).truncate(false).open(&p).map_err(io)?; Ok((json!({}), vec![])) }
            "mkdir" => { fs::create_dir(&p).map_err(io)?; Ok((json!({}), vec![])) }
            "unlink" => {
                let sz = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                fs::remove_file(&p).map_err(io)?;
                self.used = self.used.saturating_sub(sz);
                Ok((json!({}), vec![]))
            }
            "rmdir" => { fs::remove_dir(&p).map_err(io)?; Ok((json!({}), vec![])) }
            "rename" => {
                let to = self.path(req["to"].as_str().unwrap_or("")).ok_or("ruta no válida")?;
                fs::rename(&p, &to).map_err(io)?;
                Ok((json!({}), vec![]))
            }
            "truncate" => {
                let size = req["size"].as_u64().unwrap_or(0);
                let old = fs::metadata(&p).map(|m| m.len()).map_err(io)?;
                if size > old && self.used + (size - old) > QUOTA { return Err("ENOSPC".into()); }
                fs::OpenOptions::new().write(true).open(&p).map_err(io)?.set_len(size).map_err(io)?;
                self.used = (self.used + size).saturating_sub(old);
                Ok((json!({}), vec![]))
            }
            _ => Err(format!("operación desconocida: {op}")),
        }
    }
}

fn frame(v: &Value, data: &[u8]) -> Vec<u8> {
    let j = serde_json::to_vec(v).unwrap();
    let mut out = Vec::with_capacity(4 + j.len() + data.len());
    out.extend_from_slice(&(j.len() as u32).to_be_bytes());
    out.extend_from_slice(&j);
    out.extend_from_slice(data);
    out
}

/// Hilo del almacén: atiende las peticiones del equipo en orden
pub fn serve(link: Arc<Link>, base: PathBuf, host_id: String) -> mpsc::Sender<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut store = Store::open(&base, &host_id);
        while let Ok(msg) = rx.recv() {
            if msg.len() < 4 { continue; }
            let n = u32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
            if msg.len() < 4 + n { continue; }
            let req: Value = serde_json::from_slice(&msg[4..4 + n]).unwrap_or(Value::Null);
            let (mut v, data) = match store.handle(&req, &msg[4 + n..]) {
                Ok((v, d)) => (v, d),
                Err(e) => (json!({"err": e}), vec![]),
            };
            v["id"] = req["id"].clone();
            v["ok"] = json!(v.get("err").is_none());
            if link.is_closed() || link.send(CH_STORE, &frame(&v, &data)).is_err() { break; }
        }
    });
    tx
}
