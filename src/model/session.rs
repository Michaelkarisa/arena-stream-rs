use dashmap::{DashMap, DashSet};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// One dynamically-added output branch (HLS / RTMP / MP4) hanging off the
/// shared `tee`. Kept so we can cleanly remove it later without tearing
/// down the rest of the live pipeline.
pub struct OutputBranch {
    pub id: String,
    pub kind: OutputKind,
    /// The video tee's request pad feeding this branch.
    pub video_tee_pad: gst::Pad,
    /// The audio tee's request pad feeding this branch.
    pub audio_tee_pad: gst::Pad,
    /// The bin containing this branch's queue/encoder/mux/sink chain —
    /// added to the pipeline as a single unit so teardown is one call.
    pub bin: gst::Bin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Rtmp,
    Hls,
    Mp4,
}

/// One camera-operator's video input, wired into its own `compositor` sink
/// pad so `switch` can crossfade between cameras exactly like
/// fallback/ad transitions do. `Primary` is the camera wired in at
/// pipeline-build time (the one whose `start` command actually created the
/// session); `Dynamic` ones are additional cameras added later via
/// `pipeline::cameras::add_camera`, each with its own decode bin.
pub enum CameraInput {
    Primary {
        appsrc: gst_app::AppSrc,
        pad: gst::Pad,
    },
    Dynamic {
        appsrc: gst_app::AppSrc,
        pad: gst::Pad,
        bin: gst::Bin,
    },
}

impl CameraInput {
    pub fn appsrc(&self) -> &gst_app::AppSrc {
        match self {
            Self::Primary { appsrc, .. } | Self::Dynamic { appsrc, .. } => appsrc,
        }
    }

    pub fn pad(&self) -> &gst::Pad {
        match self {
            Self::Primary { pad, .. } | Self::Dynamic { pad, .. } => pad,
        }
    }
}

/// Lifecycle state of a session/match, as tracked by the control socket and
/// consumed by the ads-management loop to decide which ads are eligible.
/// Maps directly to the states named in the ads-management spec:
/// live, start, stopped, ht (half-time), ft (full-time), et (extra time).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionState {
    Start,
    Live,
    HalfTime,
    ExtraTime,
    FullTime,
    Stopped,
}

impl SessionState {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "start" => Some(Self::Start),
            "live" => Some(Self::Live),
            "ht" | "half_time" | "halftime" => Some(Self::HalfTime),
            "et" | "extra_time" | "extratime" => Some(Self::ExtraTime),
            "ft" | "full_time" | "fulltime" => Some(Self::FullTime),
            "stopped" | "stop" => Some(Self::Stopped),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Live => "live",
            Self::HalfTime => "ht",
            Self::ExtraTime => "et",
            Self::FullTime => "ft",
            Self::Stopped => "stopped",
        }
    }
}

/// All runtime state for one live stream session. Replaces
/// `com.stream.model.StreamSession` (Java) — same responsibility, different
/// mechanism: instead of owning FFmpeg FIFO handles, this owns a live
/// GStreamer pipeline and the elements needed to reconfigure it in place.
pub struct StreamSession {
    pub stream_key: String,
    pub created_at_ms: i64,
    last_activity_ms: AtomicI64,
    /// Physical canvas size (compositor output / overlay surface) this
    /// session's pipeline was built at — see
    /// `config::canvas_dims_for_quality`. Fixed for the session's lifetime;
    /// the quality registered at `start` is what the phone actually
    /// captures and sends, so this should always equal that.
    pub canvas_w: i32,
    pub canvas_h: i32,
    /// Laravel-assigned `stream_sessions.id`, set once at session start
    /// (see `api::create_session_awaited`). `stream_sessions` is a
    /// `BaseAppendOnlyModel` with a `uuid` primary key (not auto-increment),
    /// so this holds the UUID string Laravel returned, not a number.
    /// `None` until known — `log_event` callers should treat that as "not
    /// yet available" rather than a real id, and skip or defer logging
    /// rather than send a fake placeholder.
    pub laravel_id: RwLock<Option<String>>,
    /// The match's `author_id` (a `users.id` UUID), resolved by Laravel at
    /// session-creation time and echoed back in the `/stream-sessions`
    /// response (see `api::SessionCreateResult`). This — not the session id
    /// above — is what `award_rank_points` awards points to, since rank is
    /// a property of the broadcaster (the match's author), not the session.
    pub broadcaster_id: RwLock<Option<String>>,

    /// Social-platform ingest keys supplied under the `urls` map of `register`
    /// (platform name `"youtube"`/`"facebook"` -> bare stream key). Used only
    /// to look up that platform's live viewer count — see `social`.
    pub platform_keys: DashMap<String, String>,
    /// Latest viewer count fetched per platform (same keys as above).
    pub platform_views: DashMap<String, u64>,

    pub pipeline: gst::Pipeline,

    // Three inputs (2.1 in the design doc).
    pub video_appsrc: gst_app::AppSrc,
    pub audio_appsrc: gst_app::AppSrc,
    pub overlay_appsrc: gst_app::AppSrc,

    // Compositor sink pads for the live camera input and the fallback loop.
    // Alpha/xpos/ypos/width/height on these pads are what make transitions,
    // resize-for-ad-space, and fallback crossfade all "just property
    // animation" instead of pipeline rebuilds.
    pub live_video_pad: gst::Pad,
    pub fallback_video_pad: gst::Pad,
    pub overlay_pad: gst::Pad,
    /// The main audio appsrc's sink pad on `audiomixer` — captured at build
    /// time so ad/replay audio ducking has something to turn down.
    pub live_audio_pad: gst::Pad,

    pub compositor: gst::Element,
    pub audiomixer: gst::Element,
    /// Tee feeding encoded-video branches (one request pad per output).
    pub video_tee: gst::Element,
    /// Tee feeding encoded-audio branches (one request pad per output).
    pub audio_tee: gst::Element,

    pub output_branches: DashMap<String, OutputBranch>,

    /// Camera-operator client IPs registered to this session (multi-camera
    /// support — same concept as Java's `clients` set).
    pub clients: DashSet<String>,
    pub current_streamer: RwLock<Option<String>>,
    /// One entry per camera-operator IP that has a compositor pad wired in
    /// (the primary one from pipeline build, plus any added dynamically).
    /// `switch` crossfades between these; it does not touch `video_appsrc`/
    /// `live_video_pad` directly anymore except via this map's `Primary`
    /// entry, kept for backward compatibility with the fallback monitor
    /// (§4.3 of WORKFLOW.md), which still watches `live_video_pad`
    /// specifically as "the" live pad regardless of which camera is active.
    pub cameras: DashMap<String, CameraInput>,
    /// Which camera IP is currently visible (alpha≈1) on the live-video
    /// layer. `None` until the first camera's buffers start flowing.
    pub active_camera_ip: RwLock<Option<String>>,
    /// Lifecycle state (start/live/ht/et/ft/stopped) — the ads-management
    /// loop reads this to decide which ads are currently eligible.
    pub state: RwLock<SessionState>,
    /// Set while a video ad is occupying the live-video compositor slot, or
    /// while a replay is in progress — the ads loop and the `replay`
    /// control action check this so they don't stomp on each other.
    pub video_source_busy: std::sync::atomic::AtomicBool,

    /// Hold applied to frame-critical overlay events (goal/card) before
    /// they're drawn — see `config::DEFAULT_OVERLAY_EVENT_DELAY_MS` and the
    /// `set_overlay_delay` control action for why this exists and how it's
    /// calibrated. Per-session (not a global constant) so different
    /// cameras/encoders/networks on the same server can each be tuned to
    /// their own actual glass-to-glass latency.
    overlay_event_delay_ms: std::sync::atomic::AtomicU64,

    last_video_buffer_ms: AtomicI64,
}

impl StreamSession {
    pub fn new(
        stream_key: String,
        canvas_w: i32,
        canvas_h: i32,
        pipeline: gst::Pipeline,
        video_appsrc: gst_app::AppSrc,
        audio_appsrc: gst_app::AppSrc,
        overlay_appsrc: gst_app::AppSrc,
        live_video_pad: gst::Pad,
        fallback_video_pad: gst::Pad,
        overlay_pad: gst::Pad,
        live_audio_pad: gst::Pad,
        compositor: gst::Element,
        audiomixer: gst::Element,
        video_tee: gst::Element,
        audio_tee: gst::Element,
    ) -> Self {
        let now = now_ms();
        Self {
            stream_key,
            created_at_ms: now,
            last_activity_ms: AtomicI64::new(now),
            canvas_w,
            canvas_h,
            laravel_id: RwLock::new(None),
            broadcaster_id: RwLock::new(None),
            platform_keys: DashMap::new(),
            platform_views: DashMap::new(),
            pipeline,
            video_appsrc,
            audio_appsrc,
            overlay_appsrc,
            live_video_pad,
            fallback_video_pad,
            overlay_pad,
            live_audio_pad,
            compositor,
            audiomixer,
            video_tee,
            audio_tee,
            output_branches: DashMap::new(),
            clients: DashSet::new(),
            current_streamer: RwLock::new(None),
            cameras: DashMap::new(),
            active_camera_ip: RwLock::new(None),
            state: RwLock::new(SessionState::Start),
            video_source_busy: std::sync::atomic::AtomicBool::new(false),
            overlay_event_delay_ms: std::sync::atomic::AtomicU64::new(
                crate::config::DEFAULT_OVERLAY_EVENT_DELAY_MS,
            ),
            last_video_buffer_ms: AtomicI64::new(now),
        }
    }

    pub fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn last_activity_ms(&self) -> i64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }

    pub fn mark_video_buffer(&self) {
        self.last_video_buffer_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Current hold, in ms, applied before a frame-critical overlay event
    /// (goal/card) is actually drawn. See `set_overlay_event_delay_ms`.
    pub fn overlay_event_delay_ms(&self) -> u64 {
        self.overlay_event_delay_ms.load(Ordering::Relaxed)
    }

    /// Calibrate this session's overlay-event delay — the `set_overlay_delay`
    /// control action's backing call. Takes effect on the next frame-critical
    /// event; doesn't retroactively affect one already scheduled.
    pub fn set_overlay_event_delay_ms(&self, ms: u64) {
        self.overlay_event_delay_ms.store(ms, Ordering::Relaxed);
    }

    pub fn ms_since_last_video_buffer(&self) -> i64 {
        now_ms() - self.last_video_buffer_ms.load(Ordering::Relaxed)
    }

    pub fn is_current_streamer(&self, ip: &str) -> bool {
        self.current_streamer
            .read()
            .unwrap()
            .as_deref()
            .map(|s| s == ip)
            .unwrap_or(false)
    }

    pub fn state(&self) -> SessionState {
        *self.state.read().unwrap()
    }

    pub fn set_state(&self, s: SessionState) {
        *self.state.write().unwrap() = s;
    }

    /// `None` if the Laravel-side session row hasn't been created yet (the
    /// awaited call in the `start` handler hasn't returned). Callers should
    /// treat `None` as "skip logging this event" rather than substitute a
    /// placeholder id.
    pub fn laravel_id(&self) -> Option<String> {
        self.laravel_id.read().unwrap().clone()
    }

    /// Records the Laravel-assigned `stream_sessions.id` (a UUID string)
    /// once `api::create_session_awaited` returns it.
    pub fn set_laravel_id(&self, id: String) {
        *self.laravel_id.write().unwrap() = Some(id);
    }

    /// `None` until `create_session_awaited` returns one — either the
    /// session row hasn't been created yet, or the match had no
    /// `author_id` on file at creation time.
    pub fn broadcaster_id(&self) -> Option<String> {
        self.broadcaster_id.read().unwrap().clone()
    }

    /// Records the match author's `users.id`, as resolved server-side by
    /// `StreamSessionService::create` and returned alongside the session id.
    pub fn set_broadcaster_id(&self, id: String) {
        *self.broadcaster_id.write().unwrap() = Some(id);
    }

    pub fn set_platform_key(&self, platform: &str, key: String) {
        self.platform_keys.insert(platform.to_string(), key);
    }

    /// Snapshot of `(platform, latest viewer count)` for every platform that
    /// has been fetched at least once. Empty until the first successful poll.
    pub fn platform_views_snapshot(&self) -> Vec<(String, u64)> {
        self.platform_views.iter().map(|e| (e.key().clone(), *e.value())).collect()
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
