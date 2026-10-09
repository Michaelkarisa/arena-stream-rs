//! Network-management socket (TCP, `config::MANAGE_NET_PORT`).
//!
//! Every camera in a session except the live one acts as a network probe for
//! the live one: roughly every 10 s it sends an IDR frame to this socket and
//! the server times the transfer. Standby cameras share the venue network
//! with the live camera, so the average of their speeds is the best available
//! estimate of how healthy the live camera's uplink is. When that estimate
//! drops below what the session has recently seen, the live device is told to
//! cap its frame rate; when it recovers, the cap is lifted.
//!
//! ## Frame-rate policy
//! * `drop = 1 - avg_kbps / baseline_kbps`, where `baseline_kbps` is the
//!   highest session average over the last `MANAGE_BASELINE_WINDOW` readings.
//! * Drops under `MANAGE_DROP_DEADBAND` are noise: no cap (30 fps).
//! * Otherwise `cap_fps = 30 * (1 - drop)`: a 10% drop caps to 27 fps.
//! * The cap never goes below `MANAGE_MIN_FPS` (18 fps, a 40% drop) — past
//!   that the video looks sluggish.
//!
//! ## Which frames to skip (done on the Android device)
//! Dropped frames are *spaced out*, never adjacent, and an IDR is never
//! dropped. Each `cap_fps` message carries `skip_frames`: 0-based frame
//! indices within every 30-frame group (index 0 is the IDR, the group's last
//! frame is also kept). At 30 fps (33 ms/frame) a 10% drop skips indices
//! `[1, 14, 28]`, i.e. the frames at 33 ms, 462 ms and 924 ms.
//!
//! ## Wire protocol
//! Newline-delimited JSON, like the control socket. Device -> server:
//! * `{"action":"register","matchId":"<stream key>"}` — once, after connect.
//!   (`streamKey` is accepted in place of `matchId`.) May arrive before the
//!   session has been `start`ed; the cap applies once it exists.
//! * `{"action":"probe","matchId":"...","bytes":N}\n` immediately followed by
//!   exactly `N` raw bytes (the IDR, Annex-B). Probes from the live device
//!   are read but not used for the estimate.
//!
//! Server -> device:
//! * `{"event":"registered","stream_key":"...","role":"live"|"standby"}`
//! * `{"event":"probe_ack","kbps":...,"used":true|false}`
//! * `{"event":"cap_fps","fps":27,"base_fps":30,"group_size":30,
//!    "skip_frames":[1,14,28],"drop_percent":10.0,"est_kbps":...,
//!    "baseline_kbps":...}` — sent to the live device when its cap changes
//!   (`fps == base_fps` with an empty `skip_frames` means "uncapped"), and
//!   also to a device that stops being live, to lift any cap it still has.

use crate::config::{
    MANAGE_BASELINE_WINDOW, MANAGE_BASE_FPS, MANAGE_DROP_DEADBAND, MANAGE_EVAL_INTERVAL_MS,
    MANAGE_MIN_FPS, MANAGE_NET_PORT, MANAGE_PROBE_BODY_TIMEOUT_MS, MANAGE_PROBE_MAX_BYTES,
    MANAGE_PROBE_MIN_BYTES, MANAGE_SAMPLE_TTL_MS,
};
use crate::model::{now_ms, StreamSession, REGISTRY};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::{debug, info, warn};

/// Longest JSON header line accepted (probe/register lines are tiny).
const MAX_LINE_BYTES: u64 = 8 * 1024;

struct Sample {
    kbps: f64,
    at_ms: i64,
}

struct Conn {
    id: u64,
    tx: UnboundedSender<String>,
    /// Last fps cap this device was told about; `None` = nothing sent on
    /// this connection yet, so the next evaluation always sends one.
    last_fps: Option<u32>,
}

#[derive(Default)]
struct SessionNet {
    /// Latest probe sample per standby camera IP.
    samples: HashMap<String, Sample>,
    /// Recent session-average readings, newest last.
    history: VecDeque<f64>,
    conns: HashMap<String, Conn>,
}

static NET: Lazy<DashMap<String, Arc<Mutex<SessionNet>>>> = Lazy::new(DashMap::new);
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

pub async fn run() -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", MANAGE_NET_PORT)).await?;
    info!(port = MANAGE_NET_PORT, "manage net socket listening");

    tokio::spawn(evaluation_loop());

    loop {
        let (socket, addr) = listener.accept().await?;
        let peer_ip = addr.ip().to_string();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, peer_ip.clone()).await {
                warn!(peer_ip, "manage net connection ended: {e}");
            }
        });
    }
}

/// Periodic re-evaluation: picks up a `switch` of the live camera, samples
/// going stale, and forgets sessions that no longer exist.
async fn evaluation_loop() {
    let mut interval = tokio::time::interval(Duration::from_millis(MANAGE_EVAL_INTERVAL_MS));
    loop {
        interval.tick().await;
        let keys: Vec<String> = NET.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            if REGISTRY.get(&key).is_none() {
                // Session gone: drop the entry once no device is connected.
                let idle = NET.get(&key).map(|e| e.value().lock().unwrap().conns.is_empty()).unwrap_or(true);
                if idle {
                    NET.remove(&key);
                }
                continue;
            }
            reevaluate(&key);
        }
    }
}

async fn handle_connection(socket: TcpStream, peer_ip: String) -> anyhow::Result<()> {
    let (read_half, mut write_half) = socket.into_split();
    let mut reader = BufReader::new(read_half);

    let (tx, mut rx) = unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_half.write_all(msg.as_bytes()).await.is_err() {
                break;
            }
            let _ = write_half.write_all(b"\n").await;
        }
    });

    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    let mut current_key: Option<String> = None;

    let result = connection_loop(&mut reader, &tx, &peer_ip, conn_id, &mut current_key).await;

    if let Some(key) = &current_key {
        if let Some(net) = NET.get(key).map(|e| e.value().clone()) {
            let mut st = net.lock().unwrap();
            if st.conns.get(&peer_ip).map(|c| c.id) == Some(conn_id) {
                st.conns.remove(&peer_ip);
            }
        }
    }
    result
}

async fn connection_loop(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    tx: &UnboundedSender<String>,
    peer_ip: &str,
    conn_id: u64,
    current_key: &mut Option<String>,
) -> anyhow::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut *reader).take(MAX_LINE_BYTES).read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // peer closed
        }
        if !line.ends_with('\n') {
            anyhow::bail!("header line longer than {MAX_LINE_BYTES} bytes");
        }
        if line.trim().is_empty() {
            continue;
        }
        let cmd: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                warn!(peer_ip, "invalid JSON on manage net: {e}");
                continue;
            }
        };
        let key = cmd
            .get("streamKey")
            .or_else(|| cmd.get("matchId"))
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| current_key.clone())
            .or_else(|| REGISTRY.session_for_ip(peer_ip).map(|s| s.stream_key.clone()));

        match cmd.get("action").and_then(Value::as_str).unwrap_or("") {
            "register" => {
                let Some(key) = key else { continue };
                // The device opens this socket when its camera starts, which is
                // normally *before* the control socket's `start` has created the
                // session — so registering must work with no session yet.
                // Evaluation (and any cap) begins once the session exists.
                let net = NET.entry(key.clone()).or_default().value().clone();
                net.lock().unwrap().conns.insert(
                    peer_ip.to_string(),
                    Conn { id: conn_id, tx: tx.clone(), last_fps: None },
                );
                *current_key = Some(key.clone());
                let role = match REGISTRY.get(&key) {
                    Some(session) if live_ip(&session).as_deref() == Some(peer_ip) => "live",
                    _ => "standby",
                };
                let _ = tx.send(
                    json!({ "event": "registered", "stream_key": key, "role": role }).to_string(),
                );
                info!(stream_key = key, peer_ip, role, "manage net client registered");
                reevaluate(&key);
            }

            "probe" => {
                let Some(bytes) = cmd.get("bytes").and_then(Value::as_u64).map(|b| b as usize) else {
                    warn!(peer_ip, "probe without 'bytes'");
                    continue;
                };
                if !(MANAGE_PROBE_MIN_BYTES..=MANAGE_PROBE_MAX_BYTES).contains(&bytes) {
                    // The body is already on its way; we can't resync the
                    // stream without reading it, so refuse and hang up.
                    anyhow::bail!("probe size {bytes} out of range");
                }

                let started = Instant::now();
                let mut received = 0usize;
                let mut chunk = [0u8; 16 * 1024];
                let body = tokio::time::timeout(
                    Duration::from_millis(MANAGE_PROBE_BODY_TIMEOUT_MS),
                    async {
                        while received < bytes {
                            let want = chunk.len().min(bytes - received);
                            let got = reader.read(&mut chunk[..want]).await?;
                            if got == 0 {
                                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
                            }
                            received += got;
                        }
                        Ok(())
                    },
                )
                .await;
                let elapsed_ms = (started.elapsed().as_secs_f64() * 1000.0).max(1.0);
                let kbps = received as f64 * 8.0 / elapsed_ms;

                let timed_out = body.is_err();
                if let Ok(Err(e)) = body {
                    return Err(e.into());
                }

                let Some(key) = key else { continue };
                let used = record_probe(&key, peer_ip, kbps);
                let _ = tx.send(json!({ "event": "probe_ack", "kbps": kbps.round(), "used": used }).to_string());
                if timed_out {
                    // Partial transfer recorded above as a (very low) sample;
                    // the unread remainder desynchronises the stream.
                    anyhow::bail!("probe body timed out after {received}/{bytes} bytes");
                }
            }

            other => debug!(peer_ip, action = other, "ignoring unknown manage net action"),
        }
    }
}

/// The camera whose video is currently on air.
fn live_ip(session: &StreamSession) -> Option<String> {
    session
        .active_camera_ip
        .read()
        .unwrap()
        .clone()
        .or_else(|| session.current_streamer.read().unwrap().clone())
}

/// Stores a probe sample and re-evaluates the session. Returns whether the
/// sample counted (probes from the live device don't).
fn record_probe(key: &str, ip: &str, kbps: f64) -> bool {
    let Some(session) = REGISTRY.get(key) else { return false };
    session.touch();
    if live_ip(&session).as_deref() == Some(ip) {
        debug!(stream_key = key, ip, "ignoring probe from the live device");
        return false;
    }
    let net = NET.entry(key.to_string()).or_default().value().clone();
    {
        let mut st = net.lock().unwrap();
        let now = now_ms();
        st.samples.insert(ip.to_string(), Sample { kbps, at_ms: now });
        if let Some(avg) = fresh_average(&st.samples, now, None) {
            st.history.push_back(avg);
            while st.history.len() > MANAGE_BASELINE_WINDOW {
                st.history.pop_front();
            }
        }
    }
    reevaluate(key);
    true
}

fn fresh_average(samples: &HashMap<String, Sample>, now: i64, exclude: Option<&str>) -> Option<f64> {
    let fresh: Vec<f64> = samples
        .iter()
        .filter(|(ip, s)| now - s.at_ms <= MANAGE_SAMPLE_TTL_MS && Some(ip.as_str()) != exclude)
        .map(|(_, s)| s.kbps)
        .collect();
    (!fresh.is_empty()).then(|| fresh.iter().sum::<f64>() / fresh.len() as f64)
}

/// Recomputes the live device's fps cap for one session and tells every
/// connected device whose cap is out of date.
fn reevaluate(key: &str) {
    // No session (yet, or any more): nothing to evaluate. Stale entries are
    // pruned by `evaluation_loop`.
    let Some(session) = REGISTRY.get(key) else { return };
    let Some(net) = NET.get(key).map(|e| e.value().clone()) else { return };
    let live = live_ip(&session);
    let now = now_ms();

    let mut st = net.lock().unwrap();
    st.samples.retain(|_, s| now - s.at_ms <= MANAGE_SAMPLE_TTL_MS);

    // Live device's target: `None` = no fresh data, hold whatever it has.
    let live_plan = fresh_average(&st.samples, now, live.as_deref()).map(|avg| {
        let baseline = st.history.iter().copied().fold(avg, f64::max);
        let (fps, drop) = cap_fps_for(avg, baseline);
        (fps, drop, avg, baseline)
    });

    for (ip, conn) in st.conns.iter_mut() {
        let is_live = live.as_deref() == Some(ip.as_str());
        let (fps, drop, est, base) = if is_live {
            match live_plan {
                Some(p) => p,
                None => continue,
            }
        } else {
            // Not live (any more): make sure it isn't left throttled.
            (MANAGE_BASE_FPS, 0.0, 0.0, 0.0)
        };
        if conn.last_fps == Some(fps) {
            continue;
        }
        let msg = json!({
            "event": "cap_fps",
            "fps": fps,
            "base_fps": MANAGE_BASE_FPS,
            "group_size": MANAGE_BASE_FPS,
            "skip_frames": skip_indices(MANAGE_BASE_FPS, fps),
            "drop_percent": (drop * 1000.0).round() / 10.0,
            "est_kbps": est.round(),
            "baseline_kbps": base.round(),
        });
        if conn.tx.send(msg.to_string()).is_ok() {
            conn.last_fps = Some(fps);
            if is_live {
                info!(stream_key = key, ip, fps, drop_percent = drop * 100.0, "live device fps cap updated");
            }
        }
    }
}

/// `(capped fps, drop fraction)` for a session-average speed against its
/// baseline. See the module docs for the policy.
fn cap_fps_for(avg_kbps: f64, baseline_kbps: f64) -> (u32, f64) {
    let drop = if baseline_kbps > 0.0 { (1.0 - avg_kbps / baseline_kbps).clamp(0.0, 1.0) } else { 0.0 };
    if drop < MANAGE_DROP_DEADBAND {
        return (MANAGE_BASE_FPS, 0.0);
    }
    let fps = (MANAGE_BASE_FPS as f64 * (1.0 - drop)).round() as u32;
    (fps.clamp(MANAGE_MIN_FPS, MANAGE_BASE_FPS), drop)
}

/// 0-based indices to skip within each `base`-frame group when running at
/// `fps`: evenly spaced between index 1 and index `base - 2`, so index 0 (the
/// IDR) and the group's last frame are never dropped and no two dropped
/// frames are adjacent (the spacing is always > 2 for the permitted range).
fn skip_indices(base: u32, fps: u32) -> Vec<u32> {
    let skip = base.saturating_sub(fps);
    if skip == 0 || base < 4 {
        return Vec::new();
    }
    let last = base - 2;
    if skip == 1 {
        return vec![1];
    }
    (0..skip)
        .map(|k| 1 + (k as f64 * (last - 1) as f64 / (skip - 1) as f64).floor() as u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_percent_drop_caps_to_27_and_skips_spaced_frames() {
        let (fps, drop) = cap_fps_for(900.0, 1000.0);
        assert_eq!(fps, 27);
        assert!((drop - 0.1).abs() < 1e-9);
        // 33 ms, 462 ms, 924 ms at 33 ms/frame; never 0 (IDR) or 29 (last).
        assert_eq!(skip_indices(30, 27), vec![1, 14, 28]);
    }

    #[test]
    fn cap_never_below_18_and_noise_ignored() {
        assert_eq!(cap_fps_for(100.0, 1000.0).0, 18);
        assert_eq!(cap_fps_for(990.0, 1000.0).0, 30);
        assert_eq!(cap_fps_for(1200.0, 1000.0), (30, 0.0));
    }

    #[test]
    fn skips_are_never_idr_last_or_adjacent() {
        for fps in MANAGE_MIN_FPS..=MANAGE_BASE_FPS {
            let s = skip_indices(30, fps);
            assert_eq!(s.len() as u32, 30 - fps);
            assert!(s.iter().all(|&i| i >= 1 && i <= 28));
            assert!(s.windows(2).all(|w| w[1] - w[0] >= 2), "adjacent at {fps}: {s:?}");
        }
    }
}
