use crate::config::{MAX_AUDIO_FRAME_BYTES, UDP_AUDIO_PORT};
use crate::model::REGISTRY;
use gstreamer as gst;
use tokio::net::UdpSocket;
use tracing::{info, warn};

const RTP_AUDIO_PAYLOAD_TYPE: u8 = 111;
const FEC_HEADER_SIZE: usize = 4;
const FEC_MARKER: u8 = 0x01;

/// Server-wide UDP RTP receiver for PCM-16 audio. Equivalent to
/// `com.stream.ingest.UdpAudioIngest`. Only the session's current active
/// streamer's audio is forwarded — secondary camera operators are
/// video-only, same policy as the Java version.
pub async fn run() -> anyhow::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", UDP_AUDIO_PORT)).await?;
    super::sockopt::grow_recv_buffer(&socket, "udp_audio");
    info!(port = UDP_AUDIO_PORT, "udp audio ingest listening");

    let mut buf = vec![0u8; 4096];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!("udp audio recv error: {e}");
                continue;
            }
        };
        if len < 12 {
            continue;
        }
        let data = &buf[..len];
        if (data[0] & 0x7F) != RTP_AUDIO_PAYLOAD_TYPE {
            continue;
        }
        let payload = &data[12..];
        if payload.is_empty() {
            continue;
        }
        if payload.len() >= FEC_HEADER_SIZE && payload[3] == FEC_MARKER {
            continue; // FEC redundancy packet, discard
        }
        if payload.len() > MAX_AUDIO_FRAME_BYTES {
            warn!(len = payload.len(), "oversized audio payload dropped");
            continue;
        }

        let src_ip = src.ip().to_string();
        let session = match REGISTRY.session_for_ip(&src_ip) {
            Some(s) => s,
            None => continue,
        };
        if !session.is_current_streamer(&src_ip) {
            continue;
        }

        session.touch();
        let buffer = gst::Buffer::from_slice(payload.to_vec());
        if let Err(e) = session.audio_appsrc.push_buffer(buffer) {
            warn!(stream_key = %session.stream_key, "audio appsrc push failed: {e}");
        }
    }
}
