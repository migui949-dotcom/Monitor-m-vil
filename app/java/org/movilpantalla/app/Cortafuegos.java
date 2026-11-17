package org.movilpantalla.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Intent;
import android.net.VpnService;
import android.os.ParcelFileDescriptor;

/**
 * Cortafuegos propio (sustituye a NetGuard): una "VPN" local que se queda con TODO el tráfico de
 * las demás apps y lo tira, sin mandarlo a ningún sitio. La app Pantalla queda fuera de la VPN
 * (addDisallowedApplication), así que ella sí llega a los equipos de casa por el wifi.
 * No hay ningún servidor: los paquetes nunca salen del móvil.
 */
public class Cortafuegos extends VpnService {
    static final String STOP = "org.movilpantalla.app.CORTAFUEGOS_STOP";
    static volatile boolean running = false;
    private ParcelFileDescriptor tun;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && STOP.equals(intent.getAction())) {
            close();
            stopForeground(true);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (tun == null) {
            try {
                Builder b = new Builder()
                        .setSession("Pantalla: internet bloqueado")
                        .addAddress("10.111.222.1", 32)
                        .addRoute("0.0.0.0", 0)
                        .addAddress("fd6d:6f76:696c::1", 128)
                        .addRoute("::", 0)
                        .setBlocking(false);
                b.addDisallowedApplication(getPackageName());
                tun = b.establish();
            } catch (Exception e) {
                tun = null;
            }
        }
        running = tun != null;
        if (!running) { stopSelf(); return START_NOT_STICKY; }
        startForeground(1, notification());
        return START_STICKY;
    }

    private Notification notification() {
        NotificationManager nm = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        nm.createNotificationChannel(new NotificationChannel("cortafuegos", "Cortafuegos", NotificationManager.IMPORTANCE_MIN));
        PendingIntent open = PendingIntent.getActivity(this, 0, new Intent(this, MainActivity.class), 0);
        return new Notification.Builder(this, "cortafuegos")
                .setSmallIcon(android.R.drawable.ic_lock_lock)
                .setContentTitle("Internet bloqueado")
                .setContentText("Solo Pantalla puede usar el wifi, y solo con los equipos de casa")
                .setContentIntent(open)
                .setOngoing(true)
                .build();
    }

    private void close() {
        running = false;
        try { if (tun != null) tun.close(); } catch (Exception e) { }
        tun = null;
    }

    /** Otra VPN (p. ej. NetGuard) ha ocupado el sitio o el usuario la ha quitado en Ajustes. */
    @Override
    public void onRevoke() {
        close();
        getSharedPreferences("pantalla", MODE_PRIVATE).edit().putBoolean("cortafuegos", false).apply();
        stopForeground(true);
        stopSelf();
    }

    @Override
    public void onDestroy() {
        close();
        super.onDestroy();
    }
}
