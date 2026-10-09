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
 * The voice connection's foreground service (ANDROID-SPEC.md §10.1, D34,
 * D63). It has no logic: while it runs, Android treats the app as doing
 * something the user sees — the process is not frozen or killed when the
 * app leaves the screen (home, a link, the lock button), the microphone
 * keeps recording in the background, and a notification says the
 * connection is on. Started by BaucordActivity when a session connects,
 * started again when the session enters or leaves a voice room, stopped
 * when it ends; the process exiting (the Activity destroyed, D11) takes
 * it along.
 *
 * Its type follows the session (PLAY-STORE.md §5): {@code microphone}
 * in a voice room (with the permission), {@code specialUse} in the
 * lobby, where no microphone is open — waiting for a call or a direct
 * message, which a self-hosted server can only deliver over the open
 * connection. The CPU wake lock is held in a room only.
 */
public class VoiceService extends Service {
    private static final String TAG = "baucord";
    private static final String CHANNEL = "voice";
    private static final int NOTIFICATION_ID = 1;
    private static final String EXTRA_CHANNEL = "channel";
    private static final String EXTRA_TITLE = "title";
    private static final String EXTRA_TEXT = "text";
    private static final String EXTRA_IN_ROOM = "in_room";

    /** The last start's words, for {@link #refresh}; null while stopped. */
    private static volatile Intent current;

    // A start not yet answered by onStartCommand: Android crashes an app
    // whose service is stopped before it called startForeground ("did
    // not then call Service.startForeground"), so a stop that comes that
    // early (connect, then disconnect at once) waits for it instead.
    private static final Object LOCK = new Object();
    private static int starting;
    private static boolean stopAfterStart;

    private PowerManager.WakeLock wakeLock;
    /** startForeground succeeded at least once since this instance started. */
    private boolean foreground;

    static void start(Context context, String channelName, String title, String text,
            boolean inRoom) {
        Intent intent = new Intent(context, VoiceService.class)
                .putExtra(EXTRA_CHANNEL, channelName)
                .putExtra(EXTRA_TITLE, title)
                .putExtra(EXTRA_TEXT, text)
                .putExtra(EXTRA_IN_ROOM, inRoom);
        synchronized (LOCK) {
            current = intent;
            stopAfterStart = false;
        }
        send(context, intent);
    }

    /** startForegroundService, counted until onStartCommand answers it. */
    private static void send(Context context, Intent intent) {
        synchronized (LOCK) {
            starting++;
        }
        try {
            context.startForegroundService(intent);
        } catch (RuntimeException e) {
            // Refused (from the background): no onStartCommand will come.
            synchronized (LOCK) {
                starting--;
            }
            throw e;
        }
    }

    static void stop(Context context) {
        synchronized (LOCK) {
            current = null;
            if (starting > 0) {
                stopAfterStart = true;
                return;
            }
        }
        context.stopService(new Intent(context, VoiceService.class));
    }

    /** Start again with the same words (a permission was just granted). */
    static void refresh(Context context) {
        Intent intent = current;
        if (intent != null) {
            try {
                send(context, intent);
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
        boolean inRoom = intent != null && intent.getBooleanExtra(EXTRA_IN_ROOM, false);

        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(
                CHANNEL, channelName, NotificationManager.IMPORTANCE_LOW));

        Intent open = new Intent(this, BaucordActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        PendingIntent tap = PendingIntent.getActivity(this, 0, open,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification.Builder builder = Notices.styled(this, new Notification.Builder(this, CHANNEL))
                .setContentTitle(title)
                .setContentText(text)
                .setContentIntent(tap)
                .setOngoing(true)
                .setCategory(Notification.CATEGORY_CALL);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            builder.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        }

        // In a room, the microphone type — only with the permission:
        // Android 14 refuses the type without it, and the type is what
        // lets the microphone run in the background (API 29 has no such
        // type and needs none). In the lobby nothing records: the
        // special-use type (API 34+), or no type below that, where types
        // are optional.
        Notification notification = builder.build();
        boolean mic = inRoom
                && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
                && checkSelfPermission(Manifest.permission.RECORD_AUDIO)
                        == PackageManager.PERMISSION_GRANTED;
        boolean started = mic && promote(notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
        if (!started) {
            // The lobby; or a room whose microphone type was refused —
            // started from the background (a reconnect while the app is
            // off screen): the connection stays, and the microphone
            // comes back when the app is opened (PLAY-STORE.md §5).
            started = promote(notification, lobbyType());
        }
        if (started) {
            foreground = true;
        }

        // This start is answered; a stop that came before it can run now.
        boolean stopNow;
        synchronized (LOCK) {
            starting = Math.max(0, starting - 1);
            stopNow = stopAfterStart && starting == 0;
            if (stopNow) {
                stopAfterStart = false;
            }
        }
        if (stopNow) {
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (!foreground) {
            // Never in the foreground: Android requires startForeground
            // after startForegroundService, and stopping instead is its
            // "did not call startForeground" crash. Nothing else to try.
            Log.e(TAG, "the voice service could not enter the foreground");
            return START_NOT_STICKY;
        }
        // Already in the foreground from an earlier start when this one
        // was refused: it stays as it was rather than stopping — that
        // would end what keeps the app alive.

        // The CPU stays up with the screen off in a room, so audio and
        // the network keep their timing; the lobby lets it sleep.
        holdWakeLock(inRoom);
        // Not restarted by the system after a kill: the session it kept
        // alive is gone with the process.
        return START_NOT_STICKY;
    }

    /** The lobby's type: special use from API 34, none before (optional there). */
    private static int lobbyType() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            return ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE;
        }
        return ServiceInfo.FOREGROUND_SERVICE_TYPE_NONE;
    }

    /** startForeground with {@code type}; false when the system refuses. */
    private boolean promote(Notification notification, int type) {
        try {
            startForeground(NOTIFICATION_ID, notification, type);
            return true;
        } catch (Exception e) {
            Log.w(TAG, "startForeground refused (type " + type + ")", e);
            return false;
        }
    }

    private void holdWakeLock(boolean hold) {
        if (hold && wakeLock == null) {
            PowerManager power = getSystemService(PowerManager.class);
            wakeLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "baucord:voice");
            wakeLock.setReferenceCounted(false);
            wakeLock.acquire();
        } else if (!hold && wakeLock != null) {
            if (wakeLock.isHeld()) {
                wakeLock.release();
            }
            wakeLock = null;
        }
    }

    @Override
    public void onDestroy() {
        holdWakeLock(false);
        foreground = false;
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
