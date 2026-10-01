//! TCP control socket — newline-delimited JSON commands from mobile
//! clients. Mirrors `com.stream.control.ControlSocketHandler`'s dispatch
//! table. Actions: `register`/`start`/`stop`/`switch`/`add_output`/
//! `remove_output`/`remove_camera`/`overlay`/`list_outputs`/`goal`/`card`/
//! `substitute`/`lineup`/`replay`/`replay_end`/`set_overlay_delay` are fully
//! wired end-to-end (parse payload, mutate `OverlayState` and/or the
//! pipeline, fire-and-forget log via `api::CLIENT`, fan out to other
//! clients).

use crate::api::CLIENT;
use crate::config::TCP_CONTROL_PORT;
use crate::model::{OutputKind, REGISTRY};
use crate::overlay::{LineupState, OverlayState, TeamLineupData};
use crate::pipeline;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::{info, warn};

/// Fan-out registry: stream_key -> connected client write-halves.
static FANOUT: Lazy<DashMap<String, Vec<UnboundedSender<String>>>> = Lazy::new(DashMap::new);

/// Overlay state per session, separate from the media-pipeline session so
/// the render loop (`overlay::spawn_overlay_renderer`) can be started
/// independently of pipeline construction.
static OVERLAY_STATES: Lazy<DashMap<String, Arc<OverlayState>>> = Lazy::new(DashMap::new);

/// What `register` received, held until `start` actually builds the
/// session. `register` fires before any pipeline exists (see its handler's
/// comment on `add_camera`'s best-effort pre-build), so there's nowhere to
/// store the registered quality or matchData at that point — `start` is
/// what calls `pipeline::build_session_pipeline`, and that's what needs
/// them, to size the canvas correctly and seed team names/lineup instead of
/// leaving the overlay on its "HOME"/"AWAY" placeholders. Removed once
/// `start` consumes it (or overwritten by a fresher `register`, if a client
/// re-registers before `start` arrives).
struct PendingRegistration {
    quality: i32,
    match_data: Option<Value>,
    other_match_data: Option<Value>,
}
static PENDING_REGISTRATIONS: Lazy<DashMap<String, PendingRegistration>> = Lazy::new(DashMap::new);

/// Accessor for other modules (the ads-management loop) that need to flip
/// overlay flags without owning the control socket's internals.
pub fn overlay_state_for(stream_key: &str) -> Option<Arc<OverlayState>> {
    OVERLAY_STATES.get(stream_key).map(|e| e.value().clone())
}

pub async fn run() -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", TCP_CONTROL_PORT)).await?;
    info!(port = TCP_CONTROL_PORT, "control socket listening");

    loop {
        let (socket, addr) = listener.accept().await?;
        let peer_ip = addr.ip().to_string();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, peer_ip.clone()).await {
                warn!(peer_ip, "control connection ended: {e}");
            }
        });
    }
}

async fn handle_connection(socket: TcpStream, peer_ip: String) -> anyhow::Result<()> {
    let (read_half, mut write_half) = socket.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let (tx, mut rx) = unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_half.write_all(msg.as_bytes()).await.is_err() {
                break;
            }
            let _ = write_half.write_all(b"\n").await;
        }
    });

    let mut current_stream_key: Option<String> = None;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let cmd: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(peer_ip, "invalid JSON command: {e}");
                continue;
            }
        };
        let action = cmd.get("action").and_then(Value::as_str).unwrap_or("");
        // The Android app stamps every command with "matchId" (see
        // addCommand in services.dart) and the Kotlin bridge forwards it
        // verbatim — it never injects a "streamKey". Since the stream key
        // *is* the match id (api/mod.rs), accept either; "streamKey" wins
        // if both are present so older/other clients keep working.
        let stream_key = cmd
            .get("streamKey")
            .or_else(|| cmd.get("matchId"))
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| current_stream_key.clone());

        match action {
            "register" => {
                if let Some(key) = &stream_key {
                    current_stream_key = Some(key.clone());
                    REGISTRY.register_ip(&peer_ip, key);

                    // Default 720 matches both the app's own MatchData
                    // default (see models.dart) and canvas_dims_for_quality's
                    // fallback, so an old client that omits data.quality
                    // entirely still gets the exact same canvas it always
                    // has.
                    let quality = cmd
                        .get("data")
                        .and_then(|d| d.get("quality"))
                        .and_then(Value::as_i64)
                        .unwrap_or(720) as i32;
                    PENDING_REGISTRATIONS.insert(
                        key.clone(),
                        PendingRegistration {
                            quality,
                            match_data: cmd.get("matchData").cloned().filter(|v| !v.is_null()),
                            other_match_data: cmd.get("otherMatchData").cloned().filter(|v| !v.is_null()),
                        },
                    );

                    if let Some(session) = REGISTRY.get(key) {
                        session.clients.insert(peer_ip.clone());
                        // Eagerly build this camera's compositor branch so
                        // it's ready for `switch` even before its first
                        // video packet arrives. Best-effort: a session that
                        // hasn't been `start`ed yet has no pipeline to add
                        // to, so this just no-ops via the Err path below —
                        // ingest::udp_video falls back to lazy creation on
                        // first packet regardless.
                        if let Err(e) = pipeline::cameras::add_camera(&session, &peer_ip) {
                            warn!(stream_key = key, peer_ip, "camera branch not pre-built at register: {e}");
                        }
                    }
                    FANOUT.entry(key.clone()).or_default().push(tx.clone());
                    broadcast(key, "list updated");
                    info!(stream_key = key, peer_ip, "client registered");
                }
            }

            "start" => {
                let Some(key) = stream_key.clone() else { continue };
                if REGISTRY.get(&key).is_none() {
                    // Consumed once here; a later "start" for the same key
                    // (there shouldn't be one, since the check above already
                    // guards against it) would just fall back to defaults.
                    let pending = PENDING_REGISTRATIONS.remove(&key).map(|(_, p)| p);
                    let quality = pending.as_ref().map(|p| p.quality).unwrap_or(720);
                    let (canvas_w, canvas_h) = crate::config::canvas_dims_for_quality(quality);

                    // matchData/otherMatchData travel with "register" (see
                    // that handler), but are only "sufficient" if they
                    // actually carry both team names — an app that registers
                    // before it's finished loading match details, or an
                    // older client that never sends matchData at all, would
                    // otherwise leave the overlay on "HOME"/"AWAY"
                    // placeholders for the whole session. Fall back to
                    // fetching the authoritative record from Laravel rather
                    // than accept that.
                    let match_data = match pending.and_then(|p| p.match_data) {
                        Some(md) if has_usable_team_names(&md) => Some(md),
                        _ => {
                            info!(stream_key = key, "matchData from register missing/insufficient, fetching from Laravel");
                            CLIENT.get_match_awaited(&key).await
                        }
                    };

                    match pipeline::build_session_pipeline(&key, canvas_w, canvas_h) {
                        Ok(session) => {
                            REGISTRY.put(&key, session.clone());
                            REGISTRY.register_ip(&peer_ip, &key);
                            session.clients.insert(peer_ip.clone());
                            *session.current_streamer.write().unwrap() = Some(peer_ip.clone());
                            // Wire the client that issued `start` in as the
                            // primary camera — see model::CameraInput and
                            // pipeline::cameras module docs. Starts visible
                            // (alpha=1) since it's the only camera so far;
                            // fallback crossfade (alpha=0/1 vs the fallback
                            // pad) still governs actual visibility until
                            // video buffers start flowing.
                            session.cameras.insert(
                                peer_ip.clone(),
                                crate::model::CameraInput::Primary {
                                    appsrc: session.video_appsrc.clone(),
                                    pad: session.live_video_pad.clone(),
                                },
                            );
                            *session.active_camera_ip.write().unwrap() = Some(peer_ip.clone());

                            let overlay_state = Arc::new(OverlayState::default());
                            overlay_state.live.store(true, std::sync::atomic::Ordering::Relaxed);
                            if let Some(md) = &match_data {
                                seed_overlay_from_match_data(&overlay_state, md);
                            } else {
                                warn!(stream_key = key, "starting with no match data — overlay will show placeholder team names");
                            }
                            OVERLAY_STATES.insert(key.clone(), overlay_state.clone());
                            crate::overlay::spawn_overlay_renderer(session.clone(), overlay_state);
                            pipeline::transitions::spawn_fallback_monitor(session.clone());
                            crate::ads::spawn_ads_loop(session.clone());

                            // Attach the default RTMP destination if one was
                            // supplied at start time; more outputs (HLS/MP4,
                            // or additional RTMP targets) can be added later
                            // via the `add_output` action without touching
                            // this pipeline.
                            if let Some(url) = cmd.get("rtmpUrl").and_then(Value::as_str) {
                                if let Err(e) =
                                    pipeline::add_output(&session, "default", OutputKind::Rtmp, url)
                                {
                                    warn!(stream_key = key, "failed to add default output: {e}");
                                }
                            }

                            // Awaited (unlike the rest of this client) so we
                            // capture the real Laravel row id for event
                            // logging below — see `StreamSession::laravel_id`
                            // doc comment for why this one call is special.
                            match CLIENT.create_session_awaited(&key, &[]).await {
                                Some(result) => {
                                    session.set_laravel_id(result.id);
                                    if let Some(broadcaster_id) = result.broadcaster_id {
                                        session.set_broadcaster_id(broadcaster_id);
                                    } else {
                                        warn!(stream_key = key, "session created but match has no author_id; rank points won't be awarded");
                                    }
                                }
                                None => {
                                    warn!(stream_key = key, "no Laravel session id yet; events will be skipped until a later sync");
                                    // Best-effort fallback: fire-and-forget the
                                    // plain (non-awaited) create call so the
                                    // backend at least learns about this
                                    // session even though we won't have a row
                                    // id to log events against this run.
                                    CLIENT.create_session(&key, &[]);
                                }
                            }
                            CLIENT.update_session_status(&key, "live");
                            broadcast(&key, "started");
                            info!(stream_key = key, "session started");
                        }
                        Err(e) => warn!(stream_key = key, "failed to start session: {e}"),
                    }
                }
            }

            "stop" => {
                let Some(key) = stream_key.clone() else { continue };
                if let Some(session) = REGISTRY.remove(&key) {
                    pipeline::teardown(&session);
                    OVERLAY_STATES.remove(&key);
                    FANOUT.remove(&key);
                    CLIENT.update_session_status(&key, "finished");
                    broadcast(&key, "stopped");
                    info!(stream_key = key, "session stopped");
                }
            }

            "switch" => {
                // Switch the active streamer: crossfades video via
                // pipeline::cameras::switch_camera (§7.4 fix — this used to
                // only update audio authority) and updates who's
                // audio-authoritative in the same command, since a scene
                // switch implies both in practice.
                if let (Some(key), Some(new_ip)) =
                    (stream_key.clone(), cmd.get("targetIp").and_then(Value::as_str))
                {
                    if let Some(session) = REGISTRY.get(&key) {
                        match pipeline::cameras::switch_camera(&session, new_ip) {
                            Ok(()) => {
                                *session.current_streamer.write().unwrap() = Some(new_ip.to_string());
                                broadcast(&key, &format!("switched:{new_ip}"));
                                info!(stream_key = key, new_ip, "active streamer switched (video crossfaded)");
                            }
                            Err(e) => warn!(stream_key = key, new_ip, "switch failed: {e}"),
                        }
                    }
                }
            }

            "add_output" => {
                let Some(key) = stream_key.clone() else { continue };
                let (Some(id), Some(kind_str), Some(dest)) = (
                    cmd.get("id").and_then(Value::as_str),
                    cmd.get("kind").and_then(Value::as_str),
                    cmd.get("destination").and_then(Value::as_str),
                ) else { continue };
                let kind = match kind_str {
                    "rtmp" => OutputKind::Rtmp,
                    "hls" => OutputKind::Hls,
                    "mp4" => OutputKind::Mp4,
                    other => {
                        warn!("unknown output kind '{other}'");
                        continue;
                    }
                };
                if let Some(session) = REGISTRY.get(&key) {
                    match pipeline::add_output(&session, id, kind, dest) {
                        Ok(()) => broadcast(&key, &format!("output_added:{id}")),
                        Err(e) => warn!(stream_key = key, "add_output failed: {e}"),
                    }
                }
            }

            "remove_output" => {
                let Some(key) = stream_key.clone() else { continue };
                let Some(id) = cmd.get("id").and_then(Value::as_str) else { continue };
                if let Some(session) = REGISTRY.get(&key) {
                    match pipeline::remove_output(&session, id) {
                        Ok(()) => broadcast(&key, &format!("output_removed:{id}")),
                        Err(e) => warn!(stream_key = key, "remove_output failed: {e}"),
                    }
                }
            }

            "resize" => {
                // Live resize/reposition to make room for an image ad.
                let Some(key) = stream_key.clone() else { continue };
                if let Some(session) = REGISTRY.get(&key) {
                    let x = cmd.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
                    let y = cmd.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
                    let w = cmd.get("width").and_then(Value::as_i64).unwrap_or(session.canvas_w as i64) as i32;
                    let h = cmd.get("height").and_then(Value::as_i64).unwrap_or(session.canvas_h as i64) as i32;
                    pipeline::transitions::resize_and_reposition(
                        &session.live_video_pad,
                        x,
                        y,
                        w,
                        h,
                        crate::config::TRANSITION_MS,
                    );
                }
            }

            "state" => {
                // Session lifecycle transition: start/live/ht/et/ft/stopped.
                // Drives which ads are eligible in the ads-management loop.
                let Some(key) = stream_key.clone() else { continue };
                let Some(state_str) = cmd.get("value").and_then(Value::as_str) else { continue };
                let Some(new_state) = crate::model::SessionState::parse(state_str) else {
                    warn!("unknown session state '{state_str}'");
                    continue;
                };
                if let Some(session) = REGISTRY.get(&key) {
                    session.set_state(new_state);
                    if let Some(id) = session.laravel_id() {
                        CLIENT.log_event(&id, "state", cmd.clone());
                    } else {
                        warn!(stream_key = key, "state event dropped: no Laravel session id yet");
                    }
                    // `add_goal`'s sibling endpoint for the match record's
                    // own status column — see the "streamKey is the
                    // match_id" note at the top of api/mod.rs. Best-effort:
                    // an empty stream key just skips this, same as
                    // `add_goal` below.
                    if let Some(match_id) = match_id_from_key(&key) {
                        CLIENT.update_match_status(match_id, session_state_status_code(new_state));
                    }
                    // Kick off the "match center" popup (`draw_match_performance_widget`)
                    // whenever play actually starts/resumes — kickoff and
                    // the two restarts (half-time, extra-time) all map to
                    // `Live`, matching how a broadcast would re-announce
                    // "kick off!" each time.
                    if new_state == crate::model::SessionState::Live {
                        if let Some(overlay_state) = OVERLAY_STATES.get(&key) {
                            let mut ms = overlay_state.match_state.write().unwrap();
                            ms.is_active = true;
                            ms.animation_start_time = crate::overlay::graphics::current_time_millis();
                            ms.phase = 0;
                        }
                    }
                    broadcast(&key, &format!("state:{}", new_state.as_str()));
                    info!(stream_key = key, state = new_state.as_str(), "session state changed");
                }
            }

            "replay" => {
                let Some(key) = stream_key.clone() else { continue };
                let rate = cmd.get("speed").and_then(Value::as_f64).unwrap_or(0.5);
                if let Some(session) = REGISTRY.get(&key) {
                    match crate::ads::start_replay(&session, rate) {
                        Ok(()) => broadcast(&key, "replay_started"),
                        Err(e) => warn!(stream_key = key, "replay failed: {e}"),
                    }
                }
            }

            "replay_end" => {
                let Some(key) = stream_key.clone() else { continue };
                if let Some(session) = REGISTRY.get(&key) {
                    match crate::ads::end_replay(&session) {
                        Ok(()) => broadcast(&key, "replay_ended"),
                        Err(e) => warn!(stream_key = key, "replay_end failed: {e}"),
                    }
                }
            }

            "goal" => {
                let Some(key) = stream_key.clone() else { continue };
                // Real shape sent by services.dart's confirmGoal:
                // {"action":"goal","goal":{"player":<Player.toJson()>,"teamId",
                // "minute","goalType","homeGoal","awayGoal"}} — "player" is a
                // full nested Player object (name is player.name, not a flat
                // "playerName"), and "homeGoal"/"awayGoal" are the app's own
                // authoritative running score (not "gA"/"gB", and not a delta).
                // Corrections arrive as a *separate* top-level action,
                // "undoGoal" (see that arm below) — "goal" is only ever a
                // fresh goal here, never a goalType:"undo" variant of this
                // same action.
                let goal_data = cmd.get("goal").cloned().unwrap_or(Value::Null);
                let player_name = goal_data
                    .get("player")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown")
                    .to_string();
                let home_goal = goal_data.get("homeGoal").and_then(Value::as_i64).map(|v| v as i32);
                let away_goal = goal_data.get("awayGoal").and_then(Value::as_i64).map(|v| v as i32);
                // Score bump + "GOAL!" banner are what's actually visible in
                // the composited output, so they're the part that has to
                // wait for `overlay_event_delay_ms` — see
                // `schedule_overlay_event`'s doc comment. Everything below
                // this block (Laravel logging, the fan-out broadcast) isn't
                // on screen, so it fires immediately as before.
                if let Some(session) = REGISTRY.get(&key) {
                    let key2 = key.clone();
                    let event_ts = cmd.get("timestamp").and_then(Value::as_i64);
                    schedule_overlay_event(key.clone(), &session, event_ts, move || {
                        let Some(state) = OVERLAY_STATES.get(&key2) else { return };
                        let mut ms = state.match_state.write().unwrap();
                        let old_a = ms.team_a_score;
                        // Mirror the app's tally rather than incrementing our
                        // own — it's the only way this can't drift from what
                        // the operator's app itself shows (a dropped/retried
                        // command would desync a local counter).
                        if let Some(a) = home_goal { ms.team_a_score = a; }
                        if let Some(b) = away_goal { ms.team_b_score = b; }
                        let (team_name, new_score) = if ms.team_a_score != old_a {
                            (ms.team_a_name.clone(), ms.team_a_score)
                        } else {
                            (ms.team_b_name.clone(), ms.team_b_score)
                        };
                        drop(ms);
                        *state.goal.write().unwrap() = Some(crate::overlay::graphics::GoalState {
                            is_active: true,
                            animation_start_time: crate::overlay::graphics::current_time_millis(),
                            phase: 0,
                            team_name,
                            player_name,
                            new_score,
                        });
                    });
                }
                if let Some(id) = REGISTRY.get(&key).and_then(|s| s.laravel_id()) {
                    CLIENT.log_event(&id, "goal", cmd.clone());
                } else {
                    warn!(stream_key = key, "goal event dropped: no Laravel session id yet");
                }
                // Dedicated goal-on-the-match-record endpoint, distinct
                // from the generic `log_event` above — see api/mod.rs's
                // "streamKey is the match_id" note.
                if let Some(match_id) = match_id_from_key(&key) {
                    CLIENT.add_goal(match_id, cmd.clone());
                } else {
                    warn!(stream_key = key, "add_goal skipped: empty stream key");
                }
                broadcast(&key, "goal");
            }

            // A mis-tap correction (see undoLastGoal's doc comment in
            // services.dart) — a distinct top-level action from "goal", not
            // a goalType variant of it. Applies the corrected score (still
            // frame-critical: the scorebar reading a stale count is exactly
            // as visible as it reading a wrong new one) but never shows a
            // "GOAL!" banner, since nothing was just scored.
            "undoGoal" => {
                let Some(key) = stream_key.clone() else { continue };
                let goal_data = cmd.get("goal").cloned().unwrap_or(Value::Null);
                let home_goal = goal_data.get("homeGoal").and_then(Value::as_i64).map(|v| v as i32);
                let away_goal = goal_data.get("awayGoal").and_then(Value::as_i64).map(|v| v as i32);
                if let Some(session) = REGISTRY.get(&key) {
                    let key2 = key.clone();
                    let event_ts = cmd.get("timestamp").and_then(Value::as_i64);
                    schedule_overlay_event(key.clone(), &session, event_ts, move || {
                        let Some(state) = OVERLAY_STATES.get(&key2) else { return };
                        let mut ms = state.match_state.write().unwrap();
                        if let Some(a) = home_goal { ms.team_a_score = a; }
                        if let Some(b) = away_goal { ms.team_b_score = b; }
                    });
                }
                if let Some(id) = REGISTRY.get(&key).and_then(|s| s.laravel_id()) {
                    CLIENT.log_event(&id, "undoGoal", cmd.clone());
                } else {
                    warn!(stream_key = key, "undoGoal event dropped: no Laravel session id yet");
                }
                // The actual database-level undo: remove whichever Scorer
                // row was created most recently for this match. Laravel
                // resolves "which one" itself — see
                // `api::CLIENT::delete_latest_goal`'s doc comment for why
                // this server can't target a specific scorer_id directly.
                if let Some(match_id) = match_id_from_key(&key) {
                    CLIENT.delete_latest_goal(match_id);
                } else {
                    warn!(stream_key = key, "delete_latest_goal skipped: empty stream key");
                }
                broadcast(&key, "undoGoal");
            }

            // NOTE: "replay" is intentionally NOT in this arm — it has its
            // own dedicated handler above (ties into `crate::ads::start_replay`).
            // A duplicate "replay" here would be unreachable dead code since
            // Rust match arms are checked in order and the first one above
            // already claims it; caught while writing WORKFLOW.md §7.5.
            "card" => {
                let Some(key) = stream_key.clone() else { continue };
                // Real shape from the lineup card picker (streaming.dart's
                // onCard): {"action":"card","data":{"player":<Player.toJson()>,
                // "card":"Yellow"|"Red"|"None"}} — a full Player object
                // nested under "data", not flat "team"/"player"/"cardType"
                // fields, and no "reason" field exists on the wire at all.
                let data = cmd.get("data").cloned().unwrap_or(Value::Null);
                let player = data.get("player").cloned().unwrap_or(Value::Null);
                let card_str = data.get("card").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                let card_type = match card_str.as_str() {
                    "red" => crate::overlay::graphics::CardType::RedCard,
                    "yellow" => crate::overlay::graphics::CardType::YellowCard,
                    // "None" is Player.card being reset/cleared, not a card
                    // being shown — nothing to announce on screen.
                    _ => {
                        warn!(stream_key = key, card = card_str, "card event skipped: not a red/yellow card");
                        continue;
                    }
                };
                let team_name = player.get("team").and_then(Value::as_str).unwrap_or("").to_string();
                let player_name = player.get("name").and_then(Value::as_str).unwrap_or("Unknown").to_string();
                if let Some(session) = REGISTRY.get(&key) {
                    let key2 = key.clone();
                    let event_ts = cmd.get("timestamp").and_then(Value::as_i64);
                    // Same reasoning as "goal" above: the card banner is
                    // on-screen, so it waits for overlay_event_delay_ms,
                    // anchored to the command's own timestamp.
                    schedule_overlay_event(key.clone(), &session, event_ts, move || {
                        let Some(state) = OVERLAY_STATES.get(&key2) else { return };
                        *state.card.write().unwrap() = Some(crate::overlay::graphics::CardState {
                            is_active: true,
                            animation_start_time: crate::overlay::graphics::current_time_millis(),
                            phase: 0,
                            team_name,
                            player_name,
                            card_type,
                            reason: None,
                        });
                    });
                }
                if let Some(id) = REGISTRY.get(&key).and_then(|s| s.laravel_id()) {
                    CLIENT.log_event(&id, "card", cmd.clone());
                } else {
                    warn!(stream_key = key, "card event dropped: no Laravel session id yet");
                }
                broadcast(&key, "card");
            }

            "substitute" => {
                let Some(key) = stream_key.clone() else { continue };
                // Real shape from streaming.dart's onPlayerSubstitution:
                // {"action":"substitute","subs":{"playerIn":<Player.toJson()>,
                // "playerOut":<Player.toJson()>}} — two full Player objects
                // under "subs", not flat "team"/"playerIn"/"playerOut" strings.
                if let Some(state) = OVERLAY_STATES.get(&key) {
                    let subs = cmd.get("subs").cloned().unwrap_or(Value::Null);
                    let player_in = subs.get("playerIn").cloned().unwrap_or(Value::Null);
                    let player_out = subs.get("playerOut").cloned().unwrap_or(Value::Null);
                    let team_name = player_out
                        .get("team")
                        .and_then(Value::as_str)
                        .or_else(|| player_in.get("team").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    let player_out_name = player_out.get("name").and_then(Value::as_str).unwrap_or("Unknown").to_string();
                    let player_in_name = player_in.get("name").and_then(Value::as_str).unwrap_or("Unknown").to_string();
                    *state.substitution.write().unwrap() = Some(crate::overlay::graphics::SubstitutionState {
                        is_active: true,
                        animation_start_time: crate::overlay::graphics::current_time_millis(),
                        phase: 0,
                        team_name,
                        player_out: player_out_name,
                        player_in: player_in_name,
                    });
                }
                if let Some(id) = REGISTRY.get(&key).and_then(|s| s.laravel_id()) {
                    CLIENT.log_event(&id, "substitute", cmd.clone());
                } else {
                    warn!(stream_key = key, "substitute event dropped: no Laravel session id yet");
                }
                broadcast(&key, "substitute");
            }

            // `lineup` — pre-kickoff team-sheet sequence. Parses both
            // teams' rosters/benches from the payload and hands them to
            // `OverlayState::lineup`, which `render_frame` then drives
            // through the four-stage sequence (see `LineupState`'s doc
            // comment in overlay/mod.rs).
            "lineup" => {
                if let Some(key) = stream_key.clone() {
                    if let Some(state) = OVERLAY_STATES.get(&key) {
                        let from_command = match (cmd.get("teamA"), cmd.get("teamB")) {
                            (Some(team_a_val), Some(team_b_val)) => {
                                let (team_a, team_a_subs_table) = parse_team_lineup(team_a_val);
                                let (team_b, team_b_subs_table) = parse_team_lineup(team_b_val);
                                Some(LineupState {
                                    started_at_ms: crate::overlay::graphics::current_time_millis(),
                                    team_a,
                                    team_b,
                                    team_a_subs_table,
                                    team_b_subs_table,
                                })
                            }
                            _ => None,
                        };
                        // No payload on this command — fall back to whatever
                        // seed_overlay_from_match_data built at `start` from
                        // register's matchData (or a Laravel fetch), if
                        // anything. Re-stamp started_at_ms to now rather than
                        // reusing whatever time it was seeded at, since that's
                        // when it's actually being shown.
                        let to_show = from_command.or_else(|| {
                            state.pending_lineup.read().unwrap().clone().map(|mut l| {
                                l.started_at_ms = crate::overlay::graphics::current_time_millis();
                                l
                            })
                        });
                        match to_show {
                            Some(l) => *state.lineup.write().unwrap() = Some(l),
                            None => warn!(
                                stream_key = key,
                                "lineup command missing teamA/teamB payload and no match roster cached from register"
                            ),
                        }
                    }
                    if let Some(id) = REGISTRY.get(&key).and_then(|s| s.laravel_id()) {
                        CLIENT.log_event(&id, action, cmd.clone());
                    } else {
                        warn!(stream_key = key, action, "event dropped: no Laravel session id yet");
                    }
                    broadcast(&key, action);
                }
            }

            // Live on/off switch for the whole overlay compositor pad —
            // animates `overlay_pad`'s alpha the same way the camera
            // fallback crossfade does (see pipeline::transitions). Payload:
            // {"action":"overlay","value":"on"|"off"}.
            "overlay" => {
                let Some(key) = stream_key.clone() else { continue };
                if let Some(session) = REGISTRY.get(&key) {
                    let show = cmd.get("value").and_then(Value::as_str) != Some("off");
                    pipeline::transitions::animate_f64(
                        &session.overlay_pad,
                        "alpha",
                        if show { 1.0 } else { 0.0 },
                        crate::config::TRANSITION_MS,
                    );
                    broadcast(&key, if show { "overlay:on" } else { "overlay:off" });
                }
            }

            // Tears down one camera's compositor branch and forgets its
            // IP -> stream-key mapping. Complements `register`'s
            // `add_camera` — see pipeline::cameras::remove_camera's doc
            // comment, which anticipated exactly this action.
            "remove_camera" => {
                let Some(key) = stream_key.clone() else { continue };
                let target_ip = cmd.get("ip").and_then(Value::as_str).unwrap_or(&peer_ip).to_string();
                if let Some(session) = REGISTRY.get(&key) {
                    match pipeline::cameras::remove_camera(&session, &target_ip) {
                        Ok(()) => {
                            REGISTRY.unregister_ip(&target_ip);
                            session.clients.remove(&target_ip);
                            broadcast(&key, "list updated");
                            info!(stream_key = key, ip = target_ip, "camera removed");
                        }
                        Err(e) => warn!(stream_key = key, ip = target_ip, "remove_camera failed: {e}"),
                    }
                }
            }

            // Diagnostic/UI-population action: reports each active output
            // branch's id and kind back to the requesting client only
            // (not broadcast — this is per-connection state, not a
            // session-wide event).
            "list_outputs" => {
                let Some(key) = stream_key.clone() else { continue };
                if let Some(session) = REGISTRY.get(&key) {
                    let outputs: Vec<Value> = session
                        .output_branches
                        .iter()
                        .map(|entry| json!({ "id": entry.value().id, "kind": format!("{:?}", entry.value().kind) }))
                        .collect();
                    let _ = tx.send(json!({ "message": "outputs", "outputs": outputs }).to_string());
                }
            }

            // Calibrates `StreamSession::overlay_event_delay_ms` — see its
            // doc comment and `config::DEFAULT_OVERLAY_EVENT_DELAY_MS` for
            // why goal/card events are held back before drawing. Typically
            // set once per session, after measuring actual glass-to-glass
            // latency (e.g. a clapperboard/stopwatch test against the
            // composited output), not sent per event.
            "set_overlay_delay" => {
                let Some(key) = stream_key.clone() else { continue };
                let Some(ms) = cmd.get("ms").and_then(Value::as_u64) else {
                    warn!(stream_key = key, "set_overlay_delay: missing/invalid 'ms'");
                    continue;
                };
                if let Some(session) = REGISTRY.get(&key) {
                    session.set_overlay_event_delay_ms(ms);
                    let _ = tx.send(json!({ "message": "overlay_delay_set", "ms": ms }).to_string());
                    info!(stream_key = key, ms, "overlay event delay calibrated");
                }
            }

            other => warn!(peer_ip, "unknown control action '{other}'"),
        }
    }

    Ok(())
}

/// Builds one team's `TeamLineupData` (starting XI) and paired
/// `SubstitutionTable` (bench). `is_active` on the returned table starts
/// `false` — `render_frame` flips it (and stamps `animation_start_time`)
/// itself, the first frame that team's subs-table stage is actually
/// reached; see `LineupState`'s doc comment for why.
///
/// Accepts two payload dialects, since this is called from two places with
/// two different sources: the operator-triggered `lineup` command's own
/// ad-hoc `{jerseyColor, players, subs}` payload, and register-time
/// `matchData` / a `GET /matches/{id}` fetch, which both use
/// `{color, startingPlayers, substitutes}` — that's what `Team.toJson()` in
/// `models.dart` and `MatchFormatterService::format()` in Laravel each
/// produce, and they happen to agree with each other exactly. The
/// `substitutes` dialect has no `goalkeeper`/`isGoalkeeper` field at all, so
/// `is_goalkeeper` is always `false` for a lineup seeded that way — a known,
/// minor gap, not a bug: a keeper coming on as a sub just won't get the "GK"
/// treatment via this path.
fn parse_team_lineup(team: &Value) -> (TeamLineupData, crate::overlay::graphics::SubstitutionTable) {
    let team_name = team.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let formation = team.get("formation").and_then(Value::as_str).unwrap_or("4-4-2").to_string();
    let jersey_color = team
        .get("jerseyColor")
        .or_else(|| team.get("color"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;

    let starters_key = if team.get("players").is_some() { "players" } else { "startingPlayers" };
    let subs_key = if team.get("subs").is_some() { "subs" } else { "substitutes" };

    let players: Vec<std::collections::HashMap<String, String>> = team
        .get(starters_key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|p| {
                    let mut map = std::collections::HashMap::new();
                    map.insert("team".to_string(), team_name.clone());
                    map.insert("name".to_string(), p.get("name").and_then(Value::as_str).unwrap_or("").to_string());
                    map.insert("number".to_string(), p.get("number").map(value_to_display_string).unwrap_or_default());
                    map.insert("position".to_string(), p.get("position").and_then(Value::as_str).unwrap_or("").to_string());
                    map
                })
                .collect::<Vec<std::collections::HashMap<String, String>>>()
        })
        .unwrap_or_default();

    let substitutes: Vec<crate::overlay::graphics::SubstitutePlayer> = team
        .get(subs_key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|p| crate::overlay::graphics::SubstitutePlayer {
                    name: p.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    jersey_number: p.get("number").and_then(Value::as_i64).unwrap_or(0) as i32,
                    position: p.get("position").and_then(Value::as_str).unwrap_or("").to_string(),
                    is_goalkeeper: p.get("goalkeeper").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect::<Vec<crate::overlay::graphics::SubstitutePlayer>>()
        })
        .unwrap_or_default();

    let subs_table = crate::overlay::graphics::SubstitutionTable {
        is_active: false,
        animation_start_time: 0,
        phase: 0,
        team_name: team_name.clone(),
        team_color: jersey_color,
        substitutes,
    };

    (TeamLineupData { players, formation, jersey_color }, subs_table)
}

/// `matchData`/`otherMatchData` (from `register`) or a `GET /matches/{id}`
/// response is only good enough to seed the overlay if both teams actually
/// have a name — an empty/absent `matchData` still parses fine (every field
/// above defaults), it just produces two blank team names, which is worse
/// than the "HOME"/"AWAY" placeholders `OverlayState::default()` already
/// shows.
fn has_usable_team_names(match_data: &Value) -> bool {
    let name_of = |side: &str| {
        match_data
            .get(side)
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    };
    name_of("homeTeam") && name_of("awayTeam")
}

/// Seeds a freshly-created `OverlayState` from a match record — team names
/// and current score into `match_state` immediately, plus a `pending_lineup`
/// (built with `parse_team_lineup`, reusing the exact same parser the
/// `lineup` control action uses) if both teams' starting XI are present, so
/// a later bare `{"action":"lineup"}` has something real to show. Called
/// once, right after `start` builds the session — see that handler.
fn seed_overlay_from_match_data(state: &OverlayState, match_data: &Value) {
    let (Some(home), Some(away)) = (match_data.get("homeTeam"), match_data.get("awayTeam")) else {
        return;
    };

    {
        let mut ms = state.match_state.write().unwrap();
        if let Some(n) = home.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            ms.team_a_name = n.to_string();
        }
        if let Some(n) = away.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            ms.team_b_name = n.to_string();
        }
        // "goals" here is the match's score as of kickoff (normally 0) —
        // not a substitute for the scorers-table-derived score Laravel
        // tracks. Just avoids starting a resumed/reconnected session's
        // scorebug back at 0-0 if it already had goals.
        if let Some(g) = home.get("goals").and_then(Value::as_i64) {
            ms.team_a_score = g as i32;
        }
        if let Some(g) = away.get("goals").and_then(Value::as_i64) {
            ms.team_b_score = g as i32;
        }
    }

    let has_starters = |t: &Value| {
        t.get("startingPlayers")
            .or_else(|| t.get("players"))
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    };
    if has_starters(home) && has_starters(away) {
        let (team_a, team_a_subs_table) = parse_team_lineup(home);
        let (team_b, team_b_subs_table) = parse_team_lineup(away);
        *state.pending_lineup.write().unwrap() = Some(LineupState {
            started_at_ms: 0, // stamped for real only when actually shown — see the "lineup" handler.
            team_a,
            team_b,
            team_a_subs_table,
            team_b_subs_table,
        });
    }
}

/// `players[].number` is commonly sent as either a JSON string or a
/// number depending on the mobile client; normalize either to a display
/// string rather than dropping non-string values.
fn value_to_display_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Runs `apply` once the event's real-world moment has actually reached
/// the compositor — not merely `overlay_event_delay_ms` after the command
/// arrived at this server.
///
/// Commands carry a `timestamp` field (client epoch-ms, wall clock —
/// `System.currentTimeMillis()` on the existing Kotlin bridge side; see
/// `MainActivity.sendCommandEvent` for the established precedent) stamped
/// when the operator confirmed the event, not when it reached this
/// server. That distinction matters: the control command and the live
/// video frame take independent network paths with independent, often
/// very different latency (see the "goal"/"card" handlers' comments).
/// Sleeping a fixed amount *from arrival* would bake in whatever transit
/// jitter the command itself happened to pick up. Scheduling from the
/// event's own timestamp instead removes that variable — the target
/// moment is always `event_timestamp_ms + overlay_event_delay_ms`,
/// regardless of how long the command took to get here.
///
/// `event_timestamp_ms: None` (an older client, or a missing/unparseable
/// field) falls back to the pre-timestamp behavior: delay counted from
/// this function's own call time.
///
/// Clock-skew guard: an unsynchronized client clock could otherwise make
/// `event_timestamp_ms` arbitrarily wrong — a client clock running ahead
/// of the server's, in particular, would compute a target far in the
/// future and hold the overlay indefinitely. The wait is clamped to
/// `config::MAX_OVERLAY_EVENT_HOLD_MS`; hitting that cap logs a warning
/// (probable clock skew) rather than blocking that long.
///
/// Re-checks the session is still the one registered under `key` before
/// applying, in case it stopped (or was replaced) during the wait — the
/// same guard `overlay::spawn_overlay_renderer`'s loop uses.
fn schedule_overlay_event(
    key: String,
    session: &Arc<crate::model::StreamSession>,
    event_timestamp_ms: Option<i64>,
    apply: impl FnOnce() + Send + 'static,
) {
    let delay_ms = session.overlay_event_delay_ms() as i64;
    let now = crate::model::now_ms();
    let target = event_timestamp_ms.unwrap_or(now) + delay_ms;
    let mut wait_ms = (target - now).max(0) as u64;
    if wait_ms > crate::config::MAX_OVERLAY_EVENT_HOLD_MS {
        warn!(
            stream_key = key,
            wait_ms,
            cap_ms = crate::config::MAX_OVERLAY_EVENT_HOLD_MS,
            "overlay event hold exceeds MAX_OVERLAY_EVENT_HOLD_MS — capping (check for client/server clock skew)"
        );
        wait_ms = crate::config::MAX_OVERLAY_EVENT_HOLD_MS;
    }

    tokio::spawn(async move {
        if wait_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
        if REGISTRY.get(&key).is_none() {
            return;
        }
        apply();
    });
}

/// Per the "streamKey is the match_id" note at the top of api/mod.rs: the
/// Laravel `matches` table id is the stream key itself — a UUID string
/// (`matches` extends `BaseUuidModel`), not a number. Returns `None` only
/// for an empty key, so callers can skip the match-record-specific API
/// calls without sending a bogus id; any non-empty key is passed through
/// as-is, since it's the UUID by construction.
fn match_id_from_key(key: &str) -> Option<&str> {
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// Maps `SessionState` to the small integer status code the Laravel
/// `matches.status` column uses. Kept local to the control socket (rather
/// than on `SessionState` itself) since it's purely an API-wire concern.
fn session_state_status_code(state: crate::model::SessionState) -> i32 {
    use crate::model::SessionState::*;
    match state {
        Start => 0,
        Live => 1,
        HalfTime => 2,
        ExtraTime => 3,
        FullTime => 4,
        Stopped => 5,
    }
}

fn broadcast(stream_key: &str, message: &str) {
    if let Some(mut senders) = FANOUT.get_mut(stream_key) {
        let payload = json!({ "message": message }).to_string();
        senders.retain(|tx| tx.send(payload.clone()).is_ok());
    }
}
