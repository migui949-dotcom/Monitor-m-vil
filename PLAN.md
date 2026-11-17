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

## KDE Plasma (Wayland) — 2026-11-17
- `src/kde.rs`: monitor virtual con `zkde_screencast_unstable_v1.stream_virtual_output` (como krfb), colocado con
  `kscreen-doctor` según POSITION; ratón/teclado con `org_kde_kwin_fake_input` (el teclado lee el mapa XKB de KWin
  → vale cualquier distribución; tildes que no estén → tecla muerta + letra).
- `src/pw.rs`: cliente mínimo de PipeWire cargado con dlopen (sin cabeceras ni bindgen; sin PipeWire el binario sigue
  arrancando). Pide BGRx/BGRA en memoria compartida. Test que compara los bytes SPA con los de libspa.
- Permiso: KWin solo da esos protocolos a programas con `.desktop` y `X-KDE-Wayland-Interfaces`; `instalar` lo crea
  en ~/.local/share/applications/pantalla-host.desktop (Exec = ruta del binario).
- Probado con un KWin 6.7.5 de pruebas sin pantalla en el Nobara (`kwin_wayland --virtual`): ~27 fps, ventana abierta
  en el monitor del móvil, permiso por .desktop sí/no. Falta probarlo en una sesión Plasma de verdad con el móvil.
- El ordenador manda `screen_error` en `accepted` y la app lo enseña si no hay escritorio.

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

## App (código en `app/`)
- [x] Icono Linux/Windows en la lista de equipos (campo `platform`).
- [x] Botón de menú siempre visible en el escritorio (Desconectar / Volver).
- [x] Sin conexión automática (el `depuracion.json` del móvil está renombrado a `.desactivado`).
- [ ] Elegir "escritorio nuevo" o "duplicar" (en Windows sin driver virtual solo duplicar): falta en host y app.
- Compilar: en el PC (Arch) `~/.local/share/movil-pantalla/app/build.sh` (compila en el Nobara, firma en el PC).

## Probado en el Nobara (Hyprland 0.57, configuración Lua) — 2026-11-17
Instancia de pruebas + `prueba`: 30 fps, la salida MOVIL se crea y se quita sola. El hostname del Nobara es
"Linux", así que su `host.conf` lleva `NAME=Nobara`.
