//! ~/Móvil: el espacio que el móvil reserva para este equipo, montado con FUSE sobre la sesión cifrada.
//! Mismo protocolo que movilfs.py (canal 5):
//!   petición  = u32 BE longitud JSON | JSON {"id","op","path",…} | datos
//!   respuesta = u32 BE longitud JSON | JSON {"id","ok","err"?,…}  | datos
//! Sin libfuse: el montaje lo hace `fusermount3` (viene con fuse3, que ya usan Flatpak/AppImage).

use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyWrite, Request, TimeOrNow};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedSender;

const CHUNK: usize = 56 * 1024;
const TTL: Duration = Duration::from_secs(1);

/// Carpeta del almacén de la ranura `slot`: ~/Móvil para el 1º y "~/Móvil (<nombre del móvil>)"
/// para los demás (si ya hay otro con ese nombre montado, se añade el número de ranura)
pub fn mount_point(slot: u8, device: &str) -> PathBuf {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    if slot <= 1 {
        return home.join("Móvil");
    }
    let clean: String = device.chars().map(|c| if c == '/' || c.is_control() { '-' } else { c }).collect();
    let p = home.join(format!("Móvil ({})", clean.trim()));
    if mounted().contains(&p) {
        return home.join(format!("Móvil ({} {slot})", clean.trim()));
    }
    p
}

/// Carpetas montadas ahora mismo por este programa (sistema de archivos "movil")
fn mounted() -> Vec<PathBuf> {
    std::fs::read_to_string("/proc/self/mounts").unwrap_or_default().lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            (f.len() > 2 && f[0] == "movil" && f[2].starts_with("fuse")).then(|| PathBuf::from(unescape(f[1])))
        })
        .collect()
}

/// /proc/mounts escribe espacios y similares como \040
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lado de la sesión: manda peticiones al móvil y empareja respuestas por id.
pub struct Remote {
    out: UnboundedSender<(u8, Vec<u8>)>,
    ids: AtomicU64,
    waiting: Mutex<HashMap<u64, SyncSender<(Value, Vec<u8>)>>>,
}

impl Remote {
    pub fn new(out: UnboundedSender<(u8, Vec<u8>)>) -> Arc<Self> {
        Arc::new(Remote { out, ids: AtomicU64::new(1), waiting: Mutex::new(HashMap::new()) })
    }

    /// Respuesta del móvil (canal 5).
    pub fn on_message(&self, body: &[u8]) {
        if body.len() < 4 {
            return;
        }
        let n = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        if body.len() < 4 + n {
            return;
        }
        let Ok(rep) = serde_json::from_slice::<Value>(&body[4..4 + n]) else { return };
        if let Some(id) = rep["id"].as_u64() {
            if let Some(tx) = self.waiting.lock().unwrap().remove(&id) {
                let _ = tx.try_send((rep, body[4 + n..].to_vec()));
            }
        }
    }

    /// Bloquea (hilo de FUSE) hasta la respuesta; Err = errno.
    fn call(&self, mut req: Value, data: &[u8]) -> Result<(Value, Vec<u8>), i32> {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        req["id"] = json!(id);
        let (tx, rx) = sync_channel(1);
        self.waiting.lock().unwrap().insert(id, tx);
        let j = req.to_string().into_bytes();
        let mut m = Vec::with_capacity(4 + j.len() + data.len());
        m.extend_from_slice(&(j.len() as u32).to_be_bytes());
        m.extend_from_slice(&j);
        m.extend_from_slice(data);
        if self.out.send((5, m)).is_err() {
            self.waiting.lock().unwrap().remove(&id);
            return Err(libc::EIO);
        }
        let r = rx.recv_timeout(Duration::from_secs(25));
        self.waiting.lock().unwrap().remove(&id);
        let (rep, out) = r.map_err(|_| libc::EIO)?;
        if rep["ok"].as_bool() != Some(true) {
            let e = rep["err"].as_str().unwrap_or("EIO");
            return Err(match e.split(':').next().unwrap_or("") {
                "ENOENT" => libc::ENOENT,
                "EEXIST" => libc::EEXIST,
                "EACCES" => libc::EACCES,
                "ENOTEMPTY" => libc::ENOTEMPTY,
                "ENOSPC" => libc::ENOSPC,
                _ => libc::EIO,
            });
        }
        Ok((rep, out))
    }

    /// Varias peticiones seguidas sin esperar cada respuesta (escrituras grandes).
    fn call_many(&self, reqs: Vec<(Value, Vec<u8>)>) -> Result<Vec<Vec<u8>>, i32> {
        let mut rxs = vec![];
        for (mut req, data) in reqs {
            let id = self.ids.fetch_add(1, Ordering::Relaxed);
            req["id"] = json!(id);
            let (tx, rx) = sync_channel(1);
            self.waiting.lock().unwrap().insert(id, tx);
            let j = req.to_string().into_bytes();
            let mut m = Vec::with_capacity(4 + j.len() + data.len());
            m.extend_from_slice(&(j.len() as u32).to_be_bytes());
            m.extend_from_slice(&j);
            m.extend_from_slice(&data);
            if self.out.send((5, m)).is_err() {
                return Err(libc::EIO);
            }
            rxs.push((id, rx));
        }
        let mut res = Ok(vec![]);
        for (id, rx) in rxs {
            let r = rx.recv_timeout(Duration::from_secs(25));
            self.waiting.lock().unwrap().remove(&id);
            match r {
                Ok((rep, d)) if rep["ok"].as_bool() == Some(true) => {
                    if let Ok(v) = &mut res {
                        v.push(d);
                    }
                }
                Ok((rep, _)) if rep["err"].as_str().unwrap_or("").starts_with("ENOSPC") => res = Err(libc::ENOSPC),
                Ok((rep, _)) if rep["err"].as_str().unwrap_or("").starts_with("ENOENT") => res = Err(libc::ENOENT),
                _ => res = Err(libc::EIO),
            }
        }
        res
    }

    fn op(&self, op: &str, path: &str, extra: Value, data: &[u8]) -> Result<(Value, Vec<u8>), i32> {
        let mut req = json!({"op": op, "path": path});
        if let (Some(o), Some(e)) = (req.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        self.call(req, data)
    }
}

/// Los inodos se asignan por ruta (el móvil solo entiende rutas).
struct MovilFs {
    r: Arc<Remote>,
    paths: HashMap<u64, String>,
    inos: HashMap<String, u64>,
    next: u64,
    uid: u32,
    gid: u32,
    /// escritura pendiente (contigua): ruta, desplazamiento, datos
    pending: Option<(String, usize, Vec<u8>)>,
    /// lectura adelantada: ruta, desplazamiento, datos
    cache: Option<(String, usize, Vec<u8>)>,
}

const READ_AHEAD: usize = 1024 * 1024;

const PENDING_MAX: usize = 4 * 1024 * 1024;

impl MovilFs {
    fn ino(&mut self, path: &str) -> u64 {
        if let Some(i) = self.inos.get(path) {
            return *i;
        }
        let i = self.next;
        self.next += 1;
        self.inos.insert(path.to_string(), i);
        self.paths.insert(i, path.to_string());
        i
    }

    fn path(&self, ino: u64) -> Option<String> {
        self.paths.get(&ino).cloned()
    }

    fn child(&self, parent: u64, name: &OsStr) -> Option<String> {
        let p = self.path(parent)?;
        let n = name.to_str()?;
        Some(if p == "/" { format!("/{n}") } else { format!("{p}/{n}") })
    }

    fn attr(&mut self, path: &str, a: &Value) -> FileAttr {
        let dir = a["dir"].as_bool().unwrap_or(false);
        let size = a["size"].as_u64().unwrap_or(0);
        let t = UNIX_EPOCH + Duration::from_secs_f64(a["mtime"].as_f64().unwrap_or(0.0).max(0.0));
        FileAttr {
            ino: self.ino(path),
            size,
            blocks: size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if dir { FileType::Directory } else { FileType::RegularFile },
            perm: if dir { 0o755 } else { 0o644 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Manda la escritura pendiente (en trozos, todos seguidos).
    fn sync(&mut self) -> Result<(), i32> {
        let Some((path, off, data)) = self.pending.take() else { return Ok(()) };
        let reqs = data
            .chunks(CHUNK)
            .enumerate()
            .map(|(i, c)| (json!({"op": "write", "path": path, "off": off + i * CHUNK}), c.to_vec()))
            .collect();
        self.cache = None;
        self.r.call_many(reqs).map(|_| ())
    }

    fn getattr_path(&mut self, path: &str) -> Result<FileAttr, i32> {
        self.sync()?;
        let (rep, _) = self.r.op("getattr", path, json!({}), &[])?;
        Ok(self.attr(path, &rep))
    }

    fn rename_tree(&mut self, old: &str, new: &str) {
        let moved: Vec<(String, u64)> = self.inos.iter().filter(|(p, _)| *p == old || p.starts_with(&format!("{old}/"))).map(|(p, i)| (p.clone(), *i)).collect();
        for (p, i) in moved {
            let np = format!("{new}{}", &p[old.len()..]);
            self.inos.remove(&p);
            self.inos.insert(np.clone(), i);
            self.paths.insert(i, np);
        }
    }
}

impl Filesystem for MovilFs {
    fn lookup(&mut self, _r: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(p) = self.child(parent, name) else { return reply.error(libc::ENOENT) };
        match self.getattr_path(&p) {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&mut self, _r: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let Some(p) = self.path(ino) else { return reply.error(libc::ENOENT) };
        match self.getattr_path(&p) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &mut self, _r: &Request<'_>, ino: u64, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>,
        _atime: Option<TimeOrNow>, _mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<u64>,
        _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<u32>, reply: ReplyAttr,
    ) {
        self.cache = None;
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.path(ino) else { return reply.error(libc::ENOENT) };
        if let Some(sz) = size {
            if let Err(e) = self.r.op("truncate", &p, json!({"size": sz}), &[]) {
                return reply.error(e);
            }
        }
        match self.getattr_path(&p) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&mut self, _r: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.path(ino) else { return reply.error(libc::ENOENT) };
        let rep = match self.r.op("readdir", &p, json!({}), &[]) {
            Ok((rep, _)) => rep,
            Err(e) => return reply.error(e),
        };
        let mut items: Vec<(u64, FileType, String)> = vec![(ino, FileType::Directory, ".".into()), (1, FileType::Directory, "..".into())];
        for e in rep["entries"].as_array().cloned().unwrap_or_default() {
            let Some(name) = e["name"].as_str() else { continue };
            let cp = if p == "/" { format!("/{name}") } else { format!("{p}/{name}") };
            let kind = if e["dir"].as_bool().unwrap_or(false) { FileType::Directory } else { FileType::RegularFile };
            items.push((self.ino(&cp), kind, name.to_string()));
        }
        for (i, (ino, kind, name)) in items.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(&mut self, _r: &Request<'_>, ino: u64, _fh: u64, offset: i64, size: u32, _f: i32, _l: Option<u64>, reply: ReplyData) {
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.path(ino) else { return reply.error(libc::ENOENT) };
        let (off, size) = (offset as usize, size as usize);
        // ¿está en lo leído por adelantado?
        if let Some((cp, co, cd)) = &self.cache {
            if *cp == p && off >= *co && off + size <= co + cd.len() {
                return reply.data(&cd[off - co..off - co + size]);
            }
        }
        // se pide un bloque grande (como mínimo lo que hace falta) en trozos, todos a la vez
        let want = size.max(READ_AHEAD);
        let reqs = (0..want.div_ceil(CHUNK))
            .map(|i| (json!({"op": "read", "path": p, "off": off + i * CHUNK, "len": CHUNK.min(want - i * CHUNK)}), vec![]))
            .collect();
        match self.r.call_many(reqs) {
            Ok(parts) => {
                let mut data = Vec::with_capacity(want);
                for d in parts {
                    let short = d.len() < CHUNK;
                    data.extend_from_slice(&d);
                    if short {
                        break; // fin del archivo
                    }
                }
                let end = size.min(data.len());
                reply.data(&data[..end]);
                self.cache = Some((p, off, data));
            }
            Err(e) => reply.error(e),
        }
    }

    fn write(&mut self, _r: &Request<'_>, ino: u64, _fh: u64, offset: i64, data: &[u8], _wf: u32, _f: i32, _l: Option<u64>, reply: ReplyWrite) {
        let Some(p) = self.path(ino) else { return reply.error(libc::ENOENT) };
        self.cache = None;
        let off = offset as usize;
        let contiguous = matches!(&self.pending, Some((pp, o, d)) if *pp == p && o + d.len() == off);
        if !contiguous {
            if let Err(e) = self.sync() {
                return reply.error(e);
            }
            self.pending = Some((p, off, Vec::with_capacity(PENDING_MAX)));
        }
        let full = {
            let buf = &mut self.pending.as_mut().unwrap().2;
            buf.extend_from_slice(data);
            buf.len() >= PENDING_MAX
        };
        if full {
            if let Err(e) = self.sync() {
                return reply.error(e);
            }
        }
        reply.written(data.len() as u32);
    }

    fn create(&mut self, _r: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        self.cache = None;
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.child(parent, name) else { return reply.error(libc::ENOENT) };
        if let Err(e) = self.r.op("create", &p, json!({}), &[]) {
            return reply.error(e);
        }
        match self.getattr_path(&p) {
            Ok(a) => reply.created(&TTL, &a, 0, 0, 0),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&mut self, _r: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.child(parent, name) else { return reply.error(libc::ENOENT) };
        if let Err(e) = self.r.op("mkdir", &p, json!({}), &[]) {
            return reply.error(e);
        }
        match self.getattr_path(&p) {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _r: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.cache = None;
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.child(parent, name) else { return reply.error(libc::ENOENT) };
        match self.r.op("unlink", &p, json!({}), &[]) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _r: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.cache = None;
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let Some(p) = self.child(parent, name) else { return reply.error(libc::ENOENT) };
        match self.r.op("rmdir", &p, json!({}), &[]) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(&mut self, _r: &Request<'_>, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
        self.cache = None;
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        let (Some(old), Some(new)) = (self.child(parent, name), self.child(newparent, newname)) else { return reply.error(libc::ENOENT) };
        match self.r.op("rename", &old, json!({"to": new}), &[]) {
            Ok(_) => {
                self.rename_tree(&old, &new);
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&mut self, _r: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        if let Err(e) = self.sync() {
            return reply.error(e);
        }
        match self.r.op("statfs", "/", json!({}), &[]) {
            Ok((rep, _)) => {
                let bs = 4096u64;
                let total = rep["total"].as_u64().unwrap_or(0) / bs;
                let free = rep["free"].as_u64().unwrap_or(0) / bs;
                reply.statfs(total, free, free, 1_000_000, 1_000_000, bs as u32, 255, bs as u32);
            }
            Err(e) => reply.error(e),
        }
    }

    fn flush(&mut self, _r: &Request<'_>, _ino: u64, _fh: u64, _l: u64, reply: ReplyEmpty) {
        match self.sync() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&mut self, _r: &Request<'_>, _ino: u64, _fh: u64, _d: bool, reply: ReplyEmpty) {
        match self.sync() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(&mut self, _r: &Request<'_>, _ino: u64, _fh: u64, _f: i32, _l: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        match self.sync() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn destroy(&mut self) {
        let _ = self.sync();
    }
}

pub fn unmount(mp: &std::path::Path) {
    let _ = std::process::Command::new("fusermount3").arg("-uz").arg(mp).stderr(std::process::Stdio::null()).status();
    // las carpetas "~/Móvil (…)" de los demás móviles no se dejan vacías por ahí
    if mp.file_name().map(|n| n.to_string_lossy().starts_with("Móvil (")).unwrap_or(false) {
        let _ = std::fs::remove_dir(mp);
    }
}

/// Al arrancar: desmonta lo que quedara de sesiones anteriores (cierre brusco)
pub fn unmount_all() {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let mut list = mounted();
    if !list.contains(&home.join("Móvil")) {
        list.push(home.join("Móvil")); // montaje roto ("Transport endpoint is not connected") no sale en la lista
    }
    for mp in list {
        unmount(&mp);
    }
}

/// Monta el almacén del móvil en `mp` en segundo plano; se desmonta al soltar el valor devuelto.
pub fn mount(remote: Arc<Remote>, mp: &std::path::Path) -> Result<fuser::BackgroundSession, String> {
    unmount(mp); // por si quedó algo de una sesión anterior
    let mp = mp.to_path_buf();
    std::fs::create_dir_all(&mp).map_err(|e| e.to_string())?;
    let mut fs = MovilFs { r: remote, paths: HashMap::new(), inos: HashMap::new(), next: 2, uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, pending: None, cache: None };
    fs.inos.insert("/".into(), 1);
    fs.paths.insert(1, "/".into());
    fuser::spawn_mount2(fs, &mp, &[MountOption::FSName("movil".into()), MountOption::Subtype("movil".into()), MountOption::DefaultPermissions])
        .map_err(|e| format!("FUSE: {e}"))
}
