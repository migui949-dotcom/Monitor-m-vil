# Monitor móvil — pantalla-host

Programa del ordenador para la app **Pantalla** del móvil: el móvil se convierte en un monitor más
(escritorio extendido), con ratón/teclado, sonido, micrófono y almacenamiento. Un solo binario, sin
dependencias que instalar.

| | Linux (Hyprland) | Windows |
|---|---|---|
| Monitor extra por wifi o cable USB | ✅ | ✅ con driver de pantalla virtual; si no, duplica |
| Ratón y teclado | ✅ | ✅ |
| Sonido del ordenador en el móvil | ✅ | ✅ |
| Micrófono del móvil en el ordenador | ✅ | — |
| Almacén del móvil en `~/Móvil` | ✅ | — |
| Cámara del móvil | en desarrollo | en desarrollo |

La conexión va cifrada (Noise XX) y cada móvil necesita permiso en el ordenador la primera vez.

## Instalar

**Linux** (Hyprland): descarga `dist/pantalla-host-linux-x86_64` y ejecuta

```sh
chmod +x pantalla-host-linux-x86_64 && ./pantalla-host-linux-x86_64 instalar
```

Se copia a `~/.local/bin` y crea un servicio de usuario (sin sudo). Usa `pactl`/`parec` para el sonido y
`fusermount3` para el almacén (vienen en casi cualquier escritorio). Por cable hace falta `adb`.

**Windows**: descarga `dist/pantalla-host-windows-x86_64.exe` y ábrelo con doble clic. Se instala para
tu usuario (sin administrador) y arranca con la sesión. Para escritorio extendido instala un driver de
pantalla virtual (p. ej. *Virtual Display Driver*); sin él, el móvil duplica la pantalla principal.

Desinstalar: `pantalla-host desinstalar`. Ver estado: `pantalla-host info`.

## Compilar

```sh
cargo build --release                                                  # Linux
cargo build --release --target x86_64-pc-windows-gnu --bin pantalla-host  # Windows (g++-mingw-w64-x86-64)
```

Estado y siguientes pasos: [PLAN.md](PLAN.md).

## Licencia

[PolyForm Noncommercial 1.0.0](LICENSE): puedes ver, usar, modificar y compartir el código para fines
no comerciales. El uso comercial no está permitido sin permiso del autor.

El vídeo usa [OpenH264](https://github.com/cisco/openh264) (BSD), compilado desde el código fuente.
