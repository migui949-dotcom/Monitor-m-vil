# pantalla-host — estado y siguientes pasos

## Hecho (v0.1.0, 2026-11-17)
| | Linux (Hyprland) | Windows |
|---|---|---|
| Descubrimiento, Noise, permiso | ✅ probado con el móvil | ✅ compila (sin probar) |
| Pantalla (RFB + H.264) | ✅ probado | ✅ compila: pantalla virtual si hay driver IddCx, si no duplica |
| Ratón / teclado | ✅ probado | ✅ compila (SendInput) |
| Sonido → móvil (canal 2) | ✅ probado | ✅ compila (WASAPI loopback) |
| Micrófono del móvil (canal 3) | ✅ probado (barra → 6083/set?mic=on → app manda audio) | ❌ (Windows no tiene micro virtual de serie) |
| Cámara del móvil (canal 4) | ❌ | ❌ |
| Almacén ~/Móvil (canal 5) | ✅ probado, 25 MB/s | ❌ (necesitaría WinFsp) |
| Datos del stand-by ("sb") | ❌ | ❌ |

Binarios: `dist/` (Linux x86_64 y Windows x86_64, ver SHA256SUMS).
Compilar: `cargo build --release` y `cargo build --release --target x86_64-pc-windows-gnu --bin pantalla-host`
(necesita `g++-mingw-w64-x86-64`; `.cargo/config.toml` enlaza libstdc++ estática).

## Siguiente: cámara
Protocolo actual (movild.py + media.py):
- Encender/apagar: control `{"t":"media","cam":bool,"mic":bool,"facing":"user"|"environment"}` hacia la app;
  la app responde `{"t":"cam_state","camOk":bool,"error"?}`.
- Vídeo: canal 4, H.264 Annex B (cada mensaje = un fotograma completo, del codificador del móvil).

Plan (sin ffmpeg/mpv/PyAV):
1. Decodificar con `openh264::decoder::Decoder` (ya está en el binario) → I420.
2. Linux, cámara virtual: escribir YUYV/I420 directamente en `/dev/videoN` de **v4l2loopback**
   (`write()` tras `VIDIOC_S_FMT`; no hace falta ffmpeg). v4l2loopback es un módulo del kernel: única
   instalación con sudo, y solo para quien quiera usar el móvil como webcam. Si no está, solo vista previa.
3. Vista previa: ventana Wayland propia (xdg-shell + wl_shm, como la captura), con el botón de cerrar = apagar cámara.
4. Barra: `/set?cam=toggle&facing=flip` en el servidor de 6083 que ya existe (`host.rs: media_http`).
5. Windows: cámara virtual de serie solo en Windows 11 (MFCreateVirtualCamera, necesita una DLL COM de
   media source) → dejar para después; de momento, vista previa en una ventana.

## Siguiente: stand-by
`standby/server.py` da tiempo, recordatorios, música y PC cada 4 s (`{"t":"sb",…}`) cuando la barra pide
`/standby?on=1`. Portar: tiempo (open-meteo por HTTP), música (MPRIS por D-Bus / `playerctl` no está),
PC (CPU/RAM de /proc).

## App (código en el Nobara)
- Mostrar `platform` (linux/windows) y `v` del mensaje `info`.
- Elegir "escritorio nuevo" o "duplicar" (en Windows sin driver virtual solo duplicar).
