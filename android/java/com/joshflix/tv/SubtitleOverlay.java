package com.joshflix.tv;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Rect;
import android.graphics.RectF;
import android.text.Layout;
import android.text.SpannableStringBuilder;
import android.text.StaticLayout;
import android.text.TextPaint;
import android.text.style.ForegroundColorSpan;
import android.view.View;

import androidx.media3.common.text.Cue;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Draws ExoPlayer's subtitle cues: text with a black outline, and bitmap
 * subtitles (PGS, DVB, VobSub) where the cue places them.
 *
 * <p>It is an ordinary view in the window, so it sits between the video's
 * {@code SurfaceView} and the translucent UI surface on top. The look matches
 * what mpv drew before: white text, 2.5 px border and a soft shadow at 720p
 * scale, 6.4% of the screen height, 48 px from the bottom (150 px while the
 * player's controls show).
 */
final class SubtitleOverlay extends View {
    /** mpv's {@code sub-font-size=46} at its 720-line reference height. */
    private static final float TEXT_SIZE_FRACTION = 46f / 720f;
    private static final float MARGIN_FRACTION = 48f / 720f;
    private static final float LIFTED_MARGIN_FRACTION = 150f / 720f;
    private static final float OUTLINE_FRACTION = 2.5f / 720f;
    private static final float SHADOW_FRACTION = 1.5f / 720f;
    /** Text never runs closer than this to the left and right edges. */
    private static final float SIDE_FRACTION = 0.06f;

    private final TextPaint fill = new TextPaint(Paint.ANTI_ALIAS_FLAG | Paint.SUBPIXEL_TEXT_FLAG);
    private final TextPaint stroke = new TextPaint(Paint.ANTI_ALIAS_FLAG | Paint.SUBPIXEL_TEXT_FLAG);
    private final Paint bitmapPaint = new Paint(Paint.FILTER_BITMAP_FLAG | Paint.ANTI_ALIAS_FLAG);
    private final Rect source = new Rect();
    private final RectF target = new RectF();

    private List<Cue> cues = Collections.emptyList();
    private float scale = 1f;
    private boolean lifted;
    private boolean hidden;

    SubtitleOverlay(Context context) {
        super(context);
        fill.setColor(Color.WHITE);
        stroke.setColor(Color.BLACK);
        stroke.setStyle(Paint.Style.STROKE);
        stroke.setStrokeJoin(Paint.Join.ROUND);
        setWillNotDraw(false);
    }

    /** Main thread. */
    void setCues(List<Cue> next) {
        List<Cue> copy = next == null ? Collections.emptyList() : new ArrayList<>(next);
        if (copy.equals(cues)) {
            return;
        }
        cues = copy;
        invalidate();
    }

    /** Main thread: the user's subtitle size (1.0 = default). */
    void setScale(float next) {
        if (next != scale) {
            scale = next;
            invalidate();
        }
    }

    /** Main thread: draw nothing for now (a menu shows). */
    void setHidden(boolean next) {
        if (next != hidden) {
            hidden = next;
            invalidate();
        }
    }

    /** Main thread: lift text above the player's controls. */
    void setLifted(boolean next) {
        if (next != lifted) {
            lifted = next;
            invalidate();
        }
    }

    @Override
    protected void onDraw(Canvas canvas) {
        int width = getWidth();
        int height = getHeight();
        if (hidden || cues.isEmpty() || width == 0 || height == 0) {
            return;
        }
        float textSize = TEXT_SIZE_FRACTION * height * scale;
        float margin = (lifted ? LIFTED_MARGIN_FRACTION : MARGIN_FRACTION) * height;
        // Bottom-placed text cues stack upwards in order.
        float bottom = height - margin;
        for (int i = cues.size() - 1; i >= 0; i--) {
            Cue cue = cues.get(i);
            if (cue.bitmap != null) {
                drawBitmap(canvas, cue, width, height);
            } else if (cue.text != null && cue.text.length() > 0) {
                if (cue.line == Cue.DIMEN_UNSET || cue.lineType != Cue.LINE_TYPE_FRACTION) {
                    bottom = drawText(canvas, cue, width, height, textSize, bottom, true) - textSize * 0.15f;
                } else {
                    drawText(canvas, cue, width, height, textSize, cue.line * height, false);
                }
            }
        }
    }

    /**
     * Draws one text cue. With {@code fromBottom}, {@code y} is the bottom
     * edge of the text; otherwise it is the cue's line position, anchored as
     * the cue says. Returns the top edge of the drawn text.
     */
    private float drawText(Canvas canvas, Cue cue, int width, int height, float textSize, float y, boolean fromBottom) {
        float side = SIDE_FRACTION * width;
        int maxWidth = (int) (width - 2 * side);
        if (cue.size != Cue.DIMEN_UNSET) {
            maxWidth = Math.max(1, Math.min(maxWidth, (int) (cue.size * width)));
        }
        fill.setTextSize(textSize);
        stroke.setTextSize(textSize);
        float outline = OUTLINE_FRACTION * height * scale;
        stroke.setStrokeWidth(outline * 2);
        float shadow = SHADOW_FRACTION * height * scale;
        stroke.setShadowLayer(shadow * 1.5f, shadow, shadow, 0xB0000000);

        Layout.Alignment alignment = cue.textAlignment != null ? cue.textAlignment : Layout.Alignment.ALIGN_CENTER;
        CharSequence text = cue.text;
        StaticLayout textLayout = layout(text, fill, maxWidth, alignment);
        // The outline ignores text colours: strip them for the stroke pass.
        StaticLayout outlineLayout = layout(withoutColors(text), stroke, maxWidth, alignment);

        int textWidth = 0;
        for (int line = 0; line < textLayout.getLineCount(); line++) {
            textWidth = Math.max(textWidth, (int) Math.ceil(textLayout.getLineWidth(line)));
        }
        float left;
        if (cue.position != Cue.DIMEN_UNSET) {
            float x = cue.position * width;
            if (cue.positionAnchor == Cue.ANCHOR_TYPE_START) {
                left = x;
            } else if (cue.positionAnchor == Cue.ANCHOR_TYPE_END) {
                left = x - maxWidth;
            } else {
                left = x - maxWidth / 2f;
            }
        } else {
            left = (width - maxWidth) / 2f;
        }
        left = Math.max(side / 2, Math.min(left, width - side / 2 - maxWidth));

        float textHeight = textLayout.getHeight();
        float top;
        if (fromBottom) {
            top = y - textHeight;
        } else if (cue.lineAnchor == Cue.ANCHOR_TYPE_END) {
            top = y - textHeight;
        } else if (cue.lineAnchor == Cue.ANCHOR_TYPE_MIDDLE) {
            top = y - textHeight / 2;
        } else {
            top = y;
        }
        top = Math.max(0, Math.min(top, height - textHeight));

        int save = canvas.save();
        canvas.translate(left, top);
        outlineLayout.draw(canvas);
        textLayout.draw(canvas);
        canvas.restoreToCount(save);
        return top;
    }

    @SuppressWarnings("deprecation")
    private static StaticLayout layout(CharSequence text, TextPaint paint, int width, Layout.Alignment alignment) {
        return new StaticLayout(text, paint, width, alignment, 1.0f, 0f, false);
    }

    private static CharSequence withoutColors(CharSequence text) {
        SpannableStringBuilder copy = new SpannableStringBuilder(text);
        for (ForegroundColorSpan span : copy.getSpans(0, copy.length(), ForegroundColorSpan.class)) {
            copy.removeSpan(span);
        }
        return copy;
    }

    /** A bitmap cue, placed by its position, line and size fractions. */
    private void drawBitmap(Canvas canvas, Cue cue, int width, int height) {
        Bitmap bitmap = cue.bitmap;
        if (bitmap == null || bitmap.isRecycled()) {
            return;
        }
        float w = cue.size != Cue.DIMEN_UNSET ? cue.size * width : bitmap.getWidth() * (width / 1920f);
        float h = cue.bitmapHeight != Cue.DIMEN_UNSET
                ? cue.bitmapHeight * height
                : w * bitmap.getHeight() / Math.max(1, bitmap.getWidth());
        float x = cue.position != Cue.DIMEN_UNSET ? cue.position * width : (width - w) / 2;
        if (cue.positionAnchor == Cue.ANCHOR_TYPE_END) {
            x -= w;
        } else if (cue.positionAnchor == Cue.ANCHOR_TYPE_MIDDLE) {
            x -= w / 2;
        }
        float y = cue.line != Cue.DIMEN_UNSET ? cue.line * height : height - h - MARGIN_FRACTION * height;
        if (cue.lineAnchor == Cue.ANCHOR_TYPE_END) {
            y -= h;
        } else if (cue.lineAnchor == Cue.ANCHOR_TYPE_MIDDLE) {
            y -= h / 2;
        }
        source.set(0, 0, bitmap.getWidth(), bitmap.getHeight());
        target.set(x, y, x + w, y + h);
        canvas.drawBitmap(bitmap, source, target, bitmapPaint);
    }
}
