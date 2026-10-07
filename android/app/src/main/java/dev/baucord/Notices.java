package dev.baucord;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.media.AudioManager;
import android.util.Log;

/**
 * The app's notifications (ANDROID-SPEC.md D52): its look — the status-bar
 * silhouette of the app icon, the accent colour, the launcher icon itself
 * as the large icon — for every notification including the voice
 * service's, and the message and voice-activity notifications Rust posts
 * while the app is in the background ({@code app::alerts} decides when).
 * Their sound is the channel's: Android applies the ringer mode, Do Not
 * Disturb and the user's per-channel settings itself.
 */
final class Notices {
    private static final String TAG = "baucord";

    /** Direct messages: high importance (sound, heads-up). */
    static final String CHANNEL_MESSAGES = "messages";
    /** Someone started a call: default importance (sound, no heads-up). */
    static final String CHANNEL_ACTIVITY = "activity";
    /** The intent extra a message notification's tap carries: the user id in hex. */
    static final String EXTRA_DM = "dev.baucord.dm";
    /**
     * Every notice uses this id under its own tag (the key Rust gives:
     * "dm:…", "join"); the voice service's notification is id 1.
     */
    private static final int NOTICE_ID = 2;
    /** Notices posted without a sound: a group whose children never alert. */
    private static final String QUIET_GROUP = "dev.baucord.quiet";

    private static Bitmap largeIcon;

    private Notices() {}

    /**
     * The app's look on a notification: the white dog silhouette in the
     * status bar, the accent behind it, and the launcher icon as drawn
     * on the home screen.
     */
    static Notification.Builder styled(Context context, Notification.Builder builder) {
        builder.setSmallIcon(R.drawable.ic_stat_baucord)
                .setColor(context.getColor(R.color.baucord_accent));
        Bitmap icon = largeIcon(context);
        if (icon != null) {
            builder.setLargeIcon(icon);
        }
        return builder;
    }

    /** The launcher icon (the adaptive icon, masked as the launcher masks it), once. */
    private static synchronized Bitmap largeIcon(Context context) {
        if (largeIcon == null) {
            try {
                Drawable icon = context.getPackageManager()
                        .getApplicationIcon(context.getApplicationInfo());
                int size = Math.round(64 * context.getResources().getDisplayMetrics().density);
                Bitmap bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888);
                icon.setBounds(0, 0, size, size);
                icon.draw(new Canvas(bitmap));
                largeIcon = bitmap;
            } catch (Exception e) {
                Log.w(TAG, "no launcher icon for notifications", e);
            }
        }
        return largeIcon;
    }

    /**
     * Post (or replace, by {@code key}) a notification. {@code kind} is
     * "message" or "activity"; {@code dm}, when not empty, is the
     * conversation a tap opens; {@code sound} false posts it silently.
     */
    static void post(Context context, String kind, String key, String channelName,
            String title, String text, String dm, boolean sound) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager == null) {
            return;
        }
        boolean message = "message".equals(kind);
        String channel = message ? CHANNEL_MESSAGES : CHANNEL_ACTIVITY;
        // Creating an existing channel only renames it (the user's own
        // choices for it stay).
        manager.createNotificationChannel(new NotificationChannel(channel, channelName,
                message ? NotificationManager.IMPORTANCE_HIGH
                        : NotificationManager.IMPORTANCE_DEFAULT));

        Intent open = new Intent(context, BaucordActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        if (dm != null && !dm.isEmpty()) {
            open.putExtra(EXTRA_DM, dm);
        }
        // One PendingIntent per key: their extras differ.
        PendingIntent tap = PendingIntent.getActivity(context, key.hashCode(), open,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification.Builder builder = styled(context, new Notification.Builder(context, channel))
                .setContentTitle(title)
                .setContentText(text)
                .setContentIntent(tap)
                .setAutoCancel(true)
                .setShowWhen(true)
                .setWhen(System.currentTimeMillis())
                .setCategory(message ? Notification.CATEGORY_MESSAGE
                        : Notification.CATEGORY_SOCIAL);
        if (text != null && !text.isEmpty()) {
            builder.setStyle(new Notification.BigTextStyle().bigText(text));
        }
        if (!sound) {
            // What NotificationCompat.setSilent does: a member of a
            // group that alerts only through its (absent) summary.
            builder.setGroup(QUIET_GROUP)
                    .setGroupAlertBehavior(Notification.GROUP_ALERT_SUMMARY);
        }
        try {
            manager.notify(key, NOTICE_ID, builder.build());
        } catch (Exception e) {
            // No POST_NOTIFICATIONS (Android 13+, declined): nothing shows.
            Log.w(TAG, "notification " + key + " refused", e);
        }
    }

    /**
     * Whether a notification of {@code kind} would show: notifications
     * allowed for the app (Android 13+ asks) and its channel, once
     * created, not switched off by the user. When not, Rust chimes
     * instead.
     */
    static boolean allowed(Context context, String kind) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager == null || !manager.areNotificationsEnabled()) {
            return false;
        }
        String id = "message".equals(kind) ? CHANNEL_MESSAGES : CHANNEL_ACTIVITY;
        NotificationChannel channel = manager.getNotificationChannel(id);
        return channel == null
                || channel.getImportance() != NotificationManager.IMPORTANCE_NONE;
    }

    /** Take down the notification posted under {@code key}. */
    static void cancel(Context context, String key) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager != null) {
            manager.cancel(key, NOTICE_ID);
        }
    }

    /**
     * Whether the app may play an alert chime itself: the ringer is on
     * (not silent or vibrate) and Do Not Disturb is off. The voice
     * call's own audio ignores both, so the app asks before each chime.
     */
    static boolean chimeAllowed(Context context) {
        AudioManager audio = context.getSystemService(AudioManager.class);
        if (audio != null && audio.getRingerMode() != AudioManager.RINGER_MODE_NORMAL) {
            return false;
        }
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager != null) {
            int filter = manager.getCurrentInterruptionFilter();
            // ALL (1) is "no Do Not Disturb"; UNKNOWN (0) says nothing.
            if (filter != NotificationManager.INTERRUPTION_FILTER_ALL
                    && filter != NotificationManager.INTERRUPTION_FILTER_UNKNOWN) {
                return false;
            }
        }
        return true;
    }
}
