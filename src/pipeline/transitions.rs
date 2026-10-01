//! Seamless live transitions.
//!
//! Every "happen live, must be seamless" requirement (scene-change
//! transitions, resize/reposition for ad space, fallback crossfade) reduces
//! to the same primitive: animate a GObject property on a `compositor` sink
//! pad over a short duration instead of jumping to the new value.
//!
//! This implementation steps the property linearly over N ticks using a
//! background thread. It's intentionally simple so the mechanism is easy to
//! follow; for frame-accurate timing tied to the pipeline clock, swap this
//! for `gstreamer_controller::InterpolationControlSource` bound via
//! `gst::ControlBinding` — same pads/properties, tighter timing guarantees.

use crate::config::TRANSITION_MS;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::debug;

const STEPS: u32 = 20;

/// Animate a single f64 property (e.g. `alpha`) on a pad from its current
/// value to `target` over `duration_ms`.
pub fn animate_f64(pad: &gst::Pad, prop: &'static str, target: f64, duration_ms: u64) {
    let start: f64 = pad.property(prop);
    let owned_pad = pad.clone();
    spawn_tween(owned_pad.clone(), duration_ms, move |t| {
        let value = start + (target - start) * t;
        // Best-effort: ignore errors from a pad that's been unlinked mid-animation.
        owned_pad.set_property(prop, value);
    });
}

/// Animate a single i32 property (e.g. `xpos`, `ypos`, `width`, `height`)
/// on a pad from its current value to `target` over `duration_ms`.
pub fn animate_i32(pad: &gst::Pad, prop: &'static str, target: i32, duration_ms: u64) {
    let start: i32 = pad.property(prop);
    let owned_pad = pad.clone();
    spawn_tween(owned_pad.clone(), duration_ms, move |t| {
        let value = start + ((target - start) as f64 * t).round() as i32;
        owned_pad.set_property(prop, value);
    });
}

fn spawn_tween<F>(_pad_keepalive: gst::Pad, duration_ms: u64, mut step: F)
where
    F: FnMut(f64) + Send + 'static,
{
    let step_delay = Duration::from_millis((duration_ms / STEPS as u64).max(1));
    thread::spawn(move || {
        for i in 0..=STEPS {
            let t = i as f64 / STEPS as f64;
            step(t);
            thread::sleep(step_delay);
        }
    });
}

/// Crossfade compositor sink pads: fade `from` to alpha 0 while fading `to`
/// to alpha 1, over [`TRANSITION_MS`]. Used for scene-change transitions
/// (switching the active streamer) and fallback engage/disengage alike —
/// same mechanism, different pads.
pub fn crossfade(from: &gst::Pad, to: &gst::Pad) {
    debug!("crossfade start");
    animate_f64(from, "alpha", 0.0, TRANSITION_MS);
    animate_f64(to, "alpha", 1.0, TRANSITION_MS);
}

/// Smoothly resize/reposition the live video pad to make room for an
/// on-screen image ad, instead of jumping to the new geometry.
pub fn resize_and_reposition(
    pad: &gst::Pad,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    duration_ms: u64,
) {
    animate_i32(pad, "xpos", x, duration_ms);
    animate_i32(pad, "ypos", y, duration_ms);
    animate_i32(pad, "width", width, duration_ms);
    animate_i32(pad, "height", height, duration_ms);
}

/// Background task: watches for a gap in live video buffers and crossfades
/// to the fallback source; crossfades back once live buffers resume.
/// Replaces the Java `PipeManager` priming-writer / fallback-timeout logic.
pub fn spawn_fallback_monitor(session: Arc<crate::model::StreamSession>) {
    tokio::spawn(async move {
        let mut in_fallback = true; // pipeline starts in fallback (see pipeline::build_session_pipeline)
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            if !crate::model::REGISTRY
                .get(&session.stream_key)
                .map(|s| Arc::ptr_eq(&s, &session))
                .unwrap_or(false)
            {
                break; // session was removed/stopped
            }

            let gap = session.ms_since_last_video_buffer();
            let should_be_fallback = gap as u64 >= crate::config::FALLBACK_TIMEOUT_MS;
            // Crossfade against whichever camera is actually visible right
            // now, not always the primary — see pipeline::cameras::active_or_primary_pad.
            let live_pad = crate::pipeline::cameras::active_or_primary_pad(&session);

            if should_be_fallback && !in_fallback {
                crossfade(&live_pad, &session.fallback_video_pad);
                in_fallback = true;
                debug!(stream_key = %session.stream_key, "engaged fallback (no live video for {gap}ms)");
            } else if !should_be_fallback && in_fallback {
                crossfade(&session.fallback_video_pad, &live_pad);
                in_fallback = false;
                debug!(stream_key = %session.stream_key, "disengaged fallback, live video resumed");
            }
        }
    });
}
