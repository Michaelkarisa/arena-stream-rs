//! Session pipeline construction and live reconfiguration.
//!
//! This is the module that actually replaces `PipeManager` / `FfmpegOutput`
//! from the Java version. Instead of building a `filter_complex` string and
//! spawning `ffmpeg` (torn down and rebuilt for every structural change),
//! we build **one long-lived GStreamer pipeline per session** and mutate it
//! in place:
//!
//! - transitions / resize / reposition / fallback crossfade → GObject
//!   property changes on `compositor` sink pads (see [`transitions`])
//! - adding/removing HLS/RTMP/MP4 destinations while live → `tee` request
//!   pads, added and unlinked without touching the rest of the graph
//!
//! ```text
//!   [fallback: videotestsrc]  → compositor.sink_0  ─┐
//!   [video appsrc → decode]   → compositor.sink_1  ─┼→ compositor → tee(video) ─┬→ branch A (RTMP)
//!   [overlay appsrc BGRA]     → compositor.sink_2  ─┘                            ├→ branch B (HLS)
//!                                                                                 └→ branch C (MP4) [added live]
//!   [audio appsrc] → audiomixer → tee(audio) ───────────────────────────────────→ (same branches)
//! ```

pub mod ads;
pub mod cameras;
pub mod transitions;

use crate::model::{OutputBranch, OutputKind, StreamSession};
use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::Arc;
use tracing::{info, warn};

/// Build the full session pipeline: three appsrc inputs, fallback source,
/// compositor, and the video/audio tees that dynamic outputs attach to.
/// No output branch exists yet — call [`add_output`] once after this to
/// attach the first destination (the same call used later to add more
/// destinations live).
pub fn build_session_pipeline(stream_key: &str, canvas_w: i32, canvas_h: i32) -> Result<Arc<StreamSession>> {
    let desc = format!(
        "appsrc name=vsrc format=time is-live=true do-timestamp=true \
            caps=\"video/x-h264,stream-format=byte-stream,alignment=nal\" ! \
            h264parse ! avdec_h264 ! videoconvert ! videoscale ! \
            video/x-raw,width={cw},height={ch} ! comp.sink_1 \
         \
         videotestsrc name=fallbacksrc is-live=true pattern=black ! \
            video/x-raw,width={cw},height={ch},framerate=30/1 ! comp.sink_0 \
         \
         appsrc name=osrc format=time is-live=true do-timestamp=true \
            caps=\"video/x-raw,format=BGRA,width={ow},height={oh},framerate={ofps}/1\" ! \
            videoconvert ! comp.sink_2 \
         \
         compositor name=comp background=black \
            sink_0::zorder=0 sink_1::zorder=1 sink_2::zorder=2 \
            sink_1::alpha=0.0 sink_0::alpha=1.0 ! \
            video/x-raw,format=I420,width={cw},height={ch},framerate=30/1 ! \
            queue name=vqueue max-size-time=500000000 ! tee name=vtee \
         \
         appsrc name=asrc format=time is-live=true do-timestamp=true \
            caps=\"audio/x-raw,format=S16LE,rate={ar},channels={ac},layout=interleaved\" ! \
            audioconvert ! audioresample ! audiomixer name=amix ! \
            audio/x-raw,rate={ar},channels=2 ! \
            queue name=aqueue max-size-time=500000000 ! tee name=atee",
        // Overlay physical size always equals canvas physical size — see
        // config::canvas_dims_for_quality's doc comment; the two used to be
        // the same fixed constants, now they're the same dynamic values.
        cw = canvas_w,
        ch = canvas_h,
        ow = canvas_w,
        oh = canvas_h,
        ofps = crate::config::OVERLAY_FPS,
        ar = crate::config::AUDIO_SAMPLE_RATE,
        ac = crate::config::AUDIO_CHANNELS,
    );

    // Live video starts invisible (alpha=0) and fallback starts visible
    // (alpha=1) because no camera frames have arrived yet; the fallback
    // monitor task crossfades between them as data starts/stops flowing.
    let element = gst::parse::launch(&desc).context("failed to build session pipeline")?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("parsed graph did not produce a top-level Pipeline"))?;

    let vsrc = get_by_name(&pipeline, "vsrc")?;
    let osrc = get_by_name(&pipeline, "osrc")?;
    let asrc = get_by_name(&pipeline, "asrc")?;
    let comp = get_by_name(&pipeline, "comp")?;
    let vtee = get_by_name(&pipeline, "vtee")?;
    let atee = get_by_name(&pipeline, "atee")?;
    let amix = get_by_name(&pipeline, "amix")?;

    let video_appsrc = vsrc
        .dynamic_cast::<gst_app::AppSrc>()
        .map_err(|_| anyhow!("vsrc is not an AppSrc"))?;
    let overlay_appsrc = osrc
        .dynamic_cast::<gst_app::AppSrc>()
        .map_err(|_| anyhow!("osrc is not an AppSrc"))?;
    let audio_appsrc = asrc
        .dynamic_cast::<gst_app::AppSrc>()
        .map_err(|_| anyhow!("asrc is not an AppSrc"))?;

    let fallback_video_pad = find_pad(&comp, "sink_0")?;
    let live_video_pad = find_pad(&comp, "sink_1")?;
    let overlay_pad = find_pad(&comp, "sink_2")?;
    // Only one sink pad exists on the audiomixer at this point (the main
    // appsrc's) — later ad/replay audio requests their own pads separately.
    let live_audio_pad = amix
        .pads()
        .into_iter()
        .find(|p| p.direction() == gst::PadDirection::Sink)
        .ok_or_else(|| anyhow!("audiomixer has no sink pad yet"))?;

    pipeline
        .set_state(gst::State::Playing)
        .context("failed to set pipeline to PLAYING")?;

    let session = Arc::new(StreamSession::new(
        stream_key.to_string(),
        canvas_w,
        canvas_h,
        pipeline,
        video_appsrc,
        audio_appsrc,
        overlay_appsrc,
        live_video_pad,
        fallback_video_pad,
        overlay_pad,
        live_audio_pad,
        comp,
        amix,
        vtee,
        atee,
    ));

    info!(stream_key, canvas_w, canvas_h, "session pipeline built and PLAYING");
    Ok(session)
}

/// Add a new live output destination (RTMP push URL, HLS playlist
/// directory, or MP4 file path) to a running session — this is the
/// dynamic-output requirement. Uses `tee` request pads, so the rest of the
/// pipeline is completely undisturbed; only the new branch's elements go
/// through a state change.
pub fn add_output(
    session: &StreamSession,
    branch_id: &str,
    kind: OutputKind,
    destination: &str,
) -> Result<()> {
    if session.output_branches.contains_key(branch_id) {
        return Err(anyhow!("output branch '{branch_id}' already exists"));
    }

    let bin = gst::Bin::new();
    bin.set_property("name", format!("branch-{branch_id}"));

    // ── Video leg: queue -> x264enc -> h264parse -> mux ────────────────
    let vqueue = make("queue", &[("max-size-time", &500_000_000u64)])?;
    let x264 = make(
        "x264enc",
        &[
            ("tune", &"zerolatency"),
            ("speed-preset", &"ultrafast"),
            ("bitrate", &crate::config::VIDEO_BITRATE_KBPS),
            ("key-int-max", &crate::config::KEYFRAME_INTERVAL_FRAMES),
        ],
    )?;
    let h264parse = make("h264parse", &[])?;

    // ── Audio leg: queue -> voaacenc -> aacparse ───────────────────────
    let aqueue = make("queue", &[("max-size-time", &500_000_000u64)])?;
    let aacenc = make("voaacenc", &[("bitrate", &crate::config::AUDIO_BITRATE_BPS)])?;
    let aacparse = make("aacparse", &[])?;

    let (mux, sink): (gst::Element, gst::Element) = match kind {
        OutputKind::Rtmp => {
            let mux = make("flvmux", &[("streamable", &true)])?;
            let sink = make("rtmpsink", &[("location", &destination), ("sync", &false)])?;
            (mux, sink)
        }
        OutputKind::Hls => {
            let mux = make("mpegtsmux", &[])?;
            // `destination` is treated as the target directory for this
            // branch; playlist + segment locations derive from it.
            let sink = make(
                "hlssink2",
                &[
                    ("playlist-location", &format!("{destination}/playlist.m3u8")),
                    ("location", &format!("{destination}/segment_%05d.ts")),
                    ("target-duration", &6u32),
                    ("max-files", &10u32),
                ],
            )?;
            (mux, sink)
        }
        OutputKind::Mp4 => {
            // NB: plain `mp4mux` writes the moov atom at EOF, so the file
            // is only reliably playable once the branch is cleanly
            // removed / the session ends. For a file that's playable
            // *while* still being written, swap to `splitmuxsink` with
            // fragmented mp4, or `isofmp4mux` — noted here rather than
            // silently hidden behind "it works".
            let mux = make("mp4mux", &[("faststart", &true)])?;
            let sink = make("filesink", &[("location", &destination)])?;
            (mux, sink)
        }
    };

    for e in [&vqueue, &x264, &h264parse, &aqueue, &aacenc, &aacparse, &mux, &sink] {
        bin.add(e).context("failed to add element to branch bin")?;
    }

    gst::Element::link_many([&vqueue, &x264, &h264parse]).context("link video leg")?;
    gst::Element::link_many([&aqueue, &aacenc, &aacparse]).context("link audio leg")?;
    h264parse
        .link(&mux)
        .context("link video leg into muxer")?;
    aacparse.link(&mux).context("link audio leg into muxer")?;
    mux.link(&sink).context("link mux to sink")?;

    // Ghost pads so the tees (outside the bin) can be linked to elements
    // living inside it.
    let vqueue_sink = vqueue
        .static_pad("sink")
        .ok_or_else(|| anyhow!("vqueue missing sink pad"))?;
    let aqueue_sink = aqueue
        .static_pad("sink")
        .ok_or_else(|| anyhow!("aqueue missing sink pad"))?;
    let video_ghost = gst::GhostPad::with_target(&vqueue_sink)?;
    let audio_ghost = gst::GhostPad::with_target(&aqueue_sink)?;
    video_ghost.set_active(true)?;
    audio_ghost.set_active(true)?;
    bin.add_pad(&video_ghost)?;
    bin.add_pad(&audio_ghost)?;

    session
        .pipeline
        .add(&bin)
        .context("failed to add branch bin to pipeline")?;

    let video_tee_pad = session
        .video_tee
        .request_pad_simple("src_%u")
        .ok_or_else(|| anyhow!("video tee has no free request pad"))?;
    let audio_tee_pad = session
        .audio_tee
        .request_pad_simple("src_%u")
        .ok_or_else(|| anyhow!("audio tee has no free request pad"))?;

    video_tee_pad
        .link(&video_ghost)
        .context("link video tee to branch")?;
    audio_tee_pad
        .link(&audio_ghost)
        .context("link audio tee to branch")?;

    bin.sync_state_with_parent()
        .context("failed to sync new branch state with pipeline")?;

    session.output_branches.insert(
        branch_id.to_string(),
        OutputBranch {
            id: branch_id.to_string(),
            kind,
            video_tee_pad,
            audio_tee_pad,
            bin,
        },
    );

    info!(stream_key = %session.stream_key, branch_id, ?kind, destination, "output branch added live");
    Ok(())
}

/// Remove a live output branch without disturbing the rest of the
/// pipeline: block the tee pad, unlink, release the request pad, drop the
/// branch to NULL, remove it from the pipeline.
pub fn remove_output(session: &StreamSession, branch_id: &str) -> Result<()> {
    let (_, branch) = session
        .output_branches
        .remove(branch_id)
        .ok_or_else(|| anyhow!("output branch '{branch_id}' not found"))?;

    // Block the tee pad so we don't unlink mid-buffer.
    let video_tee_pad = branch.video_tee_pad.clone();
    let audio_tee_pad = branch.audio_tee_pad.clone();
    video_tee_pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
        gst::PadProbeReturn::Ok
    });
    audio_tee_pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
        gst::PadProbeReturn::Ok
    });

    let _ = branch.bin.set_state(gst::State::Null);
    let _ = session.pipeline.remove(&branch.bin);

    if let Some(peer) = video_tee_pad.peer() {
        let _ = video_tee_pad.unlink(&peer);
    }
    if let Some(peer) = audio_tee_pad.peer() {
        let _ = audio_tee_pad.unlink(&peer);
    }
    session.video_tee.release_request_pad(&video_tee_pad);
    session.audio_tee.release_request_pad(&audio_tee_pad);

    info!(stream_key = %session.stream_key, branch_id, "output branch removed live");
    Ok(())
}

pub fn teardown(session: &StreamSession) {
    for entry in session.output_branches.iter() {
        let _ = entry.value().bin.set_state(gst::State::Null);
    }
    // `StreamSession::stop()` owns the final `pipeline.set_state(Null)` —
    // kept as one call here rather than duplicating it inline so there's a
    // single place that does "stop this session's pipeline".
    session.stop();
    warn!(stream_key = %session.stream_key, "session pipeline torn down");
}

// ── helpers ──────────────────────────────────────────────────────────────

fn get_by_name(pipeline: &gst::Pipeline, name: &str) -> Result<gst::Element> {
    pipeline
        .by_name(name)
        .ok_or_else(|| anyhow!("element '{name}' not found in pipeline"))
}

fn find_pad(elem: &gst::Element, name: &str) -> Result<gst::Pad> {
    elem.pads()
        .into_iter()
        .find(|p| p.name() == name)
        .ok_or_else(|| anyhow!("pad '{name}' not found on element"))
}

fn make(factory: &str, props: &[(&str, &dyn ToValueRef)]) -> Result<gst::Element> {
    let el = gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("missing GStreamer plugin providing '{factory}'"))?;
    for (name, value) in props {
        el.set_property_from_str(name, &value.to_value_ref());
    }
    Ok(el)
}

/// Small helper so `make()` can accept a mix of &str/bool/u32/u64 literals
/// without every call site fighting `glib::Value` conversions by hand.
trait ToValueRef {
    fn to_value_ref(&self) -> String;
}
impl ToValueRef for &str {
    fn to_value_ref(&self) -> String {
        self.to_string()
    }
}
impl ToValueRef for String {
    fn to_value_ref(&self) -> String {
        self.clone()
    }
}
impl ToValueRef for bool {
    fn to_value_ref(&self) -> String {
        self.to_string()
    }
}
impl ToValueRef for u32 {
    fn to_value_ref(&self) -> String {
        self.to_string()
    }
}
impl ToValueRef for u64 {
    fn to_value_ref(&self) -> String {
        self.to_string()
    }
}
