//! RFC 6184 H.264-over-RTP reassembly: single-NALU passthrough plus FU-A
//! fragment reassembly. Ported from `com.stream.decode.FrameAssembler`.
//! Emits complete Annex-B NAL units (start-code prefixed) ready to push
//! into the video `appsrc`.

const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
const NAL_TYPE_FU_A: u8 = 28;

pub struct FrameAssembler {
    ssrc: u32,
    in_progress: Vec<u8>,
    assembling: bool,
}

impl FrameAssembler {
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            in_progress: Vec::with_capacity(64 * 1024),
            assembling: false,
        }
    }

    /// Feed one RTP payload (H.264 portion only, RTP header already
    /// stripped). Returns a complete Annex-B NAL unit if this packet
    /// finished one, else `None`.
    pub fn feed(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.is_empty() {
            return None;
        }

        let nal_type = payload[0] & 0x1F;

        if nal_type == NAL_TYPE_FU_A {
            self.feed_fu_a(payload)
        } else {
            // Single-NALU packet: already a complete NAL unit.
            let mut out = Vec::with_capacity(START_CODE.len() + payload.len());
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(payload);
            Some(out)
        }
    }

    fn feed_fu_a(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 2 {
            return None;
        }
        let indicator = payload[0];
        let header = payload[1];
        let start = header & 0x80 != 0;
        let end = header & 0x40 != 0;
        let fragment = &payload[2..];

        if start {
            self.in_progress.clear();
            self.in_progress.extend_from_slice(&START_CODE);
            let reconstructed_nal_header = (indicator & 0xE0) | (header & 0x1F);
            self.in_progress.push(reconstructed_nal_header);
            self.in_progress.extend_from_slice(fragment);
            self.assembling = true;
            None
        } else if self.assembling {
            self.in_progress.extend_from_slice(fragment);
            if end {
                self.assembling = false;
                Some(std::mem::take(&mut self.in_progress))
            } else {
                None
            }
        } else {
            // Fragment arrived without a preceding start fragment — drop.
            None
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
}
