package dev.baucord;

import android.app.NativeActivity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.ContentResolver;
import android.content.ContentValues;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Environment;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.util.Log;
import android.view.WindowManager;
import android.webkit.MimeTypeMap;

import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;

/**
 * The platform's NativeActivity plus what native code cannot do on its
 * own (ANDROID-SPEC.md D30): an activity result. NativeActivity never
 * hands one to native code, so the system file picker needs this
 * subclass; the other helpers sit here because they are a few lines of
 * Java against a page of JNI. Everything is called from Rust
 * ({@code crates/android/src/java.rs}); {@link #nativeFilePicked} is
 * registered from Rust at startup ({@code RegisterNatives} — the
 * library is loaded by NativeActivity, not by this class's loader).
 */
public class BaucordActivity extends NativeActivity {
    private static final String TAG = "baucord";
    private static final int PICK_FILE = 4201;

    /** The limit of the pick in flight (only one at a time). */
    private long pickLimit;

    /**
     * A pick is over: {@code path} is a readable copy in the cache, or
     * null with {@code error} set ({@code "too-large"} or a message),
     * or all null when the user backed out.
     */
    static native void nativeFilePicked(String path, String name, String error);

    /**
     * A message notification was tapped while the app runs: open the DM
     * conversation with {@code dm} (the user id in hex). Registered from
     * Rust like {@link #nativeFilePicked} (ANDROID-SPEC.md D52).
     */
    static native void nativeOpenConversation(String dm);

    // ── Permissions ──────────────────────────────────────────────────────

    /**
     * Rust: the system's permission prompts for {@code names}, on the UI
     * thread (from android_main's thread at startup the request could
     * land before the Activity is shown). The answer is not passed back:
     * the voice engine retries the microphone until it opens.
     */
    public void askPermissions(final String[] names) {
        runOnUiThread(() -> requestPermissions(names, 1));
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions,
            int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        // Granted while connected: the service notification can show now,
        // and the microphone type is allowed — start it again with both.
        VoiceService.refresh(this);
    }

    // ── The voice connection's foreground service (D34) ────────────────

    /**
     * Rust, on connect: keep the process running in the background — a
     * foreground service with a notification. Android otherwise freezes
     * an app soon after it leaves the screen, and the server drops it.
     * Returns false when the system refused (started from the
     * background).
     */
    public boolean startVoiceService(String channelName, String title, String text) {
        try {
            VoiceService.start(this, channelName, title, text);
            return true;
        } catch (Exception e) {
            Log.w(TAG, "the voice service could not start", e);
            return false;
        }
    }

    /** Rust, on disconnect: let the process be an ordinary app again. */
    public void stopVoiceService() {
        VoiceService.stop(this);
    }

    // ── Notifications (ANDROID-SPEC.md D52) ─────────────────────────────

    /**
     * Rust: post (or replace, by {@code key}) a message or voice-activity
     * notification. Any thread.
     */
    public void postNotice(String kind, String key, String channelName, String title,
            String text, String dm, boolean sound) {
        Notices.post(this, kind, key, channelName, title, text, dm, sound);
    }

    /** Rust: take down the notification posted under {@code key}. */
    public void cancelNotice(String key) {
        Notices.cancel(this, key);
    }

    /** Rust: whether a notification of {@code kind} would show at all. */
    public boolean noticesAllowed(String kind) {
        return Notices.allowed(this, kind);
    }

    /** Rust: play the message chime while no voice stream is open (D53). */
    public void playChime(final byte[] wav) {
        runOnUiThread(() -> Notices.playChime(this, wav));
    }

    /** Rust: whether the phone's ringer and Do Not Disturb allow an alert chime now. */
    public boolean chimeAllowed() {
        return Notices.chimeAllowed(this);
    }

    /**
     * A notification's tap with the app running (singleTask: the same
     * Activity, a new intent). One about a message opens its
     * conversation; the voice service's just brings the app back.
     */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        String dm = intent != null ? intent.getStringExtra(Notices.EXTRA_DM) : null;
        if (dm != null && !dm.isEmpty()) {
            try {
                nativeOpenConversation(dm);
            } catch (UnsatisfiedLinkError e) {
                Log.w(TAG, "nativeOpenConversation is not bound", e);
            }
        }
    }

    // ── The screen while a stream plays (ANDROID-SPEC.md D45) ──────────

    /**
     * Rust: keep the screen on while a stream is watched ({@code true}),
     * or let it time out and lock again ({@code false}). The window flag
     * only holds while this window is visible, so leaving the app lets
     * the phone sleep regardless. Window flags belong to the UI thread.
     */
    public void keepScreenOn(final boolean on) {
        runOnUiThread(() -> {
            if (on) {
                getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    // ── The clipboard ────────────────────────────────────────────────────

    /**
     * Rust: put {@code text} on the clipboard (Settings → Debug's "Copy
     * log", an invite code); {@code false} if the system refused it. Any
     * thread: the manager posts nothing to the caller's looper, and a
     * clip is one binder call — which is why Rust keeps the text well
     * under a binder transaction's 1 MB. Android 13+ confirms the copy
     * on screen itself.
     */
    public boolean copyText(String label, String text) {
        try {
            ClipboardManager clipboard = (ClipboardManager) getSystemService(CLIPBOARD_SERVICE);
            if (clipboard == null) {
                return false;
            }
            clipboard.setPrimaryClip(ClipData.newPlainText(label, text));
            return true;
        } catch (RuntimeException e) {
            Log.w(TAG, "copy to the clipboard failed", e);
            return false;
        }
    }

    // ── Files ────────────────────────────────────────────────────────────

    /** Rust: the system picker for one file of {@code mime}. Any thread. */
    public void pickFile(final String mime, final long maxBytes) {
        runOnUiThread(() -> {
            pickLimit = maxBytes;
            Intent intent = new Intent(Intent.ACTION_GET_CONTENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType(mime);
            try {
                startActivityForResult(intent, PICK_FILE);
            } catch (ActivityNotFoundException e) {
                nativeFilePicked(null, null, "no file picker on this device");
            }
        });
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != PICK_FILE) {
            return;
        }
        if (resultCode != RESULT_OK || data == null || data.getData() == null) {
            nativeFilePicked(null, null, null);
            return;
        }
        final Uri uri = data.getData();
        final long limit = pickLimit;
        // A video off a cloud drive takes a while: not on the UI thread.
        new Thread(() -> copyPicked(uri, limit), "baucord-pick").start();
    }

    /** Copy the picked document into cache/picked/ under its own name. */
    private void copyPicked(Uri uri, long limit) {
        ContentResolver resolver = getContentResolver();
        String name = displayName(resolver, uri);
        File dir = new File(getCacheDir(), "picked");
        // One pick lives here at a time; the last one has been read.
        File[] old = dir.listFiles();
        if (old != null) {
            for (File f : old) {
                f.delete();
            }
        }
        dir.mkdirs();
        File out = new File(dir, name);
        boolean tooLarge = false;
        try (InputStream in = resolver.openInputStream(uri);
             OutputStream os = new FileOutputStream(out)) {
            if (in == null) {
                throw new java.io.IOException("the provider returned no data");
            }
            byte[] buf = new byte[64 * 1024];
            long total = 0;
            int n;
            while ((n = in.read(buf)) > 0) {
                total += n;
                if (total > limit) {
                    tooLarge = true;
                    break;
                }
                os.write(buf, 0, n);
            }
        } catch (Exception e) {
            Log.w(TAG, "copying the picked file failed", e);
            out.delete();
            nativeFilePicked(null, name, String.valueOf(e.getMessage()));
            return;
        }
        if (tooLarge) {
            out.delete();
            nativeFilePicked(null, name, "too-large");
            return;
        }
        nativeFilePicked(out.getAbsolutePath(), name, null);
    }

    /**
     * The document's name, made safe for a file name, with an extension
     * from its type when it has none (the app tells images from other
     * files by extension).
     */
    private static String displayName(ContentResolver resolver, Uri uri) {
        String name = null;
        try (Cursor c = resolver.query(uri, new String[] {OpenableColumns.DISPLAY_NAME},
                null, null, null)) {
            if (c != null && c.moveToFirst()) {
                name = c.getString(0);
            }
        } catch (Exception e) {
            Log.w(TAG, "no display name for " + uri, e);
        }
        if (name == null || name.trim().isEmpty()) {
            name = "file";
        }
        name = name.replaceAll("[/\\\\:\\p{Cntrl}]", "_");
        if (name.lastIndexOf('.') <= 0) {
            String ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(resolver.getType(uri));
            if (ext != null) {
                name = name + "." + ext;
            }
        }
        return name;
    }

    /**
     * Rust: write {@code bytes} into the public Downloads (MediaStore,
     * no permission needed from API 29; the system renames on a clash).
     * Returns "uri\nDownload/shown-name", or "error\nmessage".
     */
    public String saveDownload(String name, String mime, byte[] bytes) {
        ContentResolver resolver = getContentResolver();
        Uri uri = null;
        try {
            ContentValues values = new ContentValues();
            values.put(MediaStore.MediaColumns.DISPLAY_NAME, name);
            values.put(MediaStore.MediaColumns.MIME_TYPE, downloadType(name, mime));
            values.put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS);
            values.put(MediaStore.MediaColumns.IS_PENDING, 1);
            uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values);
            if (uri == null) {
                return "error\nDownloads refused the file";
            }
            try (OutputStream os = resolver.openOutputStream(uri)) {
                if (os == null) {
                    throw new java.io.IOException("Downloads gave no stream");
                }
                os.write(bytes);
            }
            values.clear();
            values.put(MediaStore.MediaColumns.IS_PENDING, 0);
            resolver.update(uri, values, null, null);
            String shown = name;
            try (Cursor c = resolver.query(uri,
                    new String[] {MediaStore.MediaColumns.DISPLAY_NAME}, null, null, null)) {
                if (c != null && c.moveToFirst() && c.getString(0) != null) {
                    shown = c.getString(0);
                }
            }
            return uri + "\n" + Environment.DIRECTORY_DOWNLOADS + "/" + shown;
        } catch (Exception e) {
            Log.w(TAG, "saving to Downloads failed", e);
            if (uri != null) {
                try {
                    resolver.delete(uri, null, null);
                } catch (Exception ignored) {
                    // Nothing more to undo.
                }
            }
            return "error\n" + e.getMessage();
        }
    }

    /**
     * The type MediaStore files a download under. MediaProvider appends
     * the type's extension when the name's does not match it ("main.rs"
     * filed as text/plain becomes "main.rs.txt"), so the type comes from
     * the extension itself; one Android does not know is a plain binary,
     * which keeps the name as it is.
     */
    private static String downloadType(String name, String fallback) {
        int dot = name.lastIndexOf('.');
        if (dot > 0 && dot < name.length() - 1) {
            String ext = name.substring(dot + 1).toLowerCase(java.util.Locale.ROOT);
            String type = MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext);
            if (type != null) {
                return type;
            }
            return "application/octet-stream";
        }
        return fallback != null ? fallback : "application/octet-stream";
    }

    /** Rust: open a saved download with whatever handles its type. */
    public boolean openUri(String uriText, String mime) {
        Uri uri = Uri.parse(uriText);
        String type = getContentResolver().getType(uri);
        Intent intent = new Intent(Intent.ACTION_VIEW);
        intent.setDataAndType(uri, type != null ? type : mime);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            startActivity(intent);
            return true;
        } catch (ActivityNotFoundException e) {
            return false;
        }
    }

    /** Rust: whether a saved download is still there. */
    public boolean uriExists(String uriText) {
        try (Cursor c = getContentResolver().query(Uri.parse(uriText),
                new String[] {MediaStore.MediaColumns._ID}, null, null, null)) {
            return c != null && c.moveToFirst();
        } catch (Exception e) {
            return false;
        }
    }

    /** Rust: open a link in the browser. */
    public boolean openUrl(String url) {
        Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(url));
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            startActivity(intent);
            return true;
        } catch (ActivityNotFoundException e) {
            return false;
        }
    }
}
