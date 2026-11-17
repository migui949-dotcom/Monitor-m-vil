//! Pedir permiso sin Quickshell: notificación con botones Aceptar/Rechazar.

#[cfg(unix)]
pub async fn ask(device: &str, fp: &str, via: &str, timeout: u64) -> bool {
    let child = tokio::process::Command::new("notify-send")
        .args(["-a", "Móvil", "-u", "critical", "-i", "smartphone", "-t", &(timeout * 1000).to_string()])
        .args(["-A", "aceptar=Aceptar", "-A", "rechazar=Rechazar"])
        .arg(format!("{device} quiere conectarse"))
        .arg(format!("Por {via} · huella {fp}"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(timeout + 5), child).await {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).trim() == "aceptar",
        _ => false,
    }
}

#[cfg(windows)]
pub async fn ask(device: &str, fp: &str, via: &str, timeout: u64) -> bool {
    crate::windows::ask_permission(device, fp, via, timeout).await
}
