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

## Windows — probado 2026-11-18 (v0.6.0)
Máquina de pruebas: Windows 11 25H2 Enterprise de evaluación en QEMU/KVM en el Nobara (`~/vm-windows`:
`arrancar.sh`, instalación desatendida con `autounattend.xml` + `preparar.ps1`, SSH en 127.0.0.1:2222,
47801 → 127.0.0.1:47811, `qmp.py captura|clic|tecla`). Probado con el MI 9 (por cable + túnel) y con
`prueba` + `PRUEBA_GUION` (guion de gestos/texto, capturas de lo que recibe el móvil):
- ✅ doble clic → se instala en `%LOCALAPPDATA%\Pantalla`, inicio de sesión (Run), arranca, aviso «Instalado».
- ✅ permiso (MessageBox; al caducar a los 30 s ahora se cierra sola — antes se quedaba y aceptar no servía).
  Windows no le da el foco del teclado (proceso en segundo plano): hay que hacer clic.
- ✅ pantalla (DXGI, duplicada: sin driver de pantalla virtual), ratón, texto Unicode (ñ, €, emoji), borrar, Intro.
- ✅ sonido (WASAPI loopback, 48 kHz s16 estéreo).
- ✅ gestos: dos dedos mueve (si está maximizada, la restaura bajo el dedo), toque de dos dedos maximiza/restaura,
  pellizco cambia el tamaño con el mismo centro (mensajes `ventana`/`flotar`/`pinch`; la app los usa cuando
  `accepted.platform == "windows"`).
- ✅ teclado automático: UI Automation cada ¼ s (Edit / Document o ComboBox con valor editable, en la pantalla
  del móvil) → `{"t":"teclado"}`.
- Falta: micrófono y cámara del móvil (necesitan driver), almacén (WinFsp), escritorio extendido sin driver IddCx,
  probar con un Windows real (GPU de verdad, varios monitores).
Compilado en el Nobara con zig (sin mingw): ver README.

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

## Webcam siempre visible (2026-11-17, en el sistema de movild)
Chrome/Meet/Discord solo listan una v4l2loopback con exclusive_caps=1 mientras alguien escribe, y la pierden si
cambia el escritor. `webcam.py` (servicio de usuario movil-webcam) es el único escritor de /dev/video42: imagen de
espera a 5 fps o las imágenes que manda media.py por un FIFO, siempre 1280x720 (vertical con bandas).
`activar-webcam.sh` (sudo) instala/configura v4l2loopback (video_nr=42 exclusive_caps=1). Al portar la cámara a
pantalla-host hay que hacer lo mismo.

## Lanzador
`pantalla-host menu` (fuzzel/rofi/wofi/tofi/bemenu/dmenu, MENU= en host.conf) y `instalar --solo-menu`.

## 0.2.0 (2026-11-17)
- Cámara en pantalla-host (`src/devices.rs`): OpenH264 → giro (cam_rot) → 1280x720 → /dev/video42 escrito
  directamente (VIDIOC_S_FMT, sin ffmpeg), imagen de espera 2 fps, encendido/apagado automático al abrir la
  webcam o grabar del micro (fuente "Micrófono del móvil" siempre presente). Probado con el móvil: 30 fps,
  todo el servicio ~17 % CPU.
- Binario Linux con glibc ≥ 2.31 (cargo-zigbuild). `preparar-sistema` (pkexec) desde `instalar`.

## Siguiente: cámara (antiguo plan, ya hecho en Linux)
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
