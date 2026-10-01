use super::frame_assembler::FrameAssembler;
use crate::config::UDP_VIDEO_PORT;
use crate::model::REGISTRY;
use gstreamer as gst;
use gstreamer_app::prelude::*;
use std::collections::HashMap;
use tokio::net::UdpSocket;
use tracing::{info, warn};

const RTP_H264_PAYLOAD_TYPE: u8 = 96;
const MAX_ASSEMBLERS: usize = 64;

/// Server-wide UDP RTP receiver for H.264 video. Equivalent to
/// `com.stream.ingest.UdpVideoIngest`: one socket, all sessions
/// demultiplexed by sender IP via the session registry.
///
/// **Per-camera routing, not a single shared appsrc.** This used to push
/// every recognized packet into `session.video_appsrc` regardless of which
/// client sent it — meaning two camera operators on the same session would
/// have their independently-encoded H.264 streams interleaved into one
/// appsrc, which is invalid input for `h264parse`/`avdec_h264` downstream.
/// Fixed as part of building out real multi-camera support (WORKFLOW.md
/// §7.4): each sender IP now gets routed to its own `CameraInput`'s
/// appsrc, created on demand via `pipeline::cameras::add_camera` if it
/// wasn't already built at `register` time.
pub async fn run() -> anyhow::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", UDP_VIDEO_PORT)).await?;
    super::sockopt::grow_recv_buffer(&socket, "udp_video");
    info!(port = UDP_VIDEO_PORT, "udp video ingest listening");

    let mut assemblers: HashMap<u32, FrameAssembler> = HashMap::new();
    let mut buf = vec![0u8; 2048];

    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!("udp video recv error: {e}");
                continue;
            }
        };
        if len < 13 {
            continue;
        }
        let data = &buf[..len];
        if (data[0] & 0x7F) != RTP_H264_PAYLOAD_TYPE {
            continue;
        }
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let payload = &data[12..];
        if payload.is_empty() {
            continue;
        }

        let src_ip = src.ip().to_string();
        let session = match REGISTRY.session_for_ip(&src_ip) {
            Some(s) => s,
            None => continue,
        };

        if assemblers.len() > MAX_ASSEMBLERS && !assemblers.contains_key(&ssrc) {
            // Evict an arbitrary old entry rather than growing unbounded —
            // mirrors the Java LinkedHashMap eviction policy.
            if let Some(k) = assemblers.keys().next().copied() {
                if let Some(evicted) = assemblers.remove(&k) {
                    warn!(ssrc = evicted.ssrc(), "evicted stale frame assembler to bound memory use");
                }
            }
        }
        let assembler = assemblers.entry(ssrc).or_insert_with(|| FrameAssembler::new(ssrc));

        if let Some(nal_unit) = assembler.feed(payload) {
            session.mark_video_buffer();
            session.touch();

            // Lazily build this camera's branch if `register` never got a
            // chance to (or this IP skipped straight to sending video).
            if !session.cameras.contains_key(&src_ip) {
                if let Err(e) = crate::pipeline::cameras::add_camera(&session, &src_ip) {
                    warn!(stream_key = %session.stream_key, src_ip, "lazy camera branch creation failed: {e}");
                    continue;
                }
                // First camera ever seen for this session becomes active
                // by default so something is actually visible.
                if session.active_camera_ip.read().unwrap().is_none() {
                    *session.active_camera_ip.write().unwrap() = Some(src_ip.clone());
                    if let Some(cam) = session.cameras.get(&src_ip) {
                        cam.pad().set_property("alpha", 1.0f64);
                    }
                }
            }

            let Some(camera) = session.cameras.get(&src_ip) else {
                continue; // shouldn't happen given the add_camera call above, but don't panic if it does
            };
            let buffer = gst::Buffer::from_slice(nal_unit);
            if let Err(e) = camera.appsrc().push_buffer(buffer) {
                warn!(stream_key = %session.stream_key, src_ip, "video appsrc push failed: {e}");
            }
        }
    }
}

