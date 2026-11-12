package com.joshflix.tv;

import android.app.NativeActivity;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.graphics.PixelFormat;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.Process;
import android.util.Log;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
import android.widget.FrameLayout;
import android.widget.Toast;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;

/**
 * NativeActivity with two stacked surfaces instead of one window surface.
 *
 * <p>The bottom {@link SurfaceView} belongs to mpv, which draws video into it
 * on its own threads. The top one is translucent and carries the Rust UI:
 * NativeActivity's own surface callbacks are attached to it, so the native
 * side sees an ordinary window. Video therefore never passes through the UI's
 * OpenGL context, and losing the UI surface no longer stops the video.
 *
 * <p>If anything here fails, the activity keeps NativeActivity's single
 * window surface and the app draws video into the UI as before. A version of
 * the app stops layering for good ({@code single-surface} holds its version
 * code) when:
 * <ul>
 * <li>the UI never shows a frame on the layered surface within 15 s (the app
 *     then closes, so reopening it works);</li>
 * <li>the process died twice before the UI's first frame, or twice while mpv
 *     was starting on the video surface (attempt markers left behind);</li>
 * <li>the native side reports that video never appeared.</li>
 * </ul>
 * The user can also turn layering off in Settings ({@code single-surface-user}).
 *
 * <p>The static helpers called by the native side never lock
 * {@code MainActivity.class}: the native side calls them while
 * {@code surfaceDestroyed} may hold that lock and wait for it.
 */
public class MainActivity extends NativeActivity {
    private static final String TAG = "joshflix";
    /** Holds the version code whose layered surfaces failed. */
    private static final String SINGLE_SURFACE = "single-surface";
    /** Exists while the user turned layering off in Settings (written by the native side). */
    private static final String SINGLE_SURFACE_USER = "single-surface-user";
    /** Hold "versionCode pid" until the UI's first frame / until video showed. */
    private static final String UI_ATTEMPT = "ui-attempt";
    private static final String VIDEO_ATTEMPT = "video-surface-attempt";
    /** "versionCode count" of attempts that died, per kind. */
    private static final String UI_STRIKES = "ui-strikes";
    private static final String VIDEO_STRIKES = "video-surface-strikes";
    /** Attempts that may die before a version stops layering. */
    private static final int MAX_STRIKES = 2;
    /** How long the UI gets to draw its first frame on the layered surface. */
    private static final long UI_TIMEOUT_MS = 15_000;

    /** Layering state read by the native side: 0 pending, 1 layered, 2 single surface. */
    private static volatile int layerState = 0;
    private static volatile boolean uiShown;
    private static volatile File filesDir;
    private static volatile long appVersionCode;
    /** The activity whose surfaces the native side follows. */
    private static volatile MainActivity current;

    // Guarded by MainActivity.class.
    private static Surface videoSurface;
    private static int videoWidth;
    private static int videoHeight;
    private static boolean listening;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private FrameLayout layers;
    private SurfaceView videoView;
    private boolean resumed;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        layerState = 0;
        uiShown = false;
        current = this;
        synchronized (MainActivity.class) {
            // A previous activity in this process may have left these behind.
            listening = false;
            videoSurface = null;
        }
        filesDir = getFilesDir();
        appVersionCode = versionCode();
        boolean layer = decideLayering();
        if (layer) {
            writeAttempt(UI_ATTEMPT);
        }
        super.onCreate(savedInstanceState);
        if (!layer) {
            layerState = 2;
            return;
        }
        try {
            layers = new FrameLayout(this);
            videoView = newVideoView();
            SurfaceView uiView = new SurfaceView(this);
            uiView.setZOrderMediaOverlay(true);
            uiView.getHolder().setFormat(PixelFormat.TRANSLUCENT);
            layers.addView(videoView, matchParent());
            layers.addView(uiView, matchParent());
            // Hand NativeActivity the UI layer's surface instead of the window's.
            getWindow().takeSurface(null);
            uiView.getHolder().addCallback(this);
            addContentView(layers, matchParent());
            layerState = 1;
            handler.postDelayed(this::checkUiShown, UI_TIMEOUT_MS);
        } catch (Throwable t) {
            Log.e(TAG, "layered surfaces unavailable; video shares the UI surface", t);
            getWindow().takeSurface(this);
            deleteQuietly(UI_ATTEMPT);
            layerState = 2;
        }
    }

    /** Whether this launch layers surfaces, after counting attempts that died last time. */
    private boolean decideLayering() {
        if (new File(filesDir, SINGLE_SURFACE_USER).exists()) {
            Log.i(TAG, "layered surfaces turned off in Settings");
            return false;
        }
        countStrike(UI_ATTEMPT, UI_STRIKES, "before the UI's first frame");
        countStrike(VIDEO_ATTEMPT, VIDEO_STRIKES, "while starting video on its own surface");
        File marker = new File(filesDir, SINGLE_SURFACE);
        if (!marker.isFile()) {
            return true;
        }
        if (Long.toString(appVersionCode).equals(readMarker(SINGLE_SURFACE))) {
            Log.w(TAG, "layered surfaces failed in this version before; using a single surface");
            return false;
        }
        // A new version gets to try again.
        deleteQuietly(SINGLE_SURFACE);
        return true;
    }

    /**
     * A leftover attempt marker from another process of this version means
     * that process died during the attempt. (The same process can leave one
     * behind without dying, e.g. Back before the first frame.)
     */
    private static void countStrike(String attempt, String strikes, String when) {
        String version = Long.toString(appVersionCode);
        String[] marker = String.valueOf(readMarker(attempt)).split(" ");
        boolean died = marker.length == 2 && marker[0].equals(version)
                && !marker[1].equals(Integer.toString(Process.myPid()));
        if (died) {
            int count = 1;
            String[] previous = String.valueOf(readMarker(strikes)).split(" ");
            if (previous.length == 2 && previous[0].equals(version)) {
                try {
                    count += Integer.parseInt(previous[1]);
                } catch (NumberFormatException ignored) {
                    // Start counting again.
                }
            }
            Log.e(TAG, "the app died " + when + " (" + count + " of " + MAX_STRIKES + ")");
            writeFile(strikes, version + " " + count);
            if (count >= MAX_STRIKES) {
                writeMarker(SINGLE_SURFACE);
            }
        }
        deleteQuietly(attempt);
    }

    private static FrameLayout.LayoutParams matchParent() {
        return new FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT);
    }

    private SurfaceView newVideoView() {
        SurfaceView view = new SurfaceView(this);
        view.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {}

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                synchronized (MainActivity.class) {
                    if (!follows(view)) {
                        return;
                    }
                    videoSurface = holder.getSurface();
                    videoWidth = width;
                    videoHeight = height;
                    if (listening) {
                        nativeVideoSurface(videoSurface, width, height);
                    }
                }
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                // The native side detaches mpv before this returns: the surface
                // must not be drawn to afterwards.
                synchronized (MainActivity.class) {
                    if (!follows(view)) {
                        return;
                    }
                    videoSurface = null;
                    if (listening) {
                        nativeVideoSurface(null, 0, 0);
                    }
                }
            }
        });
        return view;
    }

    /** Only the current activity's current video view reports to the native side. */
    private boolean follows(SurfaceView view) {
        return current == this && view == videoView;
    }

    @Override
    protected void onResume() {
        super.onResume();
        resumed = true;
    }

    @Override
    protected void onPause() {
        resumed = false;
        super.onPause();
    }

    /** Called by the native side: 0 while onCreate runs, then 1 (layered) or 2 (single surface). */
    public static int layerState() {
        return layerState;
    }

    /**
     * Called by the native side once {@code nativeVideoSurface} is registered.
     * Reports the current video surface, if there is one, then every change.
     */
    public static void startVideoSurfaceUpdates() {
        synchronized (MainActivity.class) {
            listening = true;
            if (videoSurface != null) {
                nativeVideoSurface(videoSurface, videoWidth, videoHeight);
            }
        }
    }

    /** Called by the native side after it draws its first UI frame. */
    public static void uiShown() {
        uiShown = true;
        deleteQuietly(UI_ATTEMPT);
        deleteQuietly(UI_STRIKES);
    }

    /** Called by the native side right before mpv attaches to the video surface. */
    public static void layeredVideoStarting() {
        writeAttempt(VIDEO_ATTEMPT);
    }

    /** Called by the native side when mpv let go of the video surface cleanly. */
    public static void layeredVideoWorked() {
        deleteQuietly(VIDEO_ATTEMPT);
    }

    /** Called by the native side once video has played on its own surface for a while. */
    public static void layeredVideoConfirmed() {
        deleteQuietly(VIDEO_ATTEMPT);
        deleteQuietly(VIDEO_STRIKES);
    }

    /** Called by the native side when video never appeared on its own surface. */
    public static void layeringFailed() {
        writeMarker(SINGLE_SURFACE);
        deleteQuietly(VIDEO_ATTEMPT);
    }

    private void checkUiShown() {
        if (uiShown || isFinishing() || isDestroyed()) {
            return;
        }
        if (!resumed) {
            // Not on screen yet (or anymore): no frame is expected; check again later.
            handler.postDelayed(this::checkUiShown, UI_TIMEOUT_MS);
            return;
        }
        Log.e(TAG, "the UI never appeared on the layered surface; switching this version to a single surface");
        if (!writeMarker(SINGLE_SURFACE)) {
            return;
        }
        deleteQuietly(UI_ATTEMPT);
        Toast.makeText(this, "Joshflix hit a display problem and switched to its classic mode. Open it again.", Toast.LENGTH_LONG).show();
        // The native app can't start twice in one process: close it for good.
        handler.postDelayed(() -> {
            finishAndRemoveTask();
            Process.killProcess(Process.myPid());
        }, 3_000);
    }

    /**
     * Replace the video surface with a new one. Used when an mpv instance is
     * abandoned while it still holds the old surface.
     */
    public void recreateVideoSurface() {
        runOnUiThread(() -> {
            if (layers == null || videoView == null) {
                return;
            }
            SurfaceView old = videoView;
            // The old view's destroy callback is ignored from now on: the
            // native side has already given up that surface.
            videoView = newVideoView();
            layers.removeView(old);
            layers.addView(videoView, 0, matchParent());
        });
    }

    /** Write the app's version code to a marker file. */
    private static boolean writeMarker(String name) {
        return writeFile(name, Long.toString(appVersionCode));
    }

    /** "versionCode pid": a later process can tell this one died during the attempt. */
    private static void writeAttempt(String name) {
        writeFile(name, appVersionCode + " " + Process.myPid());
    }

    private static boolean writeFile(String name, String text) {
        File dir = filesDir;
        if (dir == null) {
            return false;
        }
        try (FileOutputStream out = new FileOutputStream(new File(dir, name))) {
            // No fsync: these only need to survive the process dying, which
            // the page cache does. They're written while mpv calls wait.
            out.write(text.getBytes(StandardCharsets.UTF_8));
            return true;
        } catch (Exception e) {
            Log.e(TAG, "couldn't write " + name, e);
            return false;
        }
    }

    /** The marker's text, or null when it doesn't exist or can't be read. */
    private static String readMarker(String name) {
        File dir = filesDir;
        File file = dir == null ? null : new File(dir, name);
        if (file == null || !file.isFile()) {
            return null;
        }
        try {
            return new String(Files.readAllBytes(file.toPath()), StandardCharsets.UTF_8).trim();
        } catch (Exception e) {
            Log.w(TAG, "couldn't read " + name, e);
            return null;
        }
    }

    private static void deleteQuietly(String name) {
        File dir = filesDir;
        if (dir == null) {
            return;
        }
        File file = new File(dir, name);
        if (file.exists() && !file.delete()) {
            Log.w(TAG, "couldn't delete " + name);
        }
    }

    @SuppressWarnings("deprecation")
    private long versionCode() {
        try {
            PackageInfo info = getPackageManager().getPackageInfo(getPackageName(), 0);
            return Build.VERSION.SDK_INT >= 28 ? info.getLongVersionCode() : info.versionCode;
        } catch (PackageManager.NameNotFoundException e) {
            return -1;
        }
    }

    /** {@code surface} is null when the video surface is gone. Registered by the native side. */
    private static native void nativeVideoSurface(Surface surface, int width, int height);
}
