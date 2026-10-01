//! Video-ad playback and replay speed control.
//!
//! Video ads reuse the exact same primitive as fallback/scene-change
//! transitions (crossfade a compositor pad in, crossfade the previous one
//! out) — the only new part is that the "new pad" isn't one of the three
//! pre-built inputs, it's a **dynamically requested** `compositor`/
//! `audiomixer` pad fed by a small `uridecodebin` bin built and torn down
//! per ad. Same pattern `pipeline::add_output` uses for tee branches,
//! applied to the compositor instead.

use crate::config::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, TRANSITION_MS};
use crate::model::StreamSession;
use crate::pipeline::transitions;
use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{info, warn};

/// Handle to a currently-playing video ad, kept by the ads-management loop
/// so it knows what to tear down when the ad finishes.
pub struct VideoAdHandle {
    pub bin: gst::Bin,
    pub video_pad: gst::Pad,
    pub audio_pad: gst::Pad,
}

/// Start playing a video ad inline: builds a `uridecodebin`-fed bin,
/// requests fresh pads on the shared `compositor`/`audiomixer`, links in,
/// then crossfades it over the live video (and ducks live audio) — all
/// without touching the rest of the pipeline or dropping a frame on the
/// output branches.
///
/// `uri` accepts anything `uridecodebin` does: `file:///...`, `http(s)://...`.
pub fn start_video_ad(session: &Arc<StreamSession>, uri: &str, ad_id: &str) -> Result<VideoAdHandle> {
    if session.video_source_busy.swap(true, Ordering::SeqCst) {
        return Err(anyhow!(
            "video source already busy (another ad or a replay is in progress)"
        ));
    }

    let bin = gst::Bin::new();
    bin.set_property("name", format!("ad-{ad_id}"));

    let decode = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        .build()
        .context("missing uridecodebin plugin")?;
    let vconv = gst::ElementFactory::make("videoconvert").build()?;
    let vscale = gst::ElementFactory::make("videoscale").build()?;
    let vcaps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", session.canvas_w)
                .field("height", session.canvas_h)
                .build(),
        )
        .build()?;
    let aconv = gst::ElementFactory::make("audioconvert").build()?;
    let aresample = gst::ElementFactory::make("audioresample").build()?;
    let acaps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", AUDIO_SAMPLE_RATE)
                .field("channels", AUDIO_CHANNELS.max(2))
                .build(),
        )
        .build()?;

    for e in [&decode, &vconv, &vscale, &vcaps, &aconv, &aresample, &acaps] {
        bin.add(e).context("add ad element to bin")?;
    }
    gst::Element::link_many([&vconv, &vscale, &vcaps]).context("link ad video leg")?;
    gst::Element::link_many([&aconv, &aresample, &acaps]).context("link ad audio leg")?;

    // uridecodebin's src pads only exist once it has probed the URI —
    // link them to our converters as they appear, by media type.
    let vconv_sink = vconv.static_pad("sink").unwrap();
    let aconv_sink = aconv.static_pad("sink").unwrap();
    decode.connect_pad_added(move |_, pad| {
        let caps = match pad.current_caps() {
            Some(c) => c,
            None => return,
        };
        let Some(s) = caps.structure(0) else { return };
        if s.name().starts_with("video/") && !vconv_sink.is_linked() {
            if let Err(e) = pad.link(&vconv_sink) {
                warn!("ad video pad link failed: {e:?}");
            }
        } else if s.name().starts_with("audio/") && !aconv_sink.is_linked() {
            if let Err(e) = pad.link(&aconv_sink) {
                warn!("ad audio pad link failed: {e:?}");
            }
        }
    });

    let video_src = vcaps.static_pad("src").unwrap();
    let audio_src = acaps.static_pad("src").unwrap();
    let video_ghost = gst::GhostPad::with_target(&video_src)?;
    let audio_ghost = gst::GhostPad::with_target(&audio_src)?;
    video_ghost.set_active(true)?;
    audio_ghost.set_active(true)?;
    bin.add_pad(&video_ghost)?;
    bin.add_pad(&audio_ghost)?;

    session.pipeline.add(&bin).context("add ad bin to pipeline")?;

    let video_pad = session
        .compositor
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("compositor has no free request pad"))?;
    let audio_pad = session
        .audiomixer
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("audiomixer has no free request pad"))?;
    video_pad.set_property("alpha", 0.0f64);
    audio_pad.set_property("volume", 0.0f64);

    video_ghost.link(&video_pad).context("link ad video into compositor")?;
    audio_ghost.link(&audio_pad).context("link ad audio into audiomixer")?;

    bin.sync_state_with_parent()
        .context("failed to sync ad bin state")?;

    // Crossfade the ad in over the live video, and duck live audio under
    // the ad's audio — same stepped-tween primitive as scene transitions.
    transitions::crossfade(&session.live_video_pad, &video_pad);
    transitions::animate_f64(&session.live_audio_pad, "volume", 0.0, TRANSITION_MS);
    transitions::animate_f64(&audio_pad, "volume", 1.0, TRANSITION_MS);

    info!(stream_key = %session.stream_key, ad_id, uri, "video ad started");
    Ok(VideoAdHandle { bin, video_pad, audio_pad })
}

/// Crossfade back to live video/audio and tear down the ad bin. Call once
/// the ad's duration has elapsed (the ads-management loop owns timing).
pub fn stop_video_ad(session: &Arc<StreamSession>, handle: VideoAdHandle) {
    transitions::crossfade(&handle.video_pad, &session.live_video_pad);
    transitions::animate_f64(&handle.audio_pad, "volume", 0.0, TRANSITION_MS);
    transitions::animate_f64(&session.live_audio_pad, "volume", 1.0, TRANSITION_MS);

    let session = session.clone();
    let VideoAdHandle { bin, video_pad, audio_pad } = handle;
    tokio::spawn(async move {
        // Give the crossfade time to finish before we rip the source out
        // from under it.
        tokio::time::sleep(std::time::Duration::from_millis(TRANSITION_MS + 50)).await;
        let _ = bin.set_state(gst::State::Null);
        let _ = session.pipeline.remove(&bin);
        session.compositor.release_request_pad(&video_pad);
        session.audiomixer.release_request_pad(&audio_pad);
        session.video_source_busy.store(false, Ordering::SeqCst);
        info!(stream_key = %session.stream_key, "video ad stopped, back to live");
    });
}

/// Best-effort live playback-rate change for the `replay` command.
///
/// **Caveat, stated plainly rather than papered over:** this sends a seek
/// event with a new `rate` on the pipeline, which is the idiomatic
/// GStreamer way to change speed — but it's designed for seekable sources.
/// The three `appsrc` inputs in this pipeline are live and not seekable, so
/// this will only behave correctly for branches that *are* seekable (e.g.
/// a replay fed from a buffered/recorded clip via `uridecodebin`, the same
/// way `start_video_ad` sources ads). If replay is wired to a genuinely
/// live-only source instead, this call will likely be a no-op or error —
/// that's the PTS-remapping gap flagged in the migration analysis doc, not
/// solved here. Treat this function as "correct for a buffered replay
/// clip", not "solves live speed change in general".
pub fn set_playback_rate(session: &Arc<StreamSession>, rate: f64) -> Result<()> {
    let seek = gst::event::Seek::new(
        rate,
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
        gst::SeekType::None,
        gst::ClockTime::NONE,
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    if !session.pipeline.send_event(seek) {
        return Err(anyhow!(
            "seek event was not handled — likely because the active source is live/non-seekable"
        ));
    }
    info!(stream_key = %session.stream_key, rate, "playback rate change requested");
    Ok(())
}
