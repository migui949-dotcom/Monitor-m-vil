package org.movilpantalla.app;

import android.Manifest;
import android.app.NativeActivity;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.net.wifi.WifiManager;
import android.os.BatteryManager;
import android.os.Environment;
import android.os.StatFs;
import java.util.Locale;
import android.os.Bundle;
import android.view.View;
import android.view.WindowManager;

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
}
