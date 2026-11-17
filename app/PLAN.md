# App nativa "Pantalla" (Mi MIX 2S) — plan

Sustituye a Brave. Núcleo en Rust (NativeActivity + egui/eframe 0.37 + NDK: MediaCodec, AAudio, Camera2).
Build sin Gradle: `./build.sh` compila en el Nobara (ssh `nobara`, ~/movil-build/app, rustup + NDK r29 en su
~/.local/share/android-dev) y empaqueta/firma en el PC (aapt2 → zipalign → apksigner). `LOCAL=1` compila en el PC.

## Flujo (pedido por el usuario)
1. Al abrir la app se elige equipo: PC, Portátil o Nobara.
2. Ese equipo muestra una notificación para Aceptar/Rechazar.
3. Si el móvil está por cable con ese equipo, los datos van por el cable (adb reverse); si no, por wifi (cifrado).
4. En cada equipo, opciones en su Quickshell.

## Fases
- [x] 0. Toolchain + esqueleto: selector de equipos, pantalla completa, se muestra ENCIMA del bloqueo
      (showWhenLocked; probado: mKeyguardOccluded=true con el patrón puesto).
- [x] 1. Servicio en cada equipo (`host/movild.py`, unidad systemd de usuario `movild`): anuncio UDP 47800,
      TCP 47801 (b"I" info / b"N" Noise XX), notificación Aceptar/Rechazar con la huella del móvil, cable vía
      adb reverse automático. App: descubrimiento (cable 127.0.0.1 + difusión wifi), clave propia
      (identidad.key) y claves de equipos recordadas (equipos.json, avisa si cambian). Estado en
      ~/.local/state/movil-pantalla/movild.json (para Quickshell).
- [x] 2. Escritorio: cliente RFB mínimo con H.264 (wayvnc -g) → MediaCodec directo a la superficie; toques → ratón.
      Inmersivo (sin barra de navegación) y control de pantalla (encender/apagar real) por JNI.
- [x] 3. Sonido (PCM → AAudio), micrófono (AAudio → PCM), cámara (Camera2 con rango de fps fijo → MediaCodec H.264).
- [x] 4. Stand-by nativo (reloj, tiempo, recordatorios, música, PC, fondos animados con los mismos shaders).
- [x] 4b. Almacenamiento: 10 GB reservados en el móvil POR EQUIPO, montados como carpeta en el equipo
      conectado (servidor de ficheros en la app sobre la sesión cifrada + montaje FUSE en el equipo).
- [ ] 5. Inalámbrico: NetGuard (sin internet, solo red local), despliegue en portátil (Debian) y Nobara
      (Hyprland + Quickshell) + opciones en sus Quickshell.

## Estado (16-11-2026, noche)
- Hecho y probado: escritorio (H.264 → MediaCodec → GPU, sin barra de Android), altavoz (AAudio), almacén ~/Móvil (FUSE,
  10 GB/equipo), pantalla apagar/encender (/screen), stand-by nativo (~58 fps, fondo al 35 % en FBO), registros
  cifrados (movil-logs), despliegue en portátil y Nobara (deploy-host.sh; wayvnc instalado por el usuario).
- Hecho, falta probar con permiso: cámara (Camera2 + codificador, fps fijos) y micrófono (AAudio).
- Falta: gestos (probar el usuario), modo wifi real (NetGuard instalado, falta configurarlo en el móvil).
- Depuración: instancia de pruebas movild-prueba (47900-47902, MOVILD_TEST_ACCEPT) + files/depuracion.json en el móvil
  (autorizado por el usuario).
