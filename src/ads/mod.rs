//! Ads-management system.
//!
//! One loop per session, ticking on an interval, that:
//!  1. reads the session's current lifecycle `SessionState` (start / live /
//!     ht / et / ft / stopped),
//!  2. asks the catalog for ads eligible in that state,
//!  3. injects the next one — image ads resize the live video and show the
//!     ad on the left; video ads take over the video pipe with a
//!     "this is an ad" banner — and reverses it when the ad's duration
//!     elapses.
//!
//! The catalog here is a small in-memory stand-in. In production this
//! should be `api::CLIENT` pulling from a Laravel-owned `ads` table
//! (columns roughly: id, kind, path/uri, eligible_states[], duration_ms,
//! weight) — swap `Catalog::eligible_ads` for an HTTP call and nothing
//! else in this module changes.

use crate::config::TRANSITION_MS;
use crate::model::{SessionState, StreamSession};
use crate::pipeline::{ads as ad_pipeline, transitions};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdKind {
    Image,
    Video,
}

#[derive(Clone, Debug)]
pub struct Ad {
    pub id: &'static str,
    pub kind: AdKind,
    /// File path (image) or URI (video, anything `uridecodebin` accepts).
    pub src: &'static str,
    pub eligible: &'static [SessionState],
    /// Display duration for image ads; for video ads, `None` means "play
    /// to EOS" (not implemented in this stub — a fixed duration is used
    /// instead so the loop has a deterministic tick).
    pub duration_ms: u64,
    pub label: &'static str,
}

/// Stand-in catalog. Half-time and full-time get longer/heavier ad slots
/// (a real broadcast sells more inventory there); live play gets short,
/// unobtrusive ones; `start`/`stopped` show nothing.
const CATALOG: &[Ad] = &[
    Ad {
        id: "pitchside-01",
        kind: AdKind::Image,
        src: "ads/pitchside_banner_01.png",
        eligible: &[SessionState::Live],
        duration_ms: 15_000,
        label: "Pitchside Sponsor",
    },
    Ad {
        id: "halftime-video-01",
        kind: AdKind::Video,
        src: "https://cdn.example.com/ads/halftime_spot_01.mp4",
        eligible: &[SessionState::HalfTime],
        duration_ms: 30_000,
        label: "Half-time Spot",
    },
    Ad {
        id: "fulltime-video-01",
        kind: AdKind::Video,
        src: "https://cdn.example.com/ads/fulltime_spot_01.mp4",
        eligible: &[SessionState::FullTime],
        duration_ms: 20_000,
        label: "Full-time Spot",
    },
    Ad {
        id: "extratime-01",
        kind: AdKind::Image,
        src: "ads/extra_time_banner_01.png",
        eligible: &[SessionState::ExtraTime],
        duration_ms: 15_000,
        label: "Extra Time Sponsor",
    },
];

fn eligible_ads(state: SessionState) -> Vec<&'static Ad> {
    CATALOG.iter().filter(|a| a.eligible.contains(&state)).collect()
}

const TICK_INTERVAL_MS: u64 = 5_000;
/// Minimum gap between two ads on the same session, regardless of state
/// changes in between — avoids back-to-back injection if the tick lands
/// right after an ad finishes.
const MIN_GAP_MS: u64 = 20_000;

/// Start the ads-management loop for a session. Call once, right after the
/// session is registered (mirrors `pipeline::transitions::spawn_fallback_monitor`).
pub fn spawn_ads_loop(session: Arc<StreamSession>) {
    tokio::spawn(async move {
        // Best-effort sync: let the backend know this session's ads loop
        // is now live. As the module doc above notes, swapping
        // `Catalog::eligible_ads` for a real HTTP-backed catalog is future
        // work; this call doesn't feed the in-memory `CATALOG` selection
        // below yet, but now correctly hits the public `ads/select`
        // endpoint (it previously built an unreachable, unauthenticated
        // path) to keep the Laravel-side `ads` picture in sync meanwhile.
        crate::api::CLIENT.get_ads(&session.stream_key, session.state().as_str());

        let mut interval = tokio::time::interval(Duration::from_millis(TICK_INTERVAL_MS));
        let mut last_ad_ended_ms: i64 = 0;
        let mut round_robin_idx: usize = 0;

        loop {
            interval.tick().await;

            // Session gone? stop the loop.
            if crate::model::REGISTRY
                .get(&session.stream_key)
                .map(|s| Arc::ptr_eq(&s, &session))
                .unwrap_or(false)
                == false
            {
                break;
            }

            let state = session.state();
            if matches!(state, SessionState::Start | SessionState::Stopped) {
                continue; // no ads before kickoff or after the session ends
            }
            if session.video_source_busy.load(Ordering::SeqCst) {
                continue; // an ad, replay, or other video-source op is already running
            }
            let now = crate::model::session::now_ms();
            if now - last_ad_ended_ms < MIN_GAP_MS as i64 {
                continue;
            }

            let candidates = eligible_ads(state);
            if candidates.is_empty() {
                continue;
            }
            round_robin_idx = (round_robin_idx + 1) % candidates.len();
            let ad = candidates[round_robin_idx];

            info!(stream_key = %session.stream_key, ad_id = ad.id, ?state, "ads loop: injecting ad");
            match ad.kind {
                AdKind::Image => inject_image_ad(&session, ad).await,
                AdKind::Video => inject_video_ad(&session, ad).await,
            }
            last_ad_ended_ms = crate::model::session::now_ms();
        }
        info!(stream_key = %session.stream_key, "ads loop stopped (session ended)");
    });
}

/// Image ad: shrink+shift the live video right to open a left-hand gap,
/// show the ad image in that gap (drawn by the overlay renderer), hold for
/// `duration_ms`, then reverse both.
async fn inject_image_ad(session: &Arc<StreamSession>, ad: &Ad) {
    let Some(overlay_state) = crate::control::overlay_state_for(&session.stream_key) else {
        warn!(stream_key = %session.stream_key, "no overlay state for session, skipping image ad");
        return;
    };

    // Reserve the left ~22% of the canvas for the ad, shift live video right.
    // Two different widths now, where there used to be one: the video pad's
    // real geometry has to use the session's *physical* canvas size (whatever
    // quality it registered at), while the overlay panel drawn by
    // `overlay::render_frame` lives in the fixed *logical* 1280-wide space
    // every widget in `overlay/graphics.rs` is written against — see
    // config::LOGICAL_CANVAS_W's doc comment. Both apply the same 22%
    // fraction, just to different coordinate systems, so they still line up
    // visually once the overlay's own scale-to-physical-size step runs.
    let physical_panel_w = (session.canvas_w as f64 * 0.22) as i32;
    let logical_panel_w = (crate::config::LOGICAL_CANVAS_W as f64 * 0.22) as i32;
    transitions::resize_and_reposition(
        &session.live_video_pad,
        physical_panel_w,
        0,
        session.canvas_w - physical_panel_w,
        session.canvas_h,
        TRANSITION_MS,
    );
    *overlay_state.image_ad.write().unwrap() = Some(crate::overlay::ImageAd {
        path: ad.src.to_string(),
        label: ad.label.to_string(),
        panel_width: logical_panel_w,
    });

    tokio::time::sleep(Duration::from_millis(ad.duration_ms)).await;

    transitions::resize_and_reposition(
        &session.live_video_pad,
        0,
        0,
        session.canvas_w,
        session.canvas_h,
        TRANSITION_MS,
    );
    *overlay_state.image_ad.write().unwrap() = None;
}

/// Video ad: take over the video pipe via `pipeline::ads::start_video_ad`,
/// show a "this is an ad" banner for the duration, then hand back to live.
async fn inject_video_ad(session: &Arc<StreamSession>, ad: &Ad) {
    let Some(overlay_state) = crate::control::overlay_state_for(&session.stream_key) else {
        warn!(stream_key = %session.stream_key, "no overlay state for session, skipping video ad");
        return;
    };

    let handle = match ad_pipeline::start_video_ad(session, ad.src, ad.id) {
        Ok(h) => h,
        Err(e) => {
            warn!(stream_key = %session.stream_key, "video ad failed to start: {e}");
            return;
        }
    };
    overlay_state.video_ad_banner.store(true, Ordering::Relaxed);

    tokio::time::sleep(Duration::from_millis(ad.duration_ms)).await;

    overlay_state.video_ad_banner.store(false, Ordering::Relaxed);
    ad_pipeline::stop_video_ad(session, handle);
}

/// `replay` control action: show a REPLAY banner and drop playback rate to
/// `rate` (defaults to 0.5x at the control-socket layer if the command
/// doesn't specify one). See `pipeline::ads::set_playback_rate` for the
/// caveat on what a rate change actually means for a live, non-seekable
/// source.
pub fn start_replay(session: &Arc<StreamSession>, rate: f64) -> anyhow::Result<()> {
    if session.video_source_busy.swap(true, Ordering::SeqCst) {
        return Err(anyhow::anyhow!("video source busy (ad in progress)"));
    }
    let Some(overlay_state) = crate::control::overlay_state_for(&session.stream_key) else {
        session.video_source_busy.store(false, Ordering::SeqCst);
        return Err(anyhow::anyhow!("no overlay state for session"));
    };
    overlay_state.replay_banner.store(true, Ordering::Relaxed);
    ad_pipeline::set_playback_rate(session, rate)?;
    info!(stream_key = %session.stream_key, rate, "replay started");
    Ok(())
}

/// `replay_end` control action (or auto-timeout, caller's choice): restore
/// 1.0x and hide the banner.
pub fn end_replay(session: &Arc<StreamSession>) -> anyhow::Result<()> {
    let Some(overlay_state) = crate::control::overlay_state_for(&session.stream_key) else {
        return Err(anyhow::anyhow!("no overlay state for session"));
    };
    overlay_state.replay_banner.store(false, Ordering::Relaxed);
    ad_pipeline::set_playback_rate(session, 1.0)?;
    session.video_source_busy.store(false, Ordering::SeqCst);
    info!(stream_key = %session.stream_key, "replay ended, back to 1.0x");
    Ok(())
}
