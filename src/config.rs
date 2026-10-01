//! Single source of truth for tunables. Mirrors `com.stream.config.StreamConfig`
//! from the original Java implementation so behavior is comparable during
//! migration testing.

// ── Ports ────────────────────────────────────────────────────────────────
pub const TCP_CONTROL_PORT: u16 = 5000;
pub const UDP_VIDEO_PORT: u16 = 5001;
pub const UDP_AUDIO_PORT: u16 = 5002;
pub const TEST_NET_PORT: u16 = 5003;
// ── Ingest ───────────────────────────────────────────────────────────────
pub const UDP_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_AUDIO_FRAME_BYTES: usize = 4096;
pub const AUDIO_SAMPLE_RATE: i32 = 44_100;
pub const AUDIO_CHANNELS: i32 = 1;

// ── Canvas / overlay ─────────────────────────────────────────────────────
/// Every widget in `overlay/graphics.rs` computes pixel positions directly
/// against a 1280×720 canvas — see that module's docs — with no scale
/// factor anywhere in the widget source. Keep these two exactly as they are:
/// they are the coordinate space *all* overlay drawing is written in, not
/// the physical output size (see `canvas_dims_for_quality` for that).
/// `overlay::render_frame` bridges the two with one `cairo::Context::scale`
/// call, so none of `graphics.rs`'s widget math has to change per session.
pub const LOGICAL_CANVAS_W: i32 = 1280;
pub const LOGICAL_CANVAS_H: i32 = 720;

/// Fallback physical canvas size — used only by `canvas_dims_for_quality`
/// for a quality value it doesn't recognize. Not read directly elsewhere;
/// every session's *actual* canvas size lives on its `StreamSession`
/// (`canvas_w`/`canvas_h`), set once at `start` from the registered quality.
pub const DEFAULT_CANVAS_W: i32 = 1280;
pub const DEFAULT_CANVAS_H: i32 = 720;

/// Physical canvas (compositor output / overlay surface) dimensions for a
/// registered stream quality. Must match `Api.quality()` in
/// `api_service.dart` exactly (854×480, 1280×720, 1920×1080, 3840×2160) —
/// that map is what actually configures the phone's camera capture
/// resolution, so this is the resolution H.264 arrives in over UDP.
///
/// Before this, the compositor's output (and therefore the encode) was
/// hard-coded to 1280×720 regardless of what the phone captured, so
/// `pipeline::build_session_pipeline`'s `videoscale` step was silently
/// up- or down-scaling every session to 720p — visibly blurry for a 480p
/// registration (upscaled) and a wasted encode for 1080p/4K (downscaled
/// after capture, for no reason). Building the pipeline at the session's
/// own registered size makes that `videoscale` step a no-op in the normal
/// case, matching output resolution to what was actually captured.
pub fn canvas_dims_for_quality(quality: i32) -> (i32, i32) {
    match quality {
        480 => (854, 480),
        720 => (1280, 720),
        1080 => (1920, 1080),
        2160 => (3840, 2160),
        other => {
            tracing::warn!(quality = other, "unrecognized quality, defaulting to 720p canvas");
            (DEFAULT_CANVAS_W, DEFAULT_CANVAS_H)
        }
    }
}
pub const OVERLAY_FPS: i32 = 30;

// ── Encode ───────────────────────────────────────────────────────────────
pub const VIDEO_BITRATE_KBPS: u32 = 2000;
pub const AUDIO_BITRATE_BPS: u32 = 96_000;
pub const KEYFRAME_INTERVAL_FRAMES: u32 = 30;

// ── Transitions ──────────────────────────────────────────────────────────
/// Duration of a crossfade / resize / reposition animation (ms). Applies to
/// scene-switch transitions and live ad-space resize/reposition alike —
/// both are just interpolated GObject property changes on compositor pads.
pub const TRANSITION_MS: u64 = 400;

// ── Fallback ─────────────────────────────────────────────────────────────
/// If no buffer has arrived on the live video pad within this window,
/// crossfade to the fallback source.
pub const FALLBACK_TIMEOUT_MS: u64 = 4_000;

// ── Overlay event timing ────────────────────────────────────────────────
/// Default hold applied to frame-critical overlay events (goal/card — see
/// `StreamSession::overlay_event_delay_ms` and the "goal"/"card" handlers
/// in `control/mod.rs`) before they're actually drawn.
///
/// An operator confirming "goal" reacts to *seeing* the ball cross the
/// line — on whatever monitor they're watching, which is very often lower
/// latency than this server's own ingest path (network transit + H.264
/// decode between the camera and `compositor.sink_1`, see
/// `pipeline::build_session_pipeline`). Applying the score bump and the
/// "GOAL!" banner the instant the command arrives risks putting them on
/// screen *before* our own video has actually reached that moment —
/// visibly wrong, since the ever-on scorebar reads the same match state.
///
/// This value is added to the command's own `timestamp` field (client
/// epoch-ms, stamped when the operator confirmed the event — see
/// `control::schedule_overlay_event`), not to this server's arrival time.
/// That keeps the target moment independent of how long the command
/// itself took to get here, since the command and the video take
/// separate, uncorrelated network paths.
///
/// `0` here is the safe default (no assumed latency); it's deliberately
/// not tuned to a "typical" setup, since ingest latency varies a lot by
/// camera/encoder/network. Calibrate per session with the `set_overlay_delay`
/// control action once the actual glass-to-glass delay is known (e.g. via
/// a clapperboard/stopwatch test against the composited output).
pub const DEFAULT_OVERLAY_EVENT_DELAY_MS: u64 = 0;

/// Upper bound on how long `schedule_overlay_event` will ever hold an
/// event, regardless of what `event_timestamp_ms + overlay_event_delay_ms`
/// computes to. Exists as a clock-skew guard: a client clock running ahead
/// of the server's would otherwise compute a target arbitrarily far in the
/// future and hold the overlay (and the score bump riding with it)
/// indefinitely. Hitting this cap is logged as a probable clock-skew
/// warning rather than failing silently.
pub const MAX_OVERLAY_EVENT_HOLD_MS: u64 = 10_000;

// ── Health / cleanup ─────────────────────────────────────────────────────
pub const INACTIVITY_CLEANUP_MS: u64 = 900_000; // 15 min
pub const METRICS_REPORT_INTERVAL_MS: u64 = 5_000;

// ── Laravel ("switch6") backend API ─────────────────────────────────────
pub const API_BASE_URL_ENV: &str = "ARENA_API_BASE_URL"; // e.g. https://switch6.example.com
pub const API_TOKEN_ENV: &str = "ARENA_API_TOKEN";
