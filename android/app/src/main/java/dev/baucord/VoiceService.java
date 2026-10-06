package dev.baucord;

import android.Manifest;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;
import android.util.Log;

/**
 * The voice connection's foreground service (ANDROID-SPEC.md §10.1, D34).
 * It has no logic: while it runs, Android treats the app as doing
 * something the user sees — the process is not frozen or killed when the
 * app leaves the screen (home, a link, the lock button), the microphone
 * keeps recording in the background, and a notification says the
 * connection is on. Started by BaucordActivity when a session connects,
 * stopped when it ends; the process exiting (the Activity destroyed,
 * D11) takes it along.
 */
public class VoiceService extends Service {
    private static final String TAG = "baucord";
    private static final String CHANNEL = "voice";
    private static final int NOTIFICATION_ID = 1;
    private static final String EXTRA_CHANNEL = "channel";
    private static final String EXTRA_TITLE = "title";
    private static final String EXTRA_TEXT = "text";

    /** The last start's words, for {@link #refresh}; null while stopped. */
    private static volatile Intent current;

    private PowerManager.WakeLock wakeLock;

    static void start(Context context, String channelName, String title, String text) {
        Intent intent = new Intent(context, VoiceService.class)
                .putExtra(EXTRA_CHANNEL, channelName)
                .putExtra(EXTRA_TITLE, title)
                .putExtra(EXTRA_TEXT, text);
        current = intent;
        context.startForegroundService(intent);
    }

    static void stop(Context context) {
        current = null;
        context.stopService(new Intent(context, VoiceService.class));
    }

    /** Start again with the same words (a permission was just granted). */
    static void refresh(Context context) {
        Intent intent = current;
        if (intent != null) {
            try {
                context.startForegroundService(intent);
            } catch (Exception e) {
                Log.w(TAG, "refreshing the voice service failed", e);
            }
        }
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String channelName = extra(intent, EXTRA_CHANNEL, "Voice connection");
        String title = extra(intent, EXTRA_TITLE, "baucord");
        String text = extra(intent, EXTRA_TEXT, "");

        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(
                CHANNEL, channelName, NotificationManager.IMPORTANCE_LOW));

        Intent open = new Intent(this, BaucordActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        PendingIntent tap = PendingIntent.getActivity(this, 0, open,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification.Builder builder = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_stat_voice)
                .setContentTitle(title)
                .setContentText(text)
                .setContentIntent(tap)
                .setOngoing(true)
                .setCategory(Notification.CATEGORY_CALL);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            builder.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        }

        // Media playback always (hearing the room); the microphone type
        // only with the permission — Android 14 refuses the type without
        // it, and the type is what lets the microphone run in the
        // background. API 29 has no microphone type and needs none.
        int type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
                && checkSelfPermission(Manifest.permission.RECORD_AUDIO)
                        == PackageManager.PERMISSION_GRANTED) {
            type |= ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
        }
        try {
            startForeground(NOTIFICATION_ID, builder.build(), type);
        } catch (Exception e) {
            Log.w(TAG, "startForeground refused", e);
            stopSelf();
            return START_NOT_STICKY;
        }

        // The CPU stays up with the screen off, so audio and the network
        // keep their timing.
        if (wakeLock == null) {
            PowerManager power = getSystemService(PowerManager.class);
            wakeLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "baucord:voice");
            wakeLock.setReferenceCounted(false);
            wakeLock.acquire();
        }
        // Not restarted by the system after a kill: the session it kept
        // alive is gone with the process.
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        if (wakeLock != null && wakeLock.isHeld()) {
            wakeLock.release();
        }
        wakeLock = null;
        stopForeground(STOP_FOREGROUND_REMOVE);
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    private static String extra(Intent intent, String key, String fallback) {
        String value = intent != null ? intent.getStringExtra(key) : null;
        return value != null ? value : fallback;
    }
}
