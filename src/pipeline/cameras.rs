//! Multi-camera scene-switch crossfade.
//!
//! This fills the gap flagged in WORKFLOW.md §7.4: `switch` used to only
//! update audio authority. Now it does what its name implies — genuinely
//! crossfades the visible video from one camera's compositor pad to
//! another's, using the exact same `transitions::crossfade` primitive as
//! fallback engage/disengage and video ads. Nothing new there; what's new
//! is that a compositor pad now exists *per camera* instead of just one.
//!
//! Each additional camera (beyond the primary one wired in at pipeline
//! build time) gets its own small bin — appsrc(H.264) → h264parse →
//! avdec_h264 → videoconvert → videoscale — linked into a freshly
//! requested `compositor` pad, added/removed live via the same
//! bin-with-ghost-pad pattern `pipeline::add_output` and
//! `pipeline::ads::start_video_ad` already use. No decodebin/dynamic-pad
//! handling is needed here (unlike `ads::start_video_ad`) because the
//! input is already raw H.264 pushed via `appsrc`, so every element in the
//! chain has a static pad — `gst::parse::bin_from_description` with
//! `ghost_unlinked_pads=true` can build and ghost it in one call.

use crate::model::{CameraInput, StreamSession};
use crate::pipeline::transitions;
use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::Arc;
use tracing::info;

fn sanitize(ip: &str) -> String {
    ip.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Idempotent: if `ip` already has a camera branch (primary or dynamic),
/// this is a no-op. Call this eagerly on `register` so `switch` has
/// somewhere to crossfade to even before that camera's first video packet
/// arrives, and also lazily from the video-ingest path as a fallback for
/// cameras that never explicitly registered.
pub fn add_camera(session: &Arc<StreamSession>, ip: &str) -> Result<()> {
    if session.cameras.contains_key(ip) {
        return Ok(());
    }

    let desc = format!(
        "appsrc name=camsrc format=time is-live=true do-timestamp=true \
            caps=\"video/x-h264,stream-format=byte-stream,alignment=nal\" ! \
         h264parse ! avdec_h264 ! videoconvert ! videoscale ! \
         video/x-raw,width={cw},height={ch}",
        cw = session.canvas_w,
        ch = session.canvas_h,
    );
    let bin = gst::parse::bin_from_description(&desc, true)
        .context("failed to build camera decode bin")?;
    bin.set_property("name", format!("camera-{}", sanitize(ip)));

    let camsrc_elem = bin
        .by_name("camsrc")
        .ok_or_else(|| anyhow!("camsrc element missing from just-built bin"))?;
    let appsrc = camsrc_elem
        .dynamic_cast::<gst_app::AppSrc>()
        .map_err(|_| anyhow!("camsrc is not an AppSrc"))?;

    let ghost_src = bin
        .static_pad("src")
        .ok_or_else(|| anyhow!("camera bin has no auto-ghosted src pad — check the bin description"))?;

    session
        .pipeline
        .add(&bin)
        .context("failed to add camera bin to pipeline")?;

    let video_pad = session
        .compositor
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("compositor has no free request pad"))?;
    // New cameras start invisible; `switch_camera` crossfades them in.
    video_pad.set_property("alpha", 0.0f64);
    video_pad.set_property("zorder", 1u32);

    ghost_src
        .link(&video_pad)
        .context("failed to link camera bin into compositor")?;

    bin.sync_state_with_parent()
        .context("failed to sync camera bin state with pipeline")?;

    session
        .cameras
        .insert(ip.to_string(), CameraInput::Dynamic { appsrc, pad: video_pad, bin });

    info!(stream_key = %session.stream_key, camera_ip = ip, "camera branch added");
    Ok(())
}

/// Remove a camera branch — e.g. once an operator disconnects. Wired to
/// the control socket's `remove_camera` action (see control/mod.rs).
/// Refuses to remove the currently-active camera; `switch` away first.
pub fn remove_camera(session: &Arc<StreamSession>, ip: &str) -> Result<()> {
    if session.active_camera_ip.read().unwrap().as_deref() == Some(ip) {
        return Err(anyhow!("camera '{ip}' is currently active — switch away before removing it"));
    }
    let Some((_, camera)) = session.cameras.remove(ip) else {
        return Err(anyhow!("camera '{ip}' not found"));
    };
    match camera {
        CameraInput::Primary { .. } => {
            // Put it back — the primary camera's pad is load-bearing for
            // the fallback monitor and isn't safe to tear down here.
            Err(anyhow!("refusing to remove the primary camera's pad"))
        }
        CameraInput::Dynamic { pad, bin, .. } => {
            let _ = bin.set_state(gst::State::Null);
            let _ = session.pipeline.remove(&bin);
            session.compositor.release_request_pad(&pad);
            info!(stream_key = %session.stream_key, camera_ip = ip, "camera branch removed");
            Ok(())
        }
    }
}

/// The actual `switch` behavior: crossfade from whichever camera is
/// currently active to `target_ip`, and update `active_camera_ip`. Builds
/// the target's branch first if it doesn't exist yet (covers the case
/// where `switch` names a camera whose `register` we saw but that hasn't
/// sent video, or one we've never heard of at all — as long as it's a
/// legitimate operator IP, its branch will just sit dark until packets
/// arrive).
pub fn switch_camera(session: &Arc<StreamSession>, target_ip: &str) -> Result<()> {
    add_camera(session, target_ip)?;

    let current_ip = session.active_camera_ip.read().unwrap().clone();
    if current_ip.as_deref() == Some(target_ip) {
        return Ok(()); // already active, no-op
    }

    let target_pad = {
        let entry = session
            .cameras
            .get(target_ip)
            .ok_or_else(|| anyhow!("camera '{target_ip}' has no compositor pad"))?;
        entry.pad().clone()
    };

    match current_ip.as_deref().and_then(|ip| session.cameras.get(ip)) {
        Some(current) => transitions::crossfade(current.pad(), &target_pad),
        None => {
            // No camera active yet (first ever switch) — just fade this
            // one in against whatever's currently showing (fallback).
            transitions::animate_f64(&target_pad, "alpha", 1.0, crate::config::TRANSITION_MS);
        }
    }

    *session.active_camera_ip.write().unwrap() = Some(target_ip.to_string());
    info!(stream_key = %session.stream_key, from = ?current_ip, to = target_ip, "camera switched");
    Ok(())
}

/// The pad the fallback monitor should crossfade against: the *active*
/// camera's pad if one is set, else the primary camera's pad (pre-switch
/// behavior, preserved as the fallback-of-the-fallback).
pub fn active_or_primary_pad(session: &StreamSession) -> gst::Pad {
    if let Some(ip) = session.active_camera_ip.read().unwrap().as_deref() {
        if let Some(cam) = session.cameras.get(ip) {
            return cam.pad().clone();
        }
    }
    session.live_video_pad.clone()
}
