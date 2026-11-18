package org.movilpantalla.app;

import android.Manifest;
import android.app.NativeActivity;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.ComponentName;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.net.VpnService;
import android.os.PowerManager;
import android.provider.Settings;
import android.net.wifi.WifiManager;
import android.os.BatteryManager;
import android.os.Environment;
import android.os.StatFs;
import java.util.Locale;
import android.os.Bundle;
import android.view.OrientationEventListener;
import android.view.View;
import android.view.WindowManager;
import android.view.KeyEvent;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.text.InputType;
import android.os.Build;

/**
 * Capa mínima en Java sobre la app en Rust (NativeActivity):
 *  - pantalla completa "inmersiva" (sin barra de estado ni de navegación)
 *  - permisos de cámara y micrófono cuando la app los pida
 *  - mantener la pantalla encendida / dejar que se apague (llamado desde Rust por JNI)
 */
public class MainActivity extends NativeActivity {
    private static final int IMMERSIVE = View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN
            | View.SYSTEM_UI_FLAG_LAYOUT_STABLE | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        immersive();
        // orientación física del móvil (acelerómetro), para girar la imagen de la cámara
        orient = new OrientationEventListener(this) {
            public void onOrientationChanged(int o) {
                if (o == ORIENTATION_UNKNOWN) return;
                int r = ((o + 45) / 90 * 90) % 360;
                // un poco de histéresis: no cambiar cerca de la diagonal
                int d = Math.abs(o - r); if (d > 180) d = 360 - d;
                if (d < 35) deviceDeg = r;
            }
        };
        if (orient.canDetectOrientation()) orient.enable();
        // si el cortafuegos estaba puesto (y Android ya dio permiso de VPN), se vuelve a poner
        if (prefs().getBoolean("cortafuegos", false) && VpnService.prepare(this) == null) startFirewallService();
    }

    private OrientationEventListener orient;
    private volatile int deviceDeg = -1;

    /** Giro del móvil respecto a su posición natural (vertical), en grados horarios: 0/90/180/270, -1 si no se sabe. */
    public int deviceOrientation() { return deviceDeg; }

    @Override
    protected void onDestroy() {
        if (orient != null) orient.disable();
        super.onDestroy();
    }

    // ───────────── Cortafuegos propio (Cortafuegos.java) ─────────────

    private static final int REQ_VPN = 7;

    private SharedPreferences prefs() { return getSharedPreferences("pantalla", MODE_PRIVATE); }

    private void startFirewallService() {
        startForegroundService(new Intent(this, Cortafuegos.class));
    }

    /** 0 = apagado, 1 = puesto. */
    public int firewallState() {
        return Cortafuegos.running ? 1 : 0;
    }

    /** Pone o quita el cortafuegos (la primera vez Android pide permiso para la "VPN"). */
    public void setFirewall(final boolean on) {
        runOnUiThread(new Runnable() {
            public void run() {
                prefs().edit().putBoolean("cortafuegos", on).apply();
                if (!on) {
                    startService(new Intent(MainActivity.this, Cortafuegos.class).setAction(Cortafuegos.STOP));
                    return;
                }
                Intent ask = VpnService.prepare(MainActivity.this);
                if (ask != null) startActivityForResult(ask, REQ_VPN);
                else startFirewallService();
            }
        });
    }

    @Override
    protected void onActivityResult(int req, int res, Intent data) {
        super.onActivityResult(req, res, data);
        if (req == REQ_VPN) {
            if (res == RESULT_OK) startFirewallService();
            else prefs().edit().putBoolean("cortafuegos", false).apply();
        }
    }

    /** true si Android no va a cerrar la app para ahorrar batería. */
    public boolean batteryExempt() {
        PowerManager pm = (PowerManager) getSystemService(POWER_SERVICE);
        return pm.isIgnoringBatteryOptimizations(getPackageName());
    }

    /** Pide quitar la app del ahorro de batería y abre el "inicio automático" de MIUI. */
    public void askBattery() {
        runOnUiThread(new Runnable() {
            public void run() {
                try {
                    if (!batteryExempt())
                        startActivity(new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                                Uri.parse("package:" + getPackageName())));
                    else openAutostart();
                } catch (Exception e) { openAutostart(); }
            }
        });
    }

    /** Ajuste de MIUI "Inicio automático" (si no existe, los ajustes de la app). */
    public void openAutostart() {
        runOnUiThread(new Runnable() {
            public void run() {
                try {
                    startActivity(new Intent().setComponent(new ComponentName("com.miui.securitycenter",
                            "com.miui.permcenter.autostart.AutoStartManagementActivity")));
                } catch (Exception e) {
                    startActivity(new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                            Uri.parse("package:" + getPackageName())));
                }
            }
        });
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) immersive();
    }

    private void immersive() {
        getWindow().getDecorView().setSystemUiVisibility(IMMERSIVE);
    }

    /** Pide cámara y micrófono si aún no están concedidos (desde Rust, cualquier hilo). */
    public void askMediaPermissions() {
        runOnUiThread(new Runnable() {
            public void run() {
                if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED
                        || checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
                    requestPermissions(new String[]{Manifest.permission.CAMERA, Manifest.permission.RECORD_AUDIO}, 1);
                }
            }
        });
    }

    /** true si la app ya tiene cámara y micrófono. */
    public boolean hasMediaPermissions() {
        return checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
                && checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED;
    }

    /** Mantener la pantalla encendida (true) o dejar que se apague (false). Desde Rust, cualquier hilo. */
    public void setKeepScreenOn(final boolean on) {
        runOnUiThread(new Runnable() {
            public void run() {
                if (on) getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
                else getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    /** "Apagar" la pantalla sin bloquear el móvil: brillo de la ventana al mínimo (true) o normal (false). */
    public void setDim(final boolean dim) {
        runOnUiThread(new Runnable() {
            public void run() {
                WindowManager.LayoutParams lp = getWindow().getAttributes();
                lp.screenBrightness = dim ? 0.0f : WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE;
                getWindow().setAttributes(lp);
            }
        });
    }

    /** Estado del móvil para el ordenador (JSON): batería, temperatura, almacenamiento, IP. */
    public String getInfo() {
        Intent b = registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
        int level = -1, scale = 100, status = -1, temp = 0;
        if (b != null) {
            level = b.getIntExtra(BatteryManager.EXTRA_LEVEL, -1);
            scale = b.getIntExtra(BatteryManager.EXTRA_SCALE, 100);
            status = b.getIntExtra(BatteryManager.EXTRA_STATUS, -1);
            temp = b.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, 0);
        }
        boolean charging = status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL;
        StatFs st = new StatFs(Environment.getDataDirectory().getPath());
        String ip = "";
        try {
            WifiManager wm = (WifiManager) getApplicationContext().getSystemService(WIFI_SERVICE);
            int a = wm.getConnectionInfo().getIpAddress();
            if (a != 0) ip = String.format(Locale.ROOT, "%d.%d.%d.%d", a & 255, (a >> 8) & 255, (a >> 16) & 255, (a >> 24) & 255);
        } catch (Exception e) { }
        float br = getWindow().getAttributes().screenBrightness;
        return String.format(Locale.ROOT,
            "{\"level\":%d,\"charging\":%b,\"btemp\":%.1f,\"stotal\":%d,\"sfree\":%d,\"ip\":\"%s\",\"brightness\":%.2f}",
            scale > 0 ? level * 100 / scale : -1, charging, temp / 10.0, st.getTotalBytes(), st.getAvailableBytes(), ip, br);
    }

    /** Brillo de la ventana de la app: 0..1, o negativo para el brillo normal del sistema. */
    public void setBrightness(final float v) {
        runOnUiThread(new Runnable() {
            public void run() {
                WindowManager.LayoutParams lp = getWindow().getAttributes();
                lp.screenBrightness = v < 0 ? WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE : Math.max(0.01f, Math.min(1f, v));
                getWindow().setAttributes(lp);
            }
        });
    }

    /** Encender la pantalla al mostrarse (true) o no (false): permite apagarla de verdad. */
    public void setWakeOnShow(final boolean on) {
        runOnUiThread(new Runnable() {
            public void run() {
                setTurnScreenOn(on);
            }
        });
    }

    // ───────────── Teclado del móvil para escribir en el ordenador ─────────────
    // Una vista invisible que hace de campo de texto: el teclado de Android le manda lo que se
    // escribe (InputConnection) y Rust lo recoge con takeTyped() en cada fotograma.
    // '\b' = borrar, '\n' = Intro, '\u007f' = Supr, '\t' = tabulador.

    private final StringBuilder typed = new StringBuilder();
    private View teclado;
    private volatile boolean tecladoOn;

    synchronized void typed(String s) { typed.append(s); }

    /** Lo escrito desde la última llamada (vacío si nada). */
    public synchronized String takeTyped() {
        String s = typed.toString();
        typed.setLength(0);
        return s;
    }

    /** Alto del teclado en píxeles (Android 11+), -1 si no se sabe. */
    public int imeHeight() {
        if (Build.VERSION.SDK_INT < 30) return -1;
        try {
            WindowInsets in = getWindow().getDecorView().getRootWindowInsets();
            return in == null ? -1 : in.getInsets(WindowInsets.Type.ime()).bottom;
        } catch (Exception e) { return -1; }
    }

    public void showKeyboard(final boolean on) {
        tecladoOn = on;
        runOnUiThread(new Runnable() {
            public void run() {
                final InputMethodManager imm = (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                if (teclado == null) {
                    teclado = new Teclado();
                    addContentView(teclado, new ViewGroup.LayoutParams(1, 1));
                }
                if (on) {
                    teclado.requestFocus();
                    imm.restartInput(teclado);
                    imm.showSoftInput(teclado, 0);
                    // a veces la vista aún no está lista: se insiste un momento después
                    teclado.postDelayed(new Runnable() { public void run() {
                        if (tecladoOn) { teclado.requestFocus(); imm.showSoftInput(teclado, 0); }
                    } }, 150);
                } else {
                    imm.hideSoftInputFromWindow(teclado.getWindowToken(), 0);
                    teclado.clearFocus();
                }
                immersive();
            }
        });
    }

    private class Teclado extends View {
        Teclado() {
            super(MainActivity.this);
            setFocusable(true);
            setFocusableInTouchMode(true);
        }

        @Override public boolean onCheckIsTextEditor() { return true; }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo o) {
            // sin sugerencias ni autocorrección (cada letra se manda al momento); sin pantalla completa
            o.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                    | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            o.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN
                    | EditorInfo.IME_ACTION_NONE;
            return new BaseInputConnection(this, false) {
                /** texto «a medias» del teclado (lo que va subrayado mientras se escribe) */
                private String comp = "";

                private void replace(String now) {
                    int p = 0;
                    while (p < comp.length() && p < now.length() && comp.charAt(p) == now.charAt(p)) p++;
                    StringBuilder out = new StringBuilder();
                    for (int i = comp.codePointCount(p, comp.length()); i > 0; i--) out.append('\b');
                    out.append(now.substring(p));
                    if (out.length() > 0) typed(out.toString());
                }

                @Override public boolean commitText(CharSequence t, int pos) {
                    replace(t.toString()); comp = ""; return true;
                }
                @Override public boolean setComposingText(CharSequence t, int pos) {
                    String n = t.toString(); replace(n); comp = n; return true;
                }
                @Override public boolean finishComposingText() { comp = ""; return true; }
                @Override public boolean deleteSurroundingText(int before, int after) {
                    StringBuilder b = new StringBuilder();
                    for (int i = 0; i < before; i++) b.append('\b');
                    for (int i = 0; i < after; i++) b.append('\u007f');
                    typed(b.toString());
                    return true;
                }
                @Override public boolean performEditorAction(int a) { typed("\n"); return true; }
                @Override public boolean sendKeyEvent(KeyEvent e) {
                    if (e.getAction() != KeyEvent.ACTION_DOWN) return true;
                    switch (e.getKeyCode()) {
                        case KeyEvent.KEYCODE_DEL: typed("\b"); break;
                        case KeyEvent.KEYCODE_FORWARD_DEL: typed("\u007f"); break;
                        case KeyEvent.KEYCODE_ENTER: case KeyEvent.KEYCODE_NUMPAD_ENTER: typed("\n"); break;
                        case KeyEvent.KEYCODE_TAB: typed("\t"); break;
                        default:
                            int c = e.getUnicodeChar();
                            if (c > 0) typed(new String(Character.toChars(c)));
                    }
                    return true;
                }
            };
        }
    }
}
