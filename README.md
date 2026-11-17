# Escritorio — el móvil como pantalla, webcam y micrófono

**Escritorio** convierte un móvil Android en un monitor más del ordenador (escritorio extendido) y,
además, en su **webcam** y su **micrófono** para Meet, Discord, OBS… Por wifi o por cable, cifrado.

Dos piezas:
- **App del móvil** (`app/`, Android 10+, ARM 64 bits).
- **`pantalla-host`** en el ordenador: un solo programa, sin dependencias que instalar.

| | Linux (Hyprland, KDE Plasma) | Windows |
|---|---|---|
| Monitor extra por wifi o cable USB | ✅ | ✅ con driver de pantalla virtual; si no, duplica |
| Ratón y teclado (cualquier distribución de teclado) | ✅ | ✅ |
| Sonido del ordenador en el móvil | ✅ | ✅ |
| **Micrófono del móvil** — siempre en la lista de micrófonos, se enciende solo al usarlo | ✅ | — |
| **Cámara del móvil** — webcam "Camara del movil", se enciende sola al usarla | ✅ | — |
| Almacén del móvil en `~/Móvil` | ✅ | — |
| Panel "Escritorio" y menú en fuzzel/rofi/wofi | ✅ | — |

## Instalar en el ordenador (Linux)

1. Descarga [`dist/pantalla-host-linux-x86_64`](dist/) (vale para casi cualquier distribución de 2020 en adelante).
2. Ábrelo: doble clic (marca antes "Permitir ejecutar como programa" en sus propiedades) o en una terminal:
   ```sh
   chmod +x pantalla-host-linux-x86_64 && ./pantalla-host-linux-x86_64
   ```
   Se instala solo para tu usuario (servicio que arranca con la sesión, lanzador "Escritorio" y, en
   KDE, el permiso para crear el monitor) y abre el panel. Solo si hace falta, pide **una vez** la
   contraseña de administrador para la webcam virtual y el cortafuegos. No hay que instalar nada más:
   el vídeo se comprime en el propio programa (vale con cualquier gráfica, también NVIDIA).

Y ya está: abre la app en el móvil, elige el ordenador y acepta en el ordenador (panel o barra).

- **Webcam y micrófono**: en Meet/Discord elige "Camara del movil" y "Micrófono del móvil". Se
  encienden solos cuando una app los usa y se apagan 10 s después.
- **Menú rápido**: busca "Escritorio" en tu lanzador (fuzzel, rofi, wofi…): cámara, micro, brillo,
  pantalla del móvil, stand-by, desconectar.
- **Opciones** en `~/.config/movil-pantalla/host.conf`: `POSITION=auto-left|auto-right|…`,
  `PHONE_WS=11` (escritorio del móvil en Hyprland), `PANEL=no`, `MENU=…`.
- Por cable USB hace falta `adb`.

**Windows**: descarga `dist/pantalla-host-windows-x86_64.exe` y ábrelo con doble clic (se instala para tu
usuario). Para escritorio extendido instala un driver de pantalla virtual (p. ej. *Virtual Display
Driver*); sin él, el móvil duplica la pantalla principal.

Desinstalar: `pantalla-host desinstalar`. Estado: `pantalla-host info`.

## Instalar la app en el móvil

Compila `app/` con `app/build.sh` (Rust + NDK, sin Gradle) e instala el APK. Al abrirla por primera vez
pide los permisos (cámara/micrófono, "bloquear internet" con su cortafuegos propio, y que Android no la
cierre).

## Compilar

```sh
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31 --bin pantalla-host  # Linux compatible (glibc ≥ 2.31)
cargo build --release --target x86_64-pc-windows-gnu --bin pantalla-host             # Windows (g++-mingw-w64-x86-64)
```

Estado y siguientes pasos: [PLAN.md](PLAN.md).

## Licencia

[PolyForm Noncommercial 1.0.0](LICENSE): puedes ver, usar, modificar y compartir el código para fines
no comerciales. El uso comercial no está permitido sin permiso del autor.

El vídeo usa [OpenH264](https://github.com/cisco/openh264) (BSD), compilado desde el código fuente.
