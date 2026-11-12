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
 * window surface and the app draws video into the UI as before. This
 * version of the app stops layering for good (the {@code single-surface}
 * marker) when:
 * <ul>
 * <li>the UI never shows a frame on the layered surface (it then closes, so
 *     reopening it works);</li>
 * <li>the process died while mpv was starting on the video surface and before
 *     video showed ({@code video-surface-attempt} was left behind);</li>
 * <li>the native side reports that video never appeared.</li>
 * </ul>
 *
 * <p>The static helpers called by the native side never lock
 * {@code MainActivity.class}: the native side calls them while
 * {@code surfaceDestroyed} may hold that lock and wait for it.
 */
public class MainActivity extends NativeActivity {
    private static final String TAG = "joshflix";
    /** Holds the version code whose layered surfaces failed. */
    private static final String SINGLE_SURFACE_MARKER = "single-surface";
    /** Holds the version code while mpv starts on the video surface. */
    private static final String ATTEMPT_MARKER = "video-surface-attempt";
    /** How long the UI gets to draw its first frame on the layered surface. */
    private static final long UI_TIMEOUT_MS = 15_000;

    private static volatile boolean uiShown;
    private static volatile File filesDir;
    private static volatile long appVersionCode;

    /** Layering state read by the native side: 0 pending, 1 layered, 2 single surface. */
    private static volatile int layerState = 0;

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
        synchronized (MainActivity.class) {
            // A previous activity in this process may have left these behind.
            listening = false;
            videoSurface = null;
        }
        filesDir = getFilesDir();
        appVersionCode = versionCode();
        File attempt = new File(filesDir, ATTEMPT_MARKER);
        if (attempt.isFile() && markerMatchesVersion(attempt)) {
            Log.e(TAG, "the app died while starting video on its own surface; using a single surface");
            writeMarker(SINGLE_SURFACE_MARKER);
        }
        deleteQuietly(attempt);
        super.onCreate(savedInstanceState);
        if (layeringFailedBefore()) {
            Log.w(TAG, "layered surfaces failed in this version before; using a single surface");
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
            layerState = 2;
        }
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

    /** Called by the native side after it draws its first UI frame. */
    public static void uiShown() {
        uiShown = true;
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
        if (!writeMarker(SINGLE_SURFACE_MARKER)) {
            return;
        }
        Toast.makeText(this, "Joshflix hit a display problem and switched to its classic mode. Open it again.", Toast.LENGTH_LONG).show();
        // The native app can't start twice in one process: close it for good.
        handler.postDelayed(() -> {
            finishAndRemoveTask();
            Process.killProcess(Process.myPid());
        }, 3_000);
    }

    /** Called by the native side right before mpv attaches to the video surface. */
    public static void layeredVideoStarting() {
        writeMarker(ATTEMPT_MARKER);
    }

    /** Called by the native side once video showed, or mpv let go of the surface cleanly. */
    public static void layeredVideoWorked() {
        File dir = filesDir;
        if (dir != null) {
            deleteQuietly(new File(dir, ATTEMPT_MARKER));
        }
    }

    /** Called by the native side when video never appeared on its own surface. */
    public static void layeringFailed() {
        writeMarker(SINGLE_SURFACE_MARKER);
        layeredVideoWorked();
    }

    private boolean layeringFailedBefore() {
        File marker = new File(filesDir, SINGLE_SURFACE_MARKER);
        if (!marker.isFile()) {
            return false;
        }
        if (markerMatchesVersion(marker)) {
            return true;
        }
        // A new version gets to try again.
        deleteQuietly(marker);
        return false;
    }

    /** Write the app's version code to a marker file. */
    private static boolean writeMarker(String name) {
        File dir = filesDir;
        if (dir == null) {
            return false;
        }
        try (FileOutputStream out = new FileOutputStream(new File(dir, name))) {
            out.write(String.valueOf(appVersionCode).getBytes(StandardCharsets.UTF_8));
            out.getFD().sync();
            return true;
        } catch (Exception e) {
            Log.e(TAG, "couldn't write " + name, e);
            return false;
        }
    }

    private static boolean markerMatchesVersion(File marker) {
        try {
            String version = new String(Files.readAllBytes(marker.toPath()), StandardCharsets.UTF_8).trim();
            return version.equals(String.valueOf(appVersionCode));
        } catch (Exception e) {
            Log.w(TAG, "couldn't read " + marker.getName(), e);
            return false;
        }
    }

    private static void deleteQuietly(File file) {
        if (file.exists() && !file.delete()) {
            Log.w(TAG, "couldn't delete " + file.getName());
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
                    videoSurface = null;
                    if (listening) {
                        nativeVideoSurface(null, 0, 0);
                    }
                }
            }
        });
        return view;
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

    /**
     * Replace the video surface with a new one. Used when an mpv instance is
     * abandoned while it still holds the old surface.
     */
    public void recreateVideoSurface() {
        runOnUiThread(() -> {
            if (layers == null || videoView == null) {
                return;
            }
            layers.removeView(videoView);
            videoView = newVideoView();
            layers.addView(videoView, 0, matchParent());
        });
    }

    /** {@code surface} is null when the video surface is gone. Registered by the native side. */
    private static native void nativeVideoSurface(Surface surface, int width, int height);
}
