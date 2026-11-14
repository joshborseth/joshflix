package com.joshflix.tv;

import android.app.NativeActivity;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.graphics.Color;
import android.graphics.PixelFormat;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.os.Process;
import android.util.Log;
import android.view.Gravity;
import android.view.PixelCopy;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewGroup;
import android.widget.FrameLayout;
import android.widget.Toast;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * NativeActivity with stacked surfaces instead of one window surface.
 *
 * <p>At the bottom are two video {@link SurfaceView}s, of which one is shown
 * at a time: ExoPlayer's ({@link ExoBridge}), where MediaCodec puts frames
 * straight onto the TV's video layer, and mpv's, which mpv draws into on its
 * own threads (the fallback player). The next surface is translucent and
 * carries the Rust UI: NativeActivity's own surface callbacks are attached
 * to it, so the native side sees an ordinary window. ExoPlayer's subtitles
 * ({@link SubtitleOverlay}) are drawn in the window itself, above all of
 * these surfaces. Video
 * therefore never passes through the UI's OpenGL context, and losing the UI
 * surface no longer stops the video.
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
 * ExoPlayer needs the layers. A version stops using it ({@code exo-off}) when
 * the process died twice while ExoPlayer started a video.
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
    /** "versionCode pid" until ExoPlayer showed its first frame in this process. */
    private static final String EXO_ATTEMPT = "exo-attempt";
    private static final String EXO_STRIKES = "exo-strikes";
    /** Holds the version code whose ExoPlayer starts kept killing the app. */
    private static final String EXO_OFF = "exo-off";
    /** Which video layer shows (showVideoLayer). */
    private static final int LAYER_MPV = 1;
    private static final int LAYER_EXO = 2;
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
    /** ExoPlayer showed a frame in this process: its attempt marker is gone. */
    private static volatile boolean exoWorked;
    /** The video layer on screen: LAYER_EXO or LAYER_MPV. */
    private static volatile int videoLayer = LAYER_EXO;

    // Guarded by MainActivity.class.
    private static Surface videoSurface;
    private static int videoWidth;
    private static int videoHeight;
    private static boolean listening;

    /**
     * Surfaces for {@link #sampleScreen}: mpv's video surface, and the UI's
     * (NativeActivity's). Cleared under {@link #SAMPLE_LOCK} before a surface
     * is released, so no copy reads a released surface.
     */
    private static volatile Surface sampleVideoSurface;
    private static volatile Surface sampleExoSurface;
    private static volatile Surface sampleUiSurface;
    /** Held around each copy and while a sampled surface goes away. Never with MainActivity.class inside. */
    private static final Object SAMPLE_LOCK = new Object();
    /** Runs PixelCopy callbacks; created on first use. */
    private static final Object COPY_LOCK = new Object();
    private static Handler copyHandler;
    /** How long {@link #sampleScreen} waits for a copy. */
    private static final long COPY_TIMEOUT_MS = 500;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private FrameLayout layers;
    private SurfaceView videoView;
    private SurfaceView exoView;
    private SubtitleOverlay subtitles;
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
        sampleVideoSurface = null;
        sampleExoSurface = null;
        sampleUiSurface = null;
        videoLayer = LAYER_EXO;
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
            exoView = newExoView();
            subtitles = new SubtitleOverlay(this);
            SurfaceView uiView = new SurfaceView(this);
            uiView.setZOrderMediaOverlay(true);
            uiView.getHolder().setFormat(PixelFormat.TRANSLUCENT);
            // mpv's view joins only when the native side switches to mpv:
            // a hidden SurfaceView has no surface for anything to attach to.
            videoView.setVisibility(View.GONE);
            layers.addView(videoView, matchParent());
            layers.addView(exoView, matchParent());
            layers.addView(uiView, matchParent());
            // Last: every SurfaceView here sits below the window and clears
            // its rectangle in it, so anything drawn before the UI's
            // full-screen one would be erased.
            layers.addView(subtitles, matchParent());
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
        countStrike(UI_ATTEMPT, UI_STRIKES, SINGLE_SURFACE, "before the UI's first frame");
        countStrike(VIDEO_ATTEMPT, VIDEO_STRIKES, SINGLE_SURFACE, "while starting video on its own surface");
        countStrike(EXO_ATTEMPT, EXO_STRIKES, EXO_OFF, "while ExoPlayer started a video");
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
    private static void countStrike(String attempt, String strikes, String marker, String when) {
        String version = Long.toString(appVersionCode);
        String[] previousAttempt = String.valueOf(readMarker(attempt)).split(" ");
        boolean died = previousAttempt.length == 2 && previousAttempt[0].equals(version)
                && !previousAttempt[1].equals(Integer.toString(Process.myPid()));
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
                writeMarker(marker);
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
                    sampleVideoSurface = videoSurface;
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
                    synchronized (SAMPLE_LOCK) {
                        sampleVideoSurface = null;
                    }
                    if (listening) {
                        nativeVideoSurface(null, 0, 0);
                    }
                }
            }
        });
        return view;
    }

    /** ExoPlayer's video view. ExoPlayer follows its surface itself. */
    private SurfaceView newExoView() {
        SurfaceView view = new SurfaceView(this);
        view.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {}

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                if (current == MainActivity.this) {
                    sampleExoSurface = holder.getSurface();
                }
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                if (current == MainActivity.this) {
                    synchronized (SAMPLE_LOCK) {
                        sampleExoSurface = null;
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

    // The UI's surface (the layered UI view, or the window), for sampleScreen.
    // Recorded before NativeActivity sees each change.

    @Override
    public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
        if (current == this) {
            sampleUiSurface = holder.getSurface();
        }
        super.surfaceChanged(holder, format, width, height);
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        if (current == this) {
            synchronized (SAMPLE_LOCK) {
                sampleUiSurface = null;
            }
        }
        super.surfaceDestroyed(holder);
    }

    /**
     * Called by the native side, off the main thread: the picture on screen
     * scaled down to {@code width} x {@code height}, as ARGB pixels, or null
     * when it can't be read right now. Reads mpv's own surface when the
     * native side says video is drawn there ({@code videoSurface}), otherwise
     * the UI surface that video is drawn into. PixelCopy scales on the GPU, so
     * this costs a few small reads per call.
     */
    public static int[] sampleScreen(int width, int height, boolean videoSurface) {
        if (Build.VERSION.SDK_INT < 24 || width <= 0 || height <= 0) {
            return null;
        }
        try {
            Bitmap bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888);
            CountDownLatch done = new CountDownLatch(1);
            int[] result = {PixelCopy.ERROR_UNKNOWN};
            // Until the copy is done, the surface can't be released under it
            // (surfaceDestroyed waits for this lock; at most COPY_TIMEOUT_MS).
            synchronized (SAMPLE_LOCK) {
                Surface layer = videoLayer == LAYER_EXO ? sampleExoSurface : sampleVideoSurface;
                Surface source = videoSurface && layerState == 1 ? layer : sampleUiSurface;
                if (source == null || !source.isValid()) {
                    return null;
                }
                PixelCopy.request(source, bitmap, status -> {
                    result[0] = status;
                    done.countDown();
                }, copyHandler());
                if (!done.await(COPY_TIMEOUT_MS, TimeUnit.MILLISECONDS) || result[0] != PixelCopy.SUCCESS) {
                    return null;
                }
            }
            int[] pixels = new int[width * height];
            bitmap.getPixels(pixels, 0, width, 0, 0, width, height);
            bitmap.recycle();
            return pixels;
        } catch (Throwable t) {
            // The surface went away mid-copy, or this device can't copy it.
            return null;
        }
    }

    private static Handler copyHandler() {
        synchronized (COPY_LOCK) {
            if (copyHandler == null) {
                HandlerThread thread = new HandlerThread("screen-copy");
                thread.start();
                copyHandler = new Handler(thread.getLooper());
            }
            return copyHandler;
        }
    }

    // ---- ExoPlayer ----

    /**
     * Called by the native side: ExoPlayer may play here. It needs the
     * layered surfaces, and a version whose ExoPlayer starts kept killing the
     * app uses mpv instead.
     */
    public static boolean exoAllowed() {
        if (layerState != 1) {
            return false;
        }
        if (Long.toString(appVersionCode).equals(readMarker(EXO_OFF))) {
            Log.w(TAG, "ExoPlayer failed in this version before; using mpv");
            return false;
        }
        return true;
    }

    /**
     * Called by the native side (any thread): show ExoPlayer's video layer
     * ({@code 2}) or mpv's ({@code 1}). Hiding mpv's view destroys its
     * surface, which detaches mpv through the usual callback.
     */
    public static void showVideoLayer(int which) {
        videoLayer = which == LAYER_MPV ? LAYER_MPV : LAYER_EXO;
        MainActivity activity = current;
        if (activity == null) {
            return;
        }
        activity.runOnUiThread(() -> {
            if (activity.videoView == null || activity.exoView == null) {
                return;
            }
            boolean mpv = videoLayer == LAYER_MPV;
            activity.exoView.setVisibility(mpv ? View.GONE : View.VISIBLE);
            activity.videoView.setVisibility(mpv ? View.VISIBLE : View.GONE);
            activity.subtitles.setVisibility(mpv ? View.GONE : View.VISIBLE);
        });
    }

    /** Main thread (ExoBridge): ExoPlayer's video surface, or null without layers. */
    static SurfaceHolder exoSurfaceHolder() {
        MainActivity activity = current;
        return activity == null || activity.exoView == null ? null : activity.exoView.getHolder();
    }

    /** Main thread (ExoBridge): where ExoPlayer's subtitles go. */
    static SubtitleOverlay subtitleOverlay() {
        MainActivity activity = current;
        return activity == null ? null : activity.subtitles;
    }

    /**
     * Main thread (ExoBridge): a video is about to start. Letterbox bars are
     * black (not the window's colour), and until the process has seen
     * ExoPlayer show a frame, a marker records the attempt.
     */
    static void exoPlaybackStarting() {
        MainActivity activity = current;
        if (activity != null && activity.layers != null) {
            activity.layers.setBackgroundColor(Color.BLACK);
        }
        if (!exoWorked) {
            writeAttempt(EXO_ATTEMPT);
        }
    }

    /** Main thread (ExoBridge): ExoPlayer showed a frame. */
    static void exoFrameShown() {
        if (!exoWorked) {
            exoWorked = true;
            deleteQuietly(EXO_ATTEMPT);
            deleteQuietly(EXO_STRIKES);
        }
    }

    /** Main thread (ExoBridge): playback stopped. */
    static void exoPlaybackEnded() {
        MainActivity activity = current;
        if (activity == null || activity.layers == null) {
            return;
        }
        activity.layers.setBackground(null);
        if (activity.exoView != null) {
            activity.exoView.setLayoutParams(matchParent());
        }
        if (!exoWorked) {
            // Stopped before a frame (Back, or a failure handled by the app): not a crash.
            deleteQuietly(EXO_ATTEMPT);
        }
    }

    /**
     * Main thread (ExoBridge): size ExoPlayer's view to the video's display
     * aspect ratio, centred. MediaCodec scales frames to the whole surface,
     * so a full-screen view would stretch anything that isn't 16:9.
     */
    static void fitExoVideo(float aspect) {
        MainActivity activity = current;
        if (activity == null || activity.exoView == null || activity.layers == null || !(aspect > 0)) {
            return;
        }
        int width = activity.layers.getWidth();
        int height = activity.layers.getHeight();
        if (width == 0 || height == 0) {
            return;
        }
        int w = width;
        int h = Math.round(width / aspect);
        if (h > height) {
            h = height;
            w = Math.round(height * aspect);
        }
        // Within a pixel or two of the screen: fill it (no hairline bars).
        if (Math.abs(w - width) <= 2 && Math.abs(h - height) <= 2) {
            w = width;
            h = height;
        }
        FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(w, h, Gravity.CENTER);
        ViewGroup.LayoutParams old = activity.exoView.getLayoutParams();
        if (old == null || old.width != w || old.height != h) {
            activity.exoView.setLayoutParams(params);
        }
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
