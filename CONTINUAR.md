# Para seguir en otro ordenador (Nobara)

Contexto para quien continúe (persona o Claude). Lee también [PLAN.md](PLAN.md).

## Qué hay
- Este repositorio = **pantalla-host**: programa del ordenador (Rust, un binario) para la app "Pantalla"
  del móvil. Sustituye a `movild` (Python, en `~/.local/share/movil-pantalla/host/movild.py`) hablando
  **exactamente el mismo protocolo**, así que la app no ha tenido que cambiar.
- Probado en Linux/Hyprland con el móvil (Mi MIX 2S): pantalla, ratón/teclado, sonido, micrófono,
  almacén `~/Móvil`. Windows compila pero **no se ha probado**.
- El **código de la app del móvil (Rust)** está en el Nobara y aún no está en el repositorio.

## Tarea 1 — subir el código de la app al repositorio
1. Acceso a GitHub desde el Nobara: crear una clave **nueva** (una deploy key no se puede repetir en
   otro equipo): `ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519_github -C nobara-monitor-movil`,
   y añadir la `.pub` en https://github.com/migui949-dotcom/Monitor-m-vil/settings/keys con
   **Allow write access**. Alias en `~/.ssh/config`:
   ```
   Host github-pantalla
       HostName github.com
       User git
       IdentityFile ~/.ssh/id_ed25519_github
       IdentitiesOnly yes
   ```
   Clonar: `git clone git@github-pantalla:migui949-dotcom/Monitor-m-vil.git`
2. Localizar el proyecto de la app (buscar `Cargo.toml` / `AndroidManifest.xml` / `org.movilpantalla.app`;
   tiene `app/src/storage.rs`).
3. Copiarlo a la carpeta **`app/`** del repositorio (misma licencia PolyForm Noncommercial).
4. **Antes de subir, revisar que NO van secretos**: claves de firma (`*.jks`, `*.keystore`,
   `keystore.properties`), `local.properties`, `.env`, tokens, claves privadas, registros, `target/`,
   `build/`, `.gradle/`. Añadirlos al `.gitignore`. Enseñar al usuario la lista de archivos antes del push.
5. Commit + push a `main`.

## Tarea 2 — cambios en la app (ver PLAN.md, sección "App")
- El mensaje `info` del ordenador ya trae `"platform": "linux"|"windows"`, `"host": "pantalla-host"` y
  `"v"`: mostrar el sistema en la lista de ordenadores.
- Elegir "escritorio nuevo" o "duplicar" (en Windows sin driver virtual solo hay duplicar).

## Tarea 3 — cámara y stand-by en pantalla-host (ver PLAN.md)

## Cosas importantes que no son obvias
- La app **descarta lo que llega por el canal 1 antes de `{"t":"accepted"}`**: mandar siempre
  `accepted` antes de arrancar el RFB.
- El hello de la app es `{"device":…, "want":["screen","audio","storage"]}`; no manda tamaño de pantalla
  (se usa 1440x720 o `SIZE=` en `~/.config/movil-pantalla/host.conf`).
- La app pide RFB 3.8 sin contraseña y solo las codificaciones `[50 (Open H.264), -223 (DesktopSize)]`.
- El enrutado de sonido devuelve al PC el audio de procesos con ventana en el PC: para probar pitidos
  usar `systemd-run --user …` (y el volumen de PulseAudio es cúbico: 31 % ≈ -31 dB).
- Para probar sin el móvil: `target/release/prueba 127.0.0.1:47901 cap.ppm` contra una instancia con
  `MOVILD_TEST_ACCEPT=1 MOVILD_TCP_PORT=47901 MOVILD_UDP_PORT=47900 MOVILD_ANSWER_PORT=47902 PANTALLA_STATE=/tmp/x`.
- Probar con el móvil de verdad: `systemctl --user stop movild` y lanzar `PANTALLA_DEBUG=1 pantalla-host`
  (misma identidad que movild: lee `~/.local/state/movil-pantalla/movild.key`). En el Nobara la
  identidad será la suya propia.
- El Nobara usa Hyprland 0.57 con configuración en **Lua**: pantalla-host ya lo detecta
  (`hyprctl eval` + `hl.monitor`), pero **no se ha probado ahí**.
