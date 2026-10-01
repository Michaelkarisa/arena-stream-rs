//! Overlay compositing input.
//!
//! Renders the scorebar/badge/banners/goal-card-sub popups into a BGRA
//! buffer and pushes it into the session's overlay `appsrc`, which the
//! pipeline composites directly onto the video. The overlay surface is
//! built at the session's actual physical canvas size (see
//! `config::canvas_dims_for_quality`) — no mismatched-size scale between
//! video and overlay at the compositor. All the widget layout math in this
//! file and in `graphics.rs` is still written against a fixed *logical*
//! 1280×720 space regardless of that physical size; `render_frame` bridges
//! the two with a single `cairo::Context::scale` call.
//!
//! **Switched from tiny-skia to Cairo+Pango** (`overlay::graphics`) — real
//! glyph rendering via Pango instead of placeholder rects, closing the gap
//! flagged since the very first pipeline pass (WORKFLOW.md §7.6).
//!
//! **Format note, important:** Cairo's `Format::ARgb32` is premultiplied
//! alpha, native-endian — on little-endian machines that's byte order
//! B,G,R,A, i.e. the same byte order as GStreamer's `BGRA` raw video
//! format. So the pipeline's overlay `appsrc` caps were changed from
//! `RGBA` to `BGRA` to match (see `pipeline::build_session_pipeline`) — no
//! channel swizzling needed. What *does* need handling is premultiplied →
//! straight alpha: GStreamer's compositor blends `BGRA` assuming straight
//! alpha, so `render_frame` un-premultiplies each pixel while copying out
//! of the Cairo surface. It's a per-pixel divide, not free, but simple and
//! correct; worth profiling before optimizing further.
//!
//! **Lineup sequence wired in.** `LineupState` drives the "team intro
//! cards → formation → subs table → crossover → repeat for the away team"
//! sequence described above, timed off `started_at_ms`; see its doc
//! comment for the per-stage breakdown. Populated by the `lineup` control
//! action (`control::mod`). Team logo lookups still go through the same
//! empty `image_cache` as everything else here — see the note below.

use crate::config::{LOGICAL_CANVAS_H, LOGICAL_CANVAS_W, OVERLAY_FPS};
use crate::model::StreamSession;
use cairo::{Context, Format, ImageSurface};
use gstreamer as gst;
use gstreamer_app::AppSrc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tracing::warn;

pub mod graphics;

use graphics::{
    CardState, Color, Font, FontStyle, Graphics2D, GoalState, MatchState, SubstitutionState,
    SubstitutionTable,
};

/// Mutable per-session overlay state. Cheap to update from the
/// control-socket thread; read (and, for the transient widgets, mutated —
/// their `is_active`/`phase` fields self-expire) once per frame by the
/// render loop.
pub struct OverlayState {
    pub match_state: RwLock<MatchState>,
    pub live: AtomicBool,
    pub goal: RwLock<Option<GoalState>>,
    pub card: RwLock<Option<CardState>>,
    pub substitution: RwLock<Option<SubstitutionState>>,
    /// Set by the ads-management loop while an image ad is occupying the
    /// left-hand panel opened up by shrinking the live video.
    pub image_ad: RwLock<Option<ImageAd>>,
    /// Set while a video ad is playing in the main video pipe — draws the
    /// "this is an ad" banner over it.
    pub video_ad_banner: AtomicBool,
    /// Set while a replay is in progress — draws the "REPLAY" banner.
    pub replay_banner: AtomicBool,
    /// Drives the pre-kickoff/team-sheet sequence — see [`LineupState`].
    pub lineup: RwLock<Option<LineupState>>,
    /// A lineup built from `register`'s `matchData`/`otherMatchData` (or,
    /// if those were missing/too thin, a `GET /matches/{id}` fetch — see
    /// `control::seed_overlay_from_match_data`), ready to show the moment a
    /// bare `{"action":"lineup"}` arrives with no `teamA`/`teamB` payload of
    /// its own. `None` if neither source had real starting-XI data.
    pub pending_lineup: RwLock<Option<LineupState>>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            match_state: RwLock::new(MatchState {
                is_active: false,
                animation_start_time: 0,
                phase: 0,
                team_a_name: "HOME".to_string(),
                team_b_name: "AWAY".to_string(),
                team_a_score: 0,
                team_b_score: 0,
                current_time: "00:00".to_string(),
            }),
            live: AtomicBool::new(false),
            goal: RwLock::new(None),
            card: RwLock::new(None),
            substitution: RwLock::new(None),
            image_ad: RwLock::new(None),
            video_ad_banner: AtomicBool::new(false),
            replay_banner: AtomicBool::new(false),
            lineup: RwLock::new(None),
            pending_lineup: RwLock::new(None),
        }
    }
}

#[derive(Clone)]
pub struct ImageAd {
    pub path: String,
    pub label: String,
    pub panel_width: i32,
}

/// One team's starting XI for the lineup sequence, as the
/// `HashMap<String, String>` shape `overlay::graphics`'s player/formation
/// functions already expect (`name`/`number`/`position`/`team` keys), plus
/// its formation string and jersey color. The bench lives directly on the
/// paired `SubstitutionTable` in `LineupState` rather than duplicated here.
#[derive(Clone)]
pub struct TeamLineupData {
    pub players: Vec<HashMap<String, String>>,
    pub formation: String,
    pub jersey_color: u32,
}

/// Drives the "team intro cards → formation → subs table → crossover →
/// repeat for the away team" sequence described in this module's docs.
/// Built once from a `lineup` control command and timed off
/// `started_at_ms`; `render_frame` reads the elapsed time each frame to
/// decide which of the four stages (each ~9-10s) is showing, and clears
/// itself once the sequence completes.
///
/// The two `*_subs_table` fields are pre-built (`is_active: false`) and
/// only actually started — `is_active` flipped and `animation_start_time`
/// stamped — the first frame their stage is reached; `current_time_millis()
/// - animation_start_time` would otherwise underflow for a table that
/// hasn't started yet (its stamp would be in the future for tables staged
/// ahead of time).
#[derive(Clone)]
pub struct LineupState {
    pub started_at_ms: u128,
    pub team_a: TeamLineupData,
    pub team_b: TeamLineupData,
    pub team_a_subs_table: SubstitutionTable,
    pub team_b_subs_table: SubstitutionTable,
}

/// Duration of each formation-display stage, ms.
const LINEUP_FORMATION_MS: u128 = 10_000;
/// Duration of each subs-table stage, ms — matches
/// `graphics::draw_substitutes_table_widget`'s own internal 9s auto-expiry,
/// so the two stay in lockstep.
const LINEUP_SUBS_MS: u128 = 9_000;

/// Broadcast-safe palette — matches the Java `OverlayPalette`.
mod palette {
    use super::Color;
    pub const PANEL_BG: Color = Color { r: 15.0 / 255.0, g: 15.0 / 255.0, b: 15.0 / 255.0, a: 1.0 };
    pub const PANEL_BORDER: Color = Color { r: 60.0 / 255.0, g: 60.0 / 255.0, b: 60.0 / 255.0, a: 1.0 };
    pub const TEAM_NAME_FG: Color = Color { r: 210.0 / 255.0, g: 210.0 / 255.0, b: 210.0 / 255.0, a: 1.0 };
    pub const CLOCK_FG: Color = Color { r: 1.0, g: 220.0 / 255.0, b: 60.0 / 255.0, a: 1.0 };
    pub const LIVE_DOT: Color = Color { r: 220.0 / 255.0, g: 30.0 / 255.0, b: 30.0 / 255.0, a: 1.0 };
    pub const CHANNEL_BG: Color = Color { r: 10.0 / 255.0, g: 10.0 / 255.0, b: 40.0 / 255.0, a: 1.0 };
    pub const CHANNEL_TEXT: Color = Color { r: 200.0 / 255.0, g: 200.0 / 255.0, b: 1.0, a: 1.0 };
    pub const AD_BG: Color = Color { r: 30.0 / 255.0, g: 20.0 / 255.0, b: 40.0 / 255.0, a: 1.0 };
    pub const AD_FG: Color = Color { r: 150.0 / 255.0, g: 100.0 / 255.0, b: 180.0 / 255.0, a: 1.0 };
    pub const GOAL_BANNER_BG: Color = Color { r: 18.0 / 255.0, g: 72.0 / 255.0, b: 18.0 / 255.0, a: 1.0 };
    pub const GOAL_BORDER: Color = Color { r: 40.0 / 255.0, g: 160.0 / 255.0, b: 40.0 / 255.0, a: 1.0 };
    pub const SUB_BG: Color = Color { r: 20.0 / 255.0, g: 20.0 / 255.0, b: 40.0 / 255.0, a: 1.0 };
    pub const SUB_BORDER: Color = Color { r: 60.0 / 255.0, g: 60.0 / 255.0, b: 120.0 / 255.0, a: 1.0 };
}

pub fn spawn_overlay_renderer(session: Arc<StreamSession>, state: Arc<OverlayState>) {
    let appsrc: AppSrc = session.overlay_appsrc.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(1000 / OVERLAY_FPS as u64));
        loop {
            interval.tick().await;

            if !crate::model::REGISTRY
                .get(&session.stream_key)
                .map(|s| Arc::ptr_eq(&s, &session))
                .unwrap_or(false)
            {
                break;
            }

            match render_frame(&session, &state) {
                Ok(buf) => {
                    if let Err(e) = appsrc.push_buffer(buf) {
                        warn!(stream_key = %session.stream_key, "overlay push_buffer failed: {e}");
                    }
                }
                Err(e) => warn!("overlay render error: {e}"),
            }
        }
    });
}

/// Small custom draw, not part of the pasted widget set (no
/// `ChannelBadgeWidget` equivalent was provided alongside the Cairo
/// primitives) — built from the same `Graphics2D` primitives everything
/// else uses, geometry matching the earlier tiny-skia placeholder pass
/// (`CB_X=20`, bottom margin 20, `H=32`).
fn draw_channel_badge(g: &mut Graphics2D, live: bool) {
    const CB_X: f64 = 20.0;
    const CB_H: f64 = 32.0;
    const CB_W: f64 = 150.0;
    let cb_y = LOGICAL_CANVAS_H as f64 - 20.0 - CB_H;

    g.set_color(palette::CHANNEL_BG);
    g.fill_round_rect(CB_X, cb_y, CB_W, CB_H, 16.0);

    if live {
        g.set_color(palette::LIVE_DOT);
        g.fill_oval(CB_X + 12.0, cb_y + CB_H / 2.0 - 4.5, 9.0, 9.0);
    }

    g.set_font(Font::new("Arial", FontStyle::Bold, 13));
    g.set_color(palette::CHANNEL_TEXT);
    g.draw_string("LIVE", CB_X + 30.0, cb_y + CB_H / 2.0 + 5.0);
}

/// Same custom-draw situation as the channel badge — `EventBannerWidget`'s
/// slide/geometry wasn't part of the pasted Cairo file, so this is a
/// direct port of the earlier tiny-skia version onto `Graphics2D`, now
/// with real text instead of a blank colored strip.
fn draw_center_banner(g: &mut Graphics2D, text: &str, bg: Color, border: Color) {
    let bw = 340.0;
    let bx = (LOGICAL_CANVAS_W as f64 - bw) / 2.0;
    let by = 20.0 + 44.0 + 8.0; // just under the scorebar, matching EventBannerWidget's real BY

    g.set_color(bg);
    g.fill_round_rect(bx, by, bw, 50.0, 6.0);
    g.set_color(border);
    g.set_stroke(graphics::BasicStroke::new(2.0));
    g.draw_round_rect(bx, by, bw, 50.0, 6.0);

    g.set_font(Font::new("Arial", FontStyle::Bold, 20));
    g.set_color(Color::WHITE);
    let metrics = g.get_font_metrics();
    let w = metrics.string_width(text) as f64;
    g.draw_string(text, bx + (bw - w) / 2.0, by + 30.0);
}

fn render_frame(session: &StreamSession, state: &OverlayState) -> anyhow::Result<gst::Buffer> {
    // Surface is the session's actual physical canvas size (set at `start`
    // from the registered quality — config::canvas_dims_for_quality), not a
    // fixed constant. Every widget below still computes pixel positions
    // against the fixed LOGICAL_CANVAS_W/H space, unchanged from before this
    // was dynamic — the `cr.scale(...)` call right after context creation
    // is the one and only bridge between the two, so none of that widget
    // math (in this file or graphics.rs) needed to change.
    let mut surface = ImageSurface::create(Format::ARgb32, session.canvas_w, session.canvas_h)
        .map_err(|e| anyhow::anyhow!("failed to create Cairo surface: {e}"))?;

    let empty_image_cache: HashMap<String, ImageSurface> = HashMap::new();

    {
        let cr = Context::new(&surface).map_err(|e| anyhow::anyhow!("failed to create Cairo context: {e}"))?;
        // Vector-quality scale (transforms the coordinate system Cairo/Pango
        // draw and measure text in, before rasterizing) rather than a raster
        // resize after the fact — text and shapes render crisply at the
        // physical size directly instead of being blurred by post-hoc
        // resampling. Pango's own text-metric queries (get_font_metrics
        // below) report logical-space sizes regardless of this transform,
        // so the positioning math that depends on measured text width stays
        // correct too.
        cr.scale(
            session.canvas_w as f64 / LOGICAL_CANVAS_W as f64,
            session.canvas_h as f64 / LOGICAL_CANVAS_H as f64,
        );
        let mut g = Graphics2D::new(&cr);

        // ── Scorebar. X=20,Y=20,H=44,W=520 — same geometry as the earlier
        // tiny-skia pass and the HTML preview, now driving real text via
        // draw_main_widget instead of placeholder bars. ──
        {
            let mut ms = state.match_state.write().unwrap();
            // Live match clock derived from wall-clock elapsed since
            // session start — a simplification, not a real match clock
            // (no halftime pause, no injury-time handling). Flagging
            // rather than pretending this is broadcast-accurate timing.
            let elapsed_s = ((crate::model::session::now_ms() - session.created_at_ms) / 1000).max(0) as u32;
            ms.current_time = format!("{:02}:{:02}", elapsed_s / 60, elapsed_s % 60);
            let period = graphics::calculate_period((elapsed_s / 60) as i64);

            graphics::draw_main_widget(
                &mut g,
                &ms.team_a_name,
                &ms.team_b_name,
                ms.team_a_score,
                ms.team_b_score,
                &ms.current_time,
                20.0,
                20.0,
                520.0,
                48.0,
                palette::PANEL_BG,
                palette::PANEL_BORDER,
                palette::TEAM_NAME_FG,
                palette::TEAM_NAME_FG,
                Color::WHITE,
                palette::CLOCK_FG,
                period,
                &empty_image_cache,
            );

            // Full-screen "match center" popup — plays once whenever
            // control/mod.rs's "state" handler flips `ms.is_active` (on
            // kickoff/restart), then self-expires after ~9s via its own
            // phase machine. Self-contained no-op when inactive.
            graphics::draw_match_performance_widget(
                &mut *ms,
                &mut g,
                LOGICAL_CANVAS_W as f64,
                LOGICAL_CANVAS_H as f64,
                480.0,
                300.0,
                palette::PANEL_BG,
                palette::PANEL_BORDER,
                period,
                &empty_image_cache,
            );
        }

        draw_channel_badge(&mut g, state.live.load(Ordering::Relaxed));

        // Platform/channel logo slot — currently a documented no-op
        // pending an asset-loading system (see graphics.rs's doc comment
        // on `draw_platform_logo`), but the call site belongs here so it
        // "just works" once that lands.
        graphics::draw_platform_logo(&mut g, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64);

        // ── Goal / card / substitution — mutually independent widgets
        // (unlike the ad/event-banner slot sharing in the earlier Java
        // widget set), each self-expiring via its own is_active/phase.
        {
            let mut goal = state.goal.write().unwrap();
            if let Some(gs) = goal.as_mut() {
                graphics::draw_goal_widget(
                    gs, &mut g, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64, 360.0, 160.0,
                    palette::GOAL_BANNER_BG, palette::GOAL_BORDER, &empty_image_cache,
                );
                if !gs.is_active {
                    *goal = None;
                }
            }
        }
        {
            let mut card = state.card.write().unwrap();
            graphics::draw_card_widget(
                &mut card, &mut g, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64, 260.0, 60.0,
                palette::PANEL_BG, palette::PANEL_BORDER, &empty_image_cache,
            );
        }
        {
            let mut sub = state.substitution.write().unwrap();
            graphics::draw_substitution_widget(
                &mut sub, &mut g, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64, 260.0, 60.0,
                palette::SUB_BG, palette::SUB_BORDER, &empty_image_cache,
            );
        }

        // ── Pre-kickoff lineup sequence — team A formation → team A subs
        // → team B formation → team B subs → done. Populated by the
        // `lineup` control action; see `LineupState` doc comment for the
        // staging. ──
        {
            let mut lineup_slot = state.lineup.write().unwrap();
            let mut finished = false;
            if let Some(lineup) = lineup_slot.as_mut() {
                let elapsed = graphics::current_time_millis().saturating_sub(lineup.started_at_ms);
                if elapsed < LINEUP_FORMATION_MS {
                    graphics::draw_team_lineup(
                        &mut g, &lineup.team_a.players, &lineup.team_a.formation,
                        lineup.team_a.jersey_color, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64, true,
                    );
                } else if elapsed < LINEUP_FORMATION_MS + LINEUP_SUBS_MS {
                    if !lineup.team_a_subs_table.is_active {
                        lineup.team_a_subs_table.is_active = true;
                        lineup.team_a_subs_table.animation_start_time = graphics::current_time_millis();
                    }
                    graphics::draw_substitutes_table_widget(
                        &mut lineup.team_a_subs_table, &mut g, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64,
                        &empty_image_cache,
                    );
                } else if elapsed < 2 * LINEUP_FORMATION_MS + LINEUP_SUBS_MS {
                    graphics::draw_team_lineup(
                        &mut g, &lineup.team_b.players, &lineup.team_b.formation,
                        lineup.team_b.jersey_color, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64, false,
                    );
                } else if elapsed < 2 * LINEUP_FORMATION_MS + 2 * LINEUP_SUBS_MS {
                    if !lineup.team_b_subs_table.is_active {
                        lineup.team_b_subs_table.is_active = true;
                        lineup.team_b_subs_table.animation_start_time = graphics::current_time_millis();
                    }
                    graphics::draw_substitutes_table_widget(
                        &mut lineup.team_b_subs_table, &mut g, LOGICAL_CANVAS_W as f64, LOGICAL_CANVAS_H as f64,
                        &empty_image_cache,
                    );
                } else {
                    finished = true;
                }
            }
            if finished {
                *lineup_slot = None;
            }
        }

        // ── Ad / replay overlays, drawn last so they stay on top ──
        if let Some(ad) = state.image_ad.read().unwrap().clone() {
            let w = ad.panel_width as f64; // logical-canvas units — see ads/mod.rs's inject_image_ad
            g.set_color(palette::AD_BG);
            g.fill_rect(0.0, 0.0, w, LOGICAL_CANVAS_H as f64);
            // Real ad creative, keyed by path, when one's been loaded into
            // the cache; falls back to the plain label placeholder (which
            // is all that's ever available today, since `empty_image_cache`
            // is always empty — see the module doc on asset loading).
            if let Some(img) = empty_image_cache.get(&ad.path) {
                g.draw_image(img, 10.0, 10.0);
            } else {
                g.set_font(Font::new("Arial", FontStyle::Bold, 13));
                g.set_color(palette::AD_FG);
                g.draw_string(&ad.label, 10.0, LOGICAL_CANVAS_H as f64 / 2.0);
            }
        }
        if state.video_ad_banner.load(Ordering::Relaxed) {
            draw_center_banner(&mut g, "THIS IS AN AD", palette::GOAL_BANNER_BG, palette::GOAL_BORDER);
        }
        if state.replay_banner.load(Ordering::Relaxed) {
            draw_center_banner(&mut g, "REPLAY", palette::SUB_BG, palette::SUB_BORDER);
        }
    } // `cr`/`g` dropped here — required before `surface.data()` can borrow mutably.

    surface.flush();
    let stride = surface.stride() as usize;
    let width = session.canvas_w as usize;
    let height = session.canvas_h as usize;

    let mut out = vec![0u8; width * height * 4];
    {
        let cairo_data = surface
            .data()
            .map_err(|e| anyhow::anyhow!("failed to borrow Cairo surface data: {e:?}"))?;

        // Un-premultiply while copying: Cairo's ARgb32 is premultiplied,
        // native-endian (= BGRA byte order on little-endian), but
        // GStreamer's compositor blends BGRA as straight alpha. See module
        // docs for why this step exists instead of just memcpy-ing.
        for y in 0..height {
            let row = &cairo_data[y * stride..y * stride + width * 4];
            let out_row = &mut out[y * width * 4..(y + 1) * width * 4];
            for x in 0..width {
                let px = &row[x * 4..x * 4 + 4]; // B,G,R,A (premultiplied)
                let a = px[3];
                let out_px = &mut out_row[x * 4..x * 4 + 4];
                if a == 0 {
                    out_px.copy_from_slice(&[0, 0, 0, 0]);
                } else if a == 255 {
                    out_px.copy_from_slice(px);
                } else {
                    let a_f = a as f32 / 255.0;
                    out_px[0] = (px[0] as f32 / a_f).min(255.0) as u8;
                    out_px[1] = (px[1] as f32 / a_f).min(255.0) as u8;
                    out_px[2] = (px[2] as f32 / a_f).min(255.0) as u8;
                    out_px[3] = a;
                }
            }
        }
    }

    let mut buffer = gst::Buffer::with_size(out.len())?;
    {
        let buffer_mut = buffer.get_mut().unwrap();
        let mut map = buffer_mut.map_writable()?;
        map.copy_from_slice(&out);
    }
    Ok(buffer)
}
