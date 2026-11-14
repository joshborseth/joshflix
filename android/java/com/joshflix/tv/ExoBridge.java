package com.joshflix.tv;

import android.content.Context;
import android.media.AudioFormat;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.view.SurfaceHolder;

import androidx.media3.common.C;
import androidx.media3.common.ColorInfo;
import androidx.media3.common.Format;
import androidx.media3.common.MediaItem;
import androidx.media3.common.MimeTypes;
import androidx.media3.common.PlaybackException;
import androidx.media3.common.Player;
import androidx.media3.common.TrackSelectionOverride;
import androidx.media3.common.Tracks;
import androidx.media3.common.VideoSize;
import androidx.media3.common.text.Cue;
import androidx.media3.common.text.CueGroup;
import androidx.media3.datasource.DefaultDataSource;
import androidx.media3.datasource.DefaultHttpDataSource;
import androidx.media3.exoplayer.DecoderCounters;
import androidx.media3.exoplayer.DecoderReuseEvaluation;
import androidx.media3.exoplayer.DefaultLoadControl;
import androidx.media3.exoplayer.DefaultRenderersFactory;
import androidx.media3.exoplayer.ExoPlayer;
import androidx.media3.exoplayer.analytics.AnalyticsListener;
import androidx.media3.exoplayer.audio.AudioCapabilities;
import androidx.media3.exoplayer.audio.AudioSink;
import androidx.media3.exoplayer.audio.DefaultAudioSink;
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory;
import androidx.media3.exoplayer.trackselection.DefaultTrackSelector;
import androidx.media3.extractor.DefaultExtractorsFactory;
import androidx.media3.extractor.text.CuesWithTiming;
import androidx.media3.extractor.text.SubtitleParser;
import androidx.media3.extractor.text.ssa.SsaParser;
import androidx.media3.extractor.text.subrip.SubripParser;
import androidx.media3.extractor.text.webvtt.WebvttParser;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.Charset;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Media3 ExoPlayer for the Rust side ({@code crates/joshflix-app/src/exo.rs}).
 *
 * <p>Video goes straight from MediaCodec to the TV's video layer through
 * {@code MainActivity}'s ExoPlayer {@code SurfaceView}: no copy and no OpenGL
 * work per frame. The TV's own decoders handle Dolby Vision, HDR10 and
 * HDR10+, and audio passes through to the TV or a soundbar when it can take
 * the format. Audio nothing on the device can decode (TrueHD, DTS on most TVs)
 * falls back to Media3's FFmpeg decoder.
 *
 * <p>Every method the Rust side calls returns at once: the work is posted to
 * the main thread, which is ExoPlayer's application thread. Events go back
 * through {@link #nativeEvent}, tagged with the handle the Rust side gave
 * {@link #create}. Subtitles from OpenSubtitles are downloaded and parsed
 * here and shown in sync with playback, since ExoPlayer can't add a
 * subtitle file to media that is already playing.
 */
public final class ExoBridge {
    private static final String TAG = "joshflix";

    // Event types for nativeEvent. Keep in sync with src/exo.rs.
    static final int EV_LOADED = 1;
    static final int EV_STARTED = 2;
    /** a = position in seconds, b = duration in seconds (negative while unknown). */
    static final int EV_PROGRESS = 3;
    /** a = seconds buffered ahead of the position. */
    static final int EV_CACHE = 4;
    /** a = 1 paused, 0 playing. */
    static final int EV_PAUSED = 5;
    /** a = percent buffered, or -1 once playing again. */
    static final int EV_BUFFERING = 6;
    /** text = the track list as JSON. */
    static final int EV_TRACKS = 7;
    /** a = frames per second. */
    static final int EV_FRAME_RATE = 8;
    /** a = width, b = height. */
    static final int EV_VIDEO_SIZE = 9;
    /** a = frames dropped late, b = frames dropped by the decoder (totals for this file). */
    static final int EV_DROPS = 10;
    static final int EV_ENDED = 11;
    /** a = error kind (ERR_*), text = description. */
    static final int EV_ERROR = 12;

    /** The stream or network failed: reopening the same link may work. */
    static final int ERR_NETWORK = 1;
    /** ExoPlayer can't play this file here; mpv may. */
    static final int ERR_UNSUPPORTED = 2;

    // Log levels for nativeLog.
    private static final int LOG_INFO = 0;
    private static final int LOG_WARN = 1;
    private static final int LOG_ERROR = 2;

    /** Track ids: embedded tracks are (group + 1) * 1000 + index; files we add start here. */
    private static final int EXTERNAL_BASE = 1_000_000;
    private static final int KIND_VIDEO = 0;
    private static final int KIND_AUDIO = 1;
    private static final int KIND_TEXT = 2;

    private static final long PROGRESS_MS = 250;
    private static final long SUBTITLE_TICK_MS = 50;
    /** Media buffered before playback starts or resumes (ms). */
    private static final int START_BUFFER_MS = 1500;
    private static final int REBUFFER_MS = 3000;
    private static final int MIN_BUFFER_MS = 30_000;
    private static final int MAX_BUFFER_MS = 60_000;
    private static final int MAX_SUBTITLE_BYTES = 8 << 20;
    /** Frames whose timestamps estimate the frame rate. */
    private static final int FPS_SAMPLES = 48;

    private static native void nativeEvent(long handle, int type, double a, double b, String text);

    private static native void nativeLog(int level, String text);

    private static final ExecutorService DOWNLOADS = Executors.newSingleThreadExecutor(r -> {
        Thread thread = new Thread(r, "subtitle-download");
        thread.setDaemon(true);
        return thread;
    });

    private final long handle;
    private final Context context;
    private final int audioSession;
    private final String userAgent;
    private final Handler main = new Handler(Looper.getMainLooper());

    // Main thread only from here on.
    private ExoPlayer player;
    private DefaultTrackSelector trackSelector;
    private String audioLanguage;
    private boolean pcmOnly;
    /** {@link #pcmOnly} as it was when the current player was built. */
    private boolean builtPcmOnly;
    private float subtitleScale = 1f;
    private boolean subtitlesLifted;
    private boolean subtitlesHidden;
    private boolean released;

    /** The current file; bumped by every load and stop, so late work can tell it's stale. */
    private int generation;
    private String url;
    private long startMs;
    private boolean loadedSent;
    private boolean firstFrameSent;
    private boolean seeking;
    private boolean buffering;
    private boolean ended;
    private boolean errorSent;
    private int lastDropsOutput = -1;
    private int lastDropsDecoder = -1;
    private float frameRateSent;
    private int lastAudioEncoding = C.ENCODING_INVALID;
    private String lastAudioOutput;
    private String lastTracksJson;
    private final List<External> externals = new ArrayList<>();
    private int selectedExternal = -1;
    private List<Cue> embeddedCues = Collections.emptyList();

    /** Frame timestamps for estimating the frame rate when the file doesn't say. */
    private final long[] frameTimes = new long[FPS_SAMPLES];
    /** Written on ExoPlayer's playback thread; reset on the main thread for each file. */
    private volatile int frameCount;
    private volatile int frameGeneration;

    private final Runnable progressTick = this::progressTick;
    private final Runnable subtitleTick = this::subtitleTick;

    /** A subtitle file from OpenSubtitles, parsed. */
    private static final class External {
        final String title;
        final String language;
        final List<CuesWithTiming> cues;

        External(String title, String language, List<CuesWithTiming> cues) {
            this.title = title;
            this.language = language;
            this.cues = cues;
        }
    }

    private ExoBridge(long handle, Context context, int audioSession, String userAgent) {
        this.handle = handle;
        this.context = context.getApplicationContext();
        this.audioSession = audioSession;
        this.userAgent = userAgent;
    }

    /** Called by the native side. {@code audioSession} 0 lets Android pick one. */
    public static ExoBridge create(long handle, Context context, int audioSession, String userAgent) {
        return new ExoBridge(handle, context, audioSession, userAgent);
    }

    // ---- calls from the native side (any thread) ----

    public void load(String url, long startMs, String audioLanguage) {
        post(() -> {
            this.audioLanguage = audioLanguage == null || audioLanguage.isEmpty() ? null : audioLanguage;
            open(url, startMs);
        });
    }

    public void stop() {
        post(() -> {
            generation++;
            url = null;
            main.removeCallbacks(progressTick);
            main.removeCallbacks(subtitleTick);
            externals.clear();
            selectedExternal = -1;
            embeddedCues = Collections.emptyList();
            showCues(Collections.emptyList());
            if (player != null) {
                player.stop();
                player.clearMediaItems();
            }
            MainActivity.exoPlaybackEnded();
        });
    }

    public void setPaused(boolean paused) {
        post(() -> {
            if (player != null) {
                player.setPlayWhenReady(!paused);
            }
        });
    }

    public void togglePause() {
        post(() -> {
            if (player != null) {
                player.setPlayWhenReady(!player.getPlayWhenReady());
            }
        });
    }

    public void seekTo(long positionMs) {
        post(() -> seekInternal(positionMs));
    }

    public void seekBy(long deltaMs) {
        post(() -> {
            if (player != null) {
                seekInternal(player.getCurrentPosition() + deltaMs);
            }
        });
    }

    /** {@code kind}: 0 video, 1 audio, 2 subtitles. {@code id} &lt; 0 turns subtitles off. */
    public void selectTrack(int kind, int id) {
        post(() -> selectInternal(kind, id));
    }

    /** Download, parse and add a subtitle file without selecting it. */
    public void addSubtitle(String subtitleUrl, String title, String language) {
        post(() -> {
            int file = generation;
            DOWNLOADS.execute(() -> {
                List<CuesWithTiming> cues = null;
                try {
                    cues = downloadSubtitle(subtitleUrl);
                } catch (Throwable t) {
                    log(LOG_WARN, "subtitle " + title + " (" + language + "): " + t);
                }
                List<CuesWithTiming> parsed = cues;
                post(() -> {
                    if (file != generation || parsed == null || parsed.isEmpty()) {
                        return;
                    }
                    externals.add(new External(title, language, parsed));
                    sendTracks(true);
                });
            });
        });
    }

    public void setSubtitleScale(float scale) {
        post(() -> {
            subtitleScale = scale;
            SubtitleOverlay overlay = MainActivity.subtitleOverlay();
            if (overlay != null) {
                overlay.setScale(scale);
            }
        });
    }

    /** Hide subtitles while a menu or an error shows: they draw above the UI. */
    public void setSubtitlesHidden(boolean hidden) {
        post(() -> {
            subtitlesHidden = hidden;
            SubtitleOverlay overlay = MainActivity.subtitleOverlay();
            if (overlay != null) {
                overlay.setHidden(hidden);
            }
        });
    }

    public void setSubtitlesLifted(boolean lifted) {
        post(() -> {
            subtitlesLifted = lifted;
            SubtitleOverlay overlay = MainActivity.subtitleOverlay();
            if (overlay != null) {
                overlay.setLifted(lifted);
            }
        });
    }

    /**
     * Decode all audio to PCM (Audio Normalization's effect only sees PCM)
     * instead of passing Dolby or DTS through. Takes effect at once when the
     * current audio is passed through.
     */
    public void setPcmOnly(boolean on) {
        post(() -> {
            if (pcmOnly == on) {
                return;
            }
            pcmOnly = on;
            if (player == null) {
                return;
            }
            if (on && url != null && isPassthrough(lastAudioEncoding)) {
                rebuild("audio normalization needs decoded audio");
            }
            // Otherwise the next file gets a player built for the new setting.
        });
    }

    public void release() {
        post(() -> {
            released = true;
            generation++;
            main.removeCallbacks(progressTick);
            main.removeCallbacks(subtitleTick);
            releasePlayer();
        });
    }

    // ---- main thread ----

    private void post(Runnable work) {
        main.post(() -> {
            if (!released) {
                safely(work);
            }
        });
    }

    /**
     * Runs bridge code on the main thread. An exception here would crash the
     * app; instead the current file fails over to mpv.
     */
    private void safely(Runnable work) {
        try {
            work.run();
        } catch (Throwable t) {
            Log.e(TAG, "ExoPlayer bridge", t);
            try {
                fail(ERR_UNSUPPORTED, "ExoPlayer bridge: " + t);
            } catch (Throwable ignored) {
                // Nothing more to do.
            }
        }
    }

    private void open(String url, long startMs) {
        generation++;
        this.url = url;
        this.startMs = Math.max(0, startMs);
        loadedSent = false;
        firstFrameSent = false;
        seeking = false;
        buffering = false;
        ended = false;
        errorSent = false;
        lastDropsOutput = -1;
        lastDropsDecoder = -1;
        frameRateSent = 0;
        frameGeneration++;
        frameCount = 0;
        lastTracksJson = null;
        externals.clear();
        selectedExternal = -1;
        embeddedCues = Collections.emptyList();
        showCues(Collections.emptyList());
        if (player != null && builtPcmOnly != pcmOnly) {
            releasePlayer();
        }
        lastAudioEncoding = C.ENCODING_INVALID;
        lastAudioOutput = null;
        ensurePlayer();
        trackSelector.setParameters(trackSelector.buildUponParameters()
                .clearOverridesOfType(C.TRACK_TYPE_AUDIO)
                .clearOverridesOfType(C.TRACK_TYPE_TEXT)
                .clearOverridesOfType(C.TRACK_TYPE_VIDEO)
                .setPreferredAudioLanguage(audioLanguage)
                // The Rust side picks subtitles once it knows every track.
                .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true));
        MainActivity.exoPlaybackStarting();
        attachSurface();
        player.setMediaItem(new MediaItem.Builder().setUri(url).build(), this.startMs);
        player.prepare();
        player.setPlayWhenReady(true);
        main.removeCallbacks(progressTick);
        main.post(progressTick);
    }

    private void ensurePlayer() {
        if (player != null) {
            return;
        }
        boolean pcm = pcmOnly;
        builtPcmOnly = pcm;
        DefaultRenderersFactory renderers = new DefaultRenderersFactory(context) {
            @Override
            @SuppressWarnings("deprecation") // setAudioCapabilities: the only way to force PCM.
            protected AudioSink buildAudioSink(Context ctx, boolean floatOutput, boolean playbackParams) {
                DefaultAudioSink.Builder sink = new DefaultAudioSink.Builder(ctx)
                        .setEnableFloatOutput(floatOutput)
                        .setEnableAudioTrackPlaybackParams(playbackParams);
                if (pcm) {
                    sink.setAudioCapabilities(AudioCapabilities.DEFAULT_AUDIO_CAPABILITIES);
                }
                return sink.build();
            }
        };
        // FFmpeg after the TV's own decoders: only for what nothing else plays.
        renderers.setExtensionRendererMode(DefaultRenderersFactory.EXTENSION_RENDERER_MODE_ON);
        renderers.setEnableDecoderFallback(true);

        trackSelector = new DefaultTrackSelector(context);
        trackSelector.setParameters(trackSelector.buildUponParameters().setTunnelingEnabled(false));

        // Buffers live on the Java heap: a quarter of it, between 32 and 160 MB.
        long heap = Runtime.getRuntime().maxMemory();
        int targetBytes = (int) Math.max(32L << 20, Math.min(160L << 20, heap / 4));
        DefaultLoadControl loadControl = new DefaultLoadControl.Builder()
                .setBufferDurationsMs(MIN_BUFFER_MS, MAX_BUFFER_MS, START_BUFFER_MS, REBUFFER_MS)
                .setTargetBufferBytes(targetBytes)
                .setPrioritizeTimeOverSizeThresholds(false)
                .build();

        DefaultHttpDataSource.Factory http = new DefaultHttpDataSource.Factory()
                .setUserAgent(userAgent)
                .setConnectTimeoutMs(30_000)
                .setReadTimeoutMs(30_000)
                .setAllowCrossProtocolRedirects(true);
        DefaultExtractorsFactory extractors = new DefaultExtractorsFactory().setConstantBitrateSeekingEnabled(true);
        DefaultMediaSourceFactory sources =
                new DefaultMediaSourceFactory(new DefaultDataSource.Factory(context, http), extractors);

        player = new ExoPlayer.Builder(context, renderers)
                .setTrackSelector(trackSelector)
                .setLoadControl(loadControl)
                .setMediaSourceFactory(sources)
                .setLooper(Looper.getMainLooper())
                .build();
        // The app votes for the video's frame rate on its own window (with a
        // mode switch when needed); ExoPlayer would only ask for seamless ones.
        player.setVideoChangeFrameRateStrategy(C.VIDEO_CHANGE_FRAME_RATE_STRATEGY_OFF);
        if (audioSession != 0) {
            // Audio Normalization's effect is attached to this session.
            player.setAudioSessionId(audioSession);
        }
        player.addListener(listener);
        player.addAnalyticsListener(analytics);
        // Called on ExoPlayer's playback thread for every frame it renders.
        player.setVideoFrameMetadataListener((presentationTimeUs, releaseTimeNs, format, mediaFormat) -> {
            int file = frameGeneration;
            if (frameCount < 0 || frameCount >= FPS_SAMPLES) {
                return;
            }
            frameTimes[frameCount++] = presentationTimeUs;
            if (frameCount == FPS_SAMPLES) {
                frameCount = -1;
                float fps = estimateFrameRate(frameTimes);
                main.post(() -> {
                    if (file == frameGeneration && frameRateSent <= 0 && fps > 0) {
                        frameRateSent = fps;
                        log(LOG_INFO, String.format(Locale.ROOT, "exo: frame rate %.3f fps (from frame timestamps)", fps));
                        nativeEvent(handle, EV_FRAME_RATE, fps, 0, null);
                    }
                });
            }
        });
        log(LOG_INFO, "exo: player ready (Media3 1.11.1), buffer up to " + (targetBytes >> 20) + " MB of a "
                + (heap >> 20) + " MB heap" + (pcm ? ", audio decoded to PCM" : ", passthrough allowed")
                + ", FFmpeg audio " + (androidx.media3.decoder.ffmpeg.FfmpegLibrary.isAvailable()
                        ? androidx.media3.decoder.ffmpeg.FfmpegLibrary.getVersion() : "unavailable"));
    }

    private void releasePlayer() {
        if (player != null) {
            player.removeListener(listener);
            player.removeAnalyticsListener(analytics);
            player.release();
            player = null;
            trackSelector = null;
        }
    }

    /** A new player for the current file, where it is, keeping the user's track choices. */
    private void rebuild(String why) {
        if (player == null || url == null) {
            return;
        }
        log(LOG_INFO, "exo: rebuilding the player: " + why);
        long position = player.getCurrentPosition();
        boolean playWhenReady = player.getPlayWhenReady();
        DefaultTrackSelector.Parameters parameters = trackSelector.getParameters();
        List<External> keep = new ArrayList<>(externals);
        int keepSelected = selectedExternal;
        releasePlayer();
        ensurePlayer();
        trackSelector.setParameters(parameters);
        externals.addAll(keep);
        selectedExternal = keepSelected;
        attachSurface();
        seeking = true;
        player.setMediaItem(new MediaItem.Builder().setUri(url).build(), position);
        player.prepare();
        player.setPlayWhenReady(playWhenReady);
    }

    private void attachSurface() {
        SurfaceHolder holder = MainActivity.exoSurfaceHolder();
        if (player != null && holder != null) {
            player.setVideoSurfaceHolder(holder);
        }
    }

    private void seekInternal(long positionMs) {
        if (player == null || url == null) {
            return;
        }
        long duration = player.getDuration();
        long target = Math.max(0, positionMs);
        if (duration != C.TIME_UNSET && duration > 0) {
            target = Math.min(target, Math.max(0, duration - 1000));
        }
        seeking = true;
        ended = false;
        player.seekTo(target);
    }

    private void selectInternal(int kind, int id) {
        if (player == null || url == null) {
            return;
        }
        if (kind == KIND_TEXT) {
            if (id >= EXTERNAL_BASE && id - EXTERNAL_BASE < externals.size()) {
                selectedExternal = id - EXTERNAL_BASE;
                trackSelector.setParameters(trackSelector.buildUponParameters().setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true));
                main.removeCallbacks(subtitleTick);
                main.post(subtitleTick);
                sendTracks(true);
                return;
            }
            selectedExternal = -1;
            main.removeCallbacks(subtitleTick);
            showCues(embeddedCues);
            if (id < 0) {
                trackSelector.setParameters(trackSelector.buildUponParameters()
                        .clearOverridesOfType(C.TRACK_TYPE_TEXT)
                        .setTrackTypeDisabled(C.TRACK_TYPE_TEXT, true));
                showCues(Collections.emptyList());
                sendTracks(true);
                return;
            }
        }
        int groupIndex = id / 1000 - 1;
        int trackIndex = id % 1000;
        List<Tracks.Group> groups = player.getCurrentTracks().getGroups();
        if (groupIndex < 0 || groupIndex >= groups.size()) {
            log(LOG_WARN, "exo: no track " + id);
            return;
        }
        Tracks.Group group = groups.get(groupIndex);
        if (trackIndex >= group.length) {
            log(LOG_WARN, "exo: no track " + id);
            return;
        }
        trackSelector.setParameters(trackSelector.buildUponParameters()
                .setTrackTypeDisabled(group.getType(), false)
                .setOverrideForType(new TrackSelectionOverride(group.getMediaTrackGroup(), trackIndex)));
    }

    private void progressTick() {
        if (player == null || url == null) {
            return;
        }
        long position = player.getCurrentPosition();
        long duration = player.getDuration();
        nativeEvent(handle, EV_PROGRESS, position / 1000.0, duration == C.TIME_UNSET ? -1 : duration / 1000.0, null);
        long ahead = Math.max(0, player.getBufferedPosition() - position);
        nativeEvent(handle, EV_CACHE, ahead / 1000.0, 0, null);
        if (buffering) {
            long wanted = firstFrameSent ? REBUFFER_MS : START_BUFFER_MS;
            nativeEvent(handle, EV_BUFFERING, Math.min(99, ahead * 100 / wanted), 0, null);
        }
        DecoderCounters counters = player.getVideoDecoderCounters();
        if (counters != null) {
            counters.ensureUpdated();
            int output = counters.droppedBufferCount;
            int decoder = counters.droppedInputBufferCount + counters.droppedToKeyframeCount;
            if (output != lastDropsOutput || decoder != lastDropsDecoder) {
                lastDropsOutput = output;
                lastDropsDecoder = decoder;
                nativeEvent(handle, EV_DROPS, output, decoder, null);
            }
        }
        main.postDelayed(progressTick, PROGRESS_MS);
    }

    /** Shows the selected OpenSubtitles file's cues for the current position. */
    private void subtitleTick() {
        if (player == null || selectedExternal < 0 || selectedExternal >= externals.size()) {
            return;
        }
        long positionUs = player.getCurrentPosition() * 1000;
        List<Cue> now = new ArrayList<>();
        for (CuesWithTiming cues : externals.get(selectedExternal).cues) {
            long end = cues.endTimeUs != C.TIME_UNSET ? cues.endTimeUs : cues.startTimeUs + cues.durationUs;
            if (positionUs >= cues.startTimeUs && positionUs < end) {
                now.addAll(cues.cues);
            }
        }
        showCues(now);
        main.postDelayed(subtitleTick, SUBTITLE_TICK_MS);
    }

    private void showCues(List<Cue> cues) {
        SubtitleOverlay overlay = MainActivity.subtitleOverlay();
        if (overlay != null) {
            overlay.setScale(subtitleScale);
            overlay.setLifted(subtitlesLifted);
            overlay.setHidden(subtitlesHidden);
            overlay.setCues(cues);
        }
    }

    private void fail(int kind, String message) {
        if (errorSent) {
            return;
        }
        errorSent = true;
        log(LOG_ERROR, "exo: " + message);
        if (kind == ERR_UNSUPPORTED && player != null) {
            // mpv takes over: no more sound or events from this file here.
            player.stop();
        }
        nativeEvent(handle, EV_ERROR, kind, 0, message);
    }

    private final Player.Listener listener = new Player.Listener() {
        @Override
        public void onPlaybackStateChanged(int state) {
            safely(() -> {
                if (state == Player.STATE_BUFFERING) {
                    buffering = true;
                    progressTick();
                    main.removeCallbacks(progressTick);
                    main.postDelayed(progressTick, PROGRESS_MS);
                } else if (state == Player.STATE_READY) {
                    if (buffering) {
                        buffering = false;
                        nativeEvent(handle, EV_BUFFERING, -1, 0, null);
                    }
                    if (seeking && firstFrameSent) {
                        nativeEvent(handle, EV_STARTED, 0, 0, null);
                    }
                    seeking = false;
                } else if (state == Player.STATE_ENDED && url != null && !ended) {
                    ended = true;
                    nativeEvent(handle, EV_ENDED, 0, 0, null);
                }
            });
        }

        @Override
        public void onPlayWhenReadyChanged(boolean playWhenReady, int reason) {
            safely(() -> {
                nativeEvent(handle, EV_PAUSED, playWhenReady ? 0 : 1, 0, null);
            });
        }

        @Override
        public void onRenderedFirstFrame() {
            safely(() -> {
                if (!firstFrameSent && url != null) {
                    firstFrameSent = true;
                    MainActivity.exoFrameShown();
                    nativeEvent(handle, EV_STARTED, 0, 0, null);
                }
            });
        }

        @Override
        public void onTracksChanged(Tracks tracks) {
            safely(() -> {
                if (url == null || tracks.getGroups().isEmpty()) {
                    return;
                }
                if (!loadedSent) {
                    loadedSent = true;
                    nativeEvent(handle, EV_LOADED, 0, 0, null);
                    if (!checkPlayable(tracks)) {
                        return;
                    }
                }
                sendTracks(false);
                Format video = selectedFormat(tracks, C.TRACK_TYPE_VIDEO);
                if (video != null && video.frameRate > 0 && video.frameRate != frameRateSent) {
                    frameRateSent = video.frameRate;
                    nativeEvent(handle, EV_FRAME_RATE, video.frameRate, 0, null);
                }
            });
        }

        @Override
        public void onVideoSizeChanged(VideoSize size) {
            safely(() -> {
                if (size.width <= 0 || size.height <= 0) {
                    return;
                }
                nativeEvent(handle, EV_VIDEO_SIZE, size.width, size.height, null);
                MainActivity.fitExoVideo(size.width * size.pixelWidthHeightRatio / size.height);
            });
        }

        @Override
        public void onCues(CueGroup cueGroup) {
            safely(() -> {
                embeddedCues = cueGroup.cues;
                if (selectedExternal < 0) {
                    showCues(embeddedCues);
                }
            });
        }

        @Override
        public void onPlayerError(PlaybackException error) {
            safely(() -> {
                if (url == null) {
                    return;
                }
                Log.e(TAG, "ExoPlayer error", error);
                StringBuilder message = new StringBuilder(error.getErrorCodeName());
                for (Throwable cause = error; cause != null; cause = cause.getCause()) {
                    if (cause.getMessage() != null && message.indexOf(cause.getMessage()) < 0) {
                        message.append(": ").append(cause.getMessage());
                    }
                }
                fail(isNetworkError(error.errorCode) ? ERR_NETWORK : ERR_UNSUPPORTED, message.toString());
            });
        }
    };

    /**
     * Whether ExoPlayer can play this file: some video it can decode, some
     * audio when the file has audio, and a seekable file when playback should
     * resume part-way. Otherwise it reports an unsupported error.
     */
    private boolean checkPlayable(Tracks tracks) {
        boolean hasVideo = false;
        boolean videoOk = false;
        boolean hasAudio = false;
        boolean audioOk = false;
        StringBuilder unsupported = new StringBuilder();
        for (Tracks.Group group : tracks.getGroups()) {
            for (int i = 0; i < group.length; i++) {
                boolean ok = group.isTrackSupported(i);
                Format format = group.getTrackFormat(i);
                if (group.getType() == C.TRACK_TYPE_VIDEO) {
                    hasVideo = true;
                    videoOk |= ok;
                } else if (group.getType() == C.TRACK_TYPE_AUDIO) {
                    hasAudio = true;
                    audioOk |= ok;
                }
                if (!ok && group.getType() != C.TRACK_TYPE_TEXT && group.getType() != C.TRACK_TYPE_METADATA) {
                    unsupported.append(' ').append(describe(format));
                }
            }
        }
        if (unsupported.length() > 0) {
            log(LOG_WARN, "exo: tracks this TV can't play:" + unsupported);
        }
        if (!hasVideo) {
            // A video codec ExoPlayer's extractor doesn't know is dropped
            // from the file altogether: it would play sound over black.
            fail(ERR_UNSUPPORTED, "no video track ExoPlayer can read");
            return false;
        }
        if (!videoOk) {
            fail(ERR_UNSUPPORTED, "no video track this TV can decode");
            return false;
        }
        if (hasAudio && !audioOk) {
            fail(ERR_UNSUPPORTED, "no audio track this TV can play");
            return false;
        }
        if (startMs > 1000 && !player.isCurrentMediaItemSeekable()) {
            fail(ERR_UNSUPPORTED, "the file can't be seeked to the resume point");
            return false;
        }
        return true;
    }

    private static boolean isNetworkError(int code) {
        return (code >= PlaybackException.ERROR_CODE_IO_UNSPECIFIED && code < PlaybackException.ERROR_CODE_PARSING_CONTAINER_MALFORMED)
                || code == PlaybackException.ERROR_CODE_TIMEOUT
                || code == PlaybackException.ERROR_CODE_REMOTE_ERROR;
    }

    private final AnalyticsListener analytics = new AnalyticsListener() {
        @Override
        public void onVideoDecoderInitialized(EventTime time, String decoderName, long initializedTimestampMs, long initializationDurationMs) {
            safely(() -> {
                log(LOG_INFO, "exo: video decoder " + decoderName + " (" + initializationDurationMs + " ms)");
            });
        }

        @Override
        public void onAudioDecoderInitialized(EventTime time, String decoderName, long initializedTimestampMs, long initializationDurationMs) {
            safely(() -> {
                log(LOG_INFO, "exo: audio decoder " + decoderName);
            });
        }

        @Override
        public void onVideoInputFormatChanged(EventTime time, Format format, DecoderReuseEvaluation reuse) {
            safely(() -> {
                log(LOG_INFO, "exo: video " + describe(format));
                if (format.frameRate > 0 && format.frameRate != frameRateSent) {
                    frameRateSent = format.frameRate;
                    nativeEvent(handle, EV_FRAME_RATE, format.frameRate, 0, null);
                }
            });
        }

        @Override
        public void onAudioInputFormatChanged(EventTime time, Format format, DecoderReuseEvaluation reuse) {
            safely(() -> {
                log(LOG_INFO, "exo: audio " + describe(format));
            });
        }

        @Override
        public void onAudioTrackInitialized(EventTime time, AudioSink.AudioTrackConfig config) {
            safely(() -> {
                lastAudioEncoding = config.encoding;
                String output = "exo: audio output " + encodingName(config.encoding) + " " + config.sampleRate + " Hz, "
                        + Integer.bitCount(config.channelConfig) + " ch"
                        + (isPassthrough(config.encoding) ? " (passthrough)" : "")
                        + (config.tunneling ? ", tunneled" : "") + (config.offload ? ", offloaded" : "");
                // Every seek sets the output up again: log only changes.
                if (!output.equals(lastAudioOutput)) {
                    lastAudioOutput = output;
                    log(LOG_INFO, output);
                }
                if (pcmOnly && isPassthrough(config.encoding)) {
                    main.post(() -> rebuild("audio normalization needs decoded audio"));
                }
            });
        }

        @Override
        public void onAudioUnderrun(EventTime time, int bufferSize, long bufferSizeMs, long elapsedSinceLastFeedMs) {
            safely(() -> {
                log(LOG_WARN, "exo: audio underrun (" + elapsedSinceLastFeedMs + " ms since the last write)");
            });
        }

        @Override
        public void onAudioSinkError(EventTime time, Exception error) {
            safely(() -> {
                log(LOG_WARN, "exo: audio sink: " + error);
            });
        }

        @Override
        public void onVideoCodecError(EventTime time, Exception error) {
            safely(() -> {
                log(LOG_WARN, "exo: video codec: " + error);
            });
        }
    };

    /**
     * The frame rate from presentation timestamps: the mean gap between
     * consecutive frames (Matroska rounds timestamps to milliseconds, so a
     * single gap isn't exact), leaving out gaps where a frame was dropped,
     * snapped to the usual film and TV rates.
     */
    static float estimateFrameRate(long[] times) {
        long[] sorted = times.clone();
        java.util.Arrays.sort(sorted);
        long[] gaps = new long[sorted.length - 1];
        for (int i = 1; i < sorted.length; i++) {
            gaps[i - 1] = sorted[i] - sorted[i - 1];
        }
        long[] ordered = gaps.clone();
        java.util.Arrays.sort(ordered);
        long median = ordered[ordered.length / 2];
        if (median <= 0) {
            return 0;
        }
        long total = 0;
        int counted = 0;
        for (long gap : gaps) {
            if (gap > 0 && gap < median * 3 / 2) {
                total += gap;
                counted++;
            }
        }
        if (counted == 0) {
            return 0;
        }
        float fps = 1_000_000f * counted / total;
        float[] common = {23.976f, 24f, 25f, 29.97f, 30f, 47.952f, 48f, 50f, 59.94f, 60f};
        float best = 0;
        for (float rate : common) {
            if (Math.abs(fps - rate) / rate < 0.004f && (best == 0 || Math.abs(fps - rate) < Math.abs(fps - best))) {
                best = rate;
            }
        }
        if (best > 0) {
            return best;
        }
        return fps >= 10 && fps <= 120 ? fps : 0;
    }

    private static boolean isPassthrough(int encoding) {
        return encoding != C.ENCODING_INVALID
                && encoding != C.ENCODING_PCM_16BIT
                && encoding != C.ENCODING_PCM_FLOAT
                && encoding != C.ENCODING_PCM_8BIT
                && encoding != C.ENCODING_PCM_24BIT
                && encoding != C.ENCODING_PCM_32BIT
                && encoding != AudioFormat.ENCODING_DEFAULT;
    }

    private static String encodingName(int encoding) {
        switch (encoding) {
            case C.ENCODING_PCM_16BIT: return "PCM 16-bit";
            case C.ENCODING_PCM_FLOAT: return "PCM float";
            case C.ENCODING_PCM_24BIT: return "PCM 24-bit";
            case C.ENCODING_PCM_32BIT: return "PCM 32-bit";
            case C.ENCODING_AC3: return "Dolby Digital";
            case C.ENCODING_E_AC3: return "Dolby Digital Plus";
            case C.ENCODING_E_AC3_JOC: return "Dolby Digital Plus Atmos";
            case C.ENCODING_DOLBY_TRUEHD: return "Dolby TrueHD";
            case C.ENCODING_DTS: return "DTS";
            case C.ENCODING_DTS_HD: return "DTS-HD";
            case C.ENCODING_AC4: return "AC-4";
            default: return "encoding " + encoding;
        }
    }

    /** "video/hevc dvhe.08.06 3840x2160 23.976 fps PQ" style description for the log. */
    private static String describe(Format format) {
        StringBuilder s = new StringBuilder();
        s.append(format.sampleMimeType);
        if (format.codecs != null) {
            s.append(' ').append(format.codecs);
        }
        if (format.width > 0) {
            s.append(' ').append(format.width).append('x').append(format.height);
        }
        if (format.frameRate > 0) {
            s.append(String.format(Locale.ROOT, " %.3f fps", format.frameRate));
        }
        if (format.channelCount > 0) {
            s.append(' ').append(format.channelCount).append(" ch");
        }
        if (format.sampleRate > 0) {
            s.append(' ').append(format.sampleRate).append(" Hz");
        }
        ColorInfo color = format.colorInfo;
        if (color != null) {
            if (color.colorTransfer == C.COLOR_TRANSFER_ST2084) {
                s.append(" PQ");
            } else if (color.colorTransfer == C.COLOR_TRANSFER_HLG) {
                s.append(" HLG");
            }
            if (color.colorSpace == C.COLOR_SPACE_BT2020) {
                s.append(" BT.2020");
            }
        }
        if (format.language != null) {
            s.append(" [").append(format.language).append(']');
        }
        return s.toString();
    }

    private static Format selectedFormat(Tracks tracks, int type) {
        for (Tracks.Group group : tracks.getGroups()) {
            if (group.getType() != type) {
                continue;
            }
            for (int i = 0; i < group.length; i++) {
                if (group.isTrackSelected(i)) {
                    return group.getTrackFormat(i);
                }
            }
        }
        return null;
    }

    /** The track list as JSON for the Rust side; sent when it changed (or when {@code always}). */
    private void sendTracks(boolean always) {
        if (player == null || url == null) {
            return;
        }
        JSONArray list = new JSONArray();
        try {
            List<Tracks.Group> groups = player.getCurrentTracks().getGroups();
            for (int g = 0; g < groups.size(); g++) {
                Tracks.Group group = groups.get(g);
                String type;
                switch (group.getType()) {
                    case C.TRACK_TYPE_VIDEO: type = "video"; break;
                    case C.TRACK_TYPE_AUDIO: type = "audio"; break;
                    case C.TRACK_TYPE_TEXT: type = "sub"; break;
                    default: continue;
                }
                for (int i = 0; i < group.length; i++) {
                    if (!group.isTrackSupported(i) && group.getType() != C.TRACK_TYPE_VIDEO) {
                        continue;
                    }
                    Format format = group.getTrackFormat(i);
                    JSONObject track = new JSONObject();
                    track.put("id", (g + 1) * 1000 + i);
                    track.put("type", type);
                    putIfSet(track, "title", format.label);
                    putIfSet(track, "lang", format.language);
                    putIfSet(track, "codec", codecName(format));
                    if (format.channelCount > 0) {
                        track.put("channels", format.channelCount);
                    }
                    boolean selected = group.isTrackSelected(i)
                            && !(group.getType() == C.TRACK_TYPE_TEXT && selectedExternal >= 0);
                    track.put("selected", selected);
                    track.put("external", false);
                    track.put("default", (format.selectionFlags & C.SELECTION_FLAG_DEFAULT) != 0);
                    track.put("forced", (format.selectionFlags & C.SELECTION_FLAG_FORCED) != 0);
                    list.put(track);
                }
            }
            for (int i = 0; i < externals.size(); i++) {
                External external = externals.get(i);
                JSONObject track = new JSONObject();
                track.put("id", EXTERNAL_BASE + i);
                track.put("type", "sub");
                track.put("title", external.title);
                putIfSet(track, "lang", external.language);
                track.put("codec", "subrip");
                track.put("selected", i == selectedExternal);
                track.put("external", true);
                track.put("default", false);
                track.put("forced", false);
                list.put(track);
            }
        } catch (JSONException e) {
            log(LOG_WARN, "exo: track list: " + e);
            return;
        }
        String json = list.toString();
        if (always || !json.equals(lastTracksJson)) {
            lastTracksJson = json;
            nativeEvent(handle, EV_TRACKS, 0, 0, json);
        }
    }

    private static void putIfSet(JSONObject object, String key, String value) throws JSONException {
        if (value != null && !value.isEmpty()) {
            object.put(key, value);
        }
    }

    /** mpv-style short codec names, which the Rust side shows in track menus. */
    private static String codecName(Format format) {
        // Subtitles parsed while loading carry their original type in `codecs`.
        String mime = MimeTypes.APPLICATION_MEDIA3_CUES.equals(format.sampleMimeType) && format.codecs != null
                ? format.codecs
                : format.sampleMimeType;
        if (mime == null) {
            return null;
        }
        switch (mime) {
            case MimeTypes.AUDIO_TRUEHD: return "truehd";
            case MimeTypes.AUDIO_E_AC3: return "eac3";
            case MimeTypes.AUDIO_E_AC3_JOC: return "eac3 atmos";
            case MimeTypes.AUDIO_AC3: return "ac3";
            case MimeTypes.AUDIO_DTS: return "dts";
            case MimeTypes.AUDIO_DTS_HD: return "dts-hd";
            case MimeTypes.AUDIO_DTS_EXPRESS: return "dts express";
            case MimeTypes.AUDIO_AAC: return "aac";
            case MimeTypes.AUDIO_OPUS: return "opus";
            case MimeTypes.AUDIO_FLAC: return "flac";
            case MimeTypes.AUDIO_VORBIS: return "vorbis";
            case MimeTypes.AUDIO_MPEG: return "mp3";
            case MimeTypes.AUDIO_RAW: return "pcm";
            case MimeTypes.TEXT_SSA: return "ass";
            case MimeTypes.APPLICATION_SUBRIP: return "subrip";
            case MimeTypes.TEXT_VTT: return "webvtt";
            case MimeTypes.APPLICATION_PGS: return "pgs";
            case MimeTypes.APPLICATION_VOBSUB: return "vobsub";
            case MimeTypes.APPLICATION_DVBSUBS: return "dvbsub";
            case MimeTypes.VIDEO_H265: return "hevc";
            case MimeTypes.VIDEO_H264: return "h264";
            case MimeTypes.VIDEO_DOLBY_VISION: return "dolby vision";
            case MimeTypes.VIDEO_AV1: return "av1";
            case MimeTypes.VIDEO_VP9: return "vp9";
            default:
                int slash = mime.indexOf('/');
                return slash >= 0 ? mime.substring(slash + 1) : mime;
        }
    }

    // ---- OpenSubtitles files ----

    private List<CuesWithTiming> downloadSubtitle(String subtitleUrl) throws IOException {
        HttpURLConnection connection = (HttpURLConnection) new URL(subtitleUrl).openConnection();
        connection.setConnectTimeout(15_000);
        connection.setReadTimeout(15_000);
        connection.setInstanceFollowRedirects(true);
        connection.setRequestProperty("User-Agent", userAgent);
        byte[] data;
        try {
            int status = connection.getResponseCode();
            if (status != HttpURLConnection.HTTP_OK) {
                throw new IOException("HTTP " + status);
            }
            try (InputStream in = connection.getInputStream()) {
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                byte[] chunk = new byte[16 * 1024];
                int n;
                while ((n = in.read(chunk)) > 0) {
                    out.write(chunk, 0, n);
                    if (out.size() > MAX_SUBTITLE_BYTES) {
                        throw new IOException("subtitle file over " + (MAX_SUBTITLE_BYTES >> 20) + " MB");
                    }
                }
                data = out.toByteArray();
            }
        } finally {
            connection.disconnect();
        }
        byte[] utf8 = toUtf8(data);
        String head = new String(utf8, 0, Math.min(utf8.length, 4096), StandardCharsets.UTF_8);
        SubtitleParser parser;
        if (head.startsWith("WEBVTT")) {
            parser = new WebvttParser();
        } else if (head.contains("[Script Info]")) {
            parser = new SsaParser();
        } else {
            parser = new SubripParser();
        }
        List<CuesWithTiming> cues = new ArrayList<>();
        parser.parse(utf8, SubtitleParser.OutputOptions.allCues(), cues::add);
        return cues;
    }

    /** Subtitle files come in UTF-8 (with or without a BOM), UTF-16, or a Windows code page. */
    private static byte[] toUtf8(byte[] data) {
        if (data.length >= 3 && (data[0] & 0xFF) == 0xEF && (data[1] & 0xFF) == 0xBB && (data[2] & 0xFF) == 0xBF) {
            byte[] rest = new byte[data.length - 3];
            System.arraycopy(data, 3, rest, 0, rest.length);
            return rest;
        }
        if (data.length >= 2 && (((data[0] & 0xFF) == 0xFF && (data[1] & 0xFF) == 0xFE) || ((data[0] & 0xFF) == 0xFE && (data[1] & 0xFF) == 0xFF))) {
            return new String(data, StandardCharsets.UTF_16).getBytes(StandardCharsets.UTF_8);
        }
        try {
            StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(data));
            return data;
        } catch (CharacterCodingException e) {
            CharBuffer text = Charset.forName("windows-1252").decode(ByteBuffer.wrap(data));
            return text.toString().getBytes(StandardCharsets.UTF_8);
        }
    }

    /** Into the TV log through the native side (which also writes logcat). */
    private static void log(int level, String text) {
        try {
            nativeLog(level, text);
        } catch (UnsatisfiedLinkError e) {
            // The native side hasn't registered yet: logcat only.
            switch (level) {
                case LOG_ERROR: Log.e(TAG, text); break;
                case LOG_WARN: Log.w(TAG, text); break;
                default: Log.i(TAG, text); break;
            }
        }
    }
}
