# arena-stream-rs — Workflow & Behavior Reference

This documents how the system actually behaves end-to-end as currently built:
session lifecycle, every control command, the media pipeline, overlay
rendering, and the ads-management system. Written from a fresh read-through
of the code, which surfaced a few real inconsistencies — those are called
out explicitly in **§7**, and are already fixed in the shipped source, not
just noted.

---

## 1. Process startup

```
main()
 ├─ gstreamer::init()
 ├─ spawn ingest::udp_video::run()      (UDP :5001, RTP/H.264)
 ├─ spawn ingest::udp_audio::run()      (UDP :5002, RTP/PCM16)
 ├─ spawn control::run()                (TCP :5000, newline-JSON commands)
 └─ spawn inactivity_reaper()           (every 5s, tick from METRICS_REPORT_INTERVAL_MS)
```

Nothing session-specific exists yet at this point — `model::REGISTRY` is
empty. A session only comes into being when a `start` command arrives on
the control socket.

---

## 2. Session lifecycle

```
 (no session)
     │  "register"                     "start"
     │  binds sender IP → stream_key    builds pipeline, registers session
     ▼                                  ▼
 ┌─────────────────────────────────────────────────────────────┐
 │  SessionState::Start   (default on creation)                 │
 └─────────────────────────────────────────────────────────────┘
     │ "state" {value:"live"}
     ▼
 ┌─────────────────────────────────────────────────────────────┐
 │  SessionState::Live  ──┐                                     │
 │                        │ "state" {value:"ht"|"et"}            │
 │                        ▼                                     │
 │                  HalfTime / ExtraTime ──(back to Live)──┐    │
 │                        │                                 │    │
 │                        │ "state" {value:"ft"}             │    │
 │                        ▼                                 │    │
 │                     FullTime                              │    │
 └─────────────────────────────────────────────────────────────┘
     │ "stop"
     ▼
 (session torn down, pipeline → NULL, removed from REGISTRY)
```

**What `start` actually does**, in order (`control::run` → `"start"` arm):

1. `pipeline::build_session_pipeline(key)` — builds and PLAYs the GStreamer
   pipeline (§4). Nothing is visible yet: live video starts at `alpha=0`,
   the fallback source at `alpha=1` (see §4.3).
2. Registers the session (`REGISTRY.put`), registers the sender IP →
   stream_key mapping, marks that IP as `current_streamer`.
3. Creates an `OverlayState`, starts `overlay::spawn_overlay_renderer` (§5)
   and `pipeline::transitions::spawn_fallback_monitor` (§4.3).
4. Starts `ads::spawn_ads_loop` (§6) — begins ticking every 5s immediately,
   though it no-ops until `state` moves off `Start`.
5. If `rtmpUrl` was supplied, attaches it via `pipeline::add_output` — the
   *same* function used later for dynamically adding more outputs, so the
   very first destination and every subsequent one go through identical
   code.
6. **Awaits** `api::create_session_awaited` to get the real Laravel
   `stream_sessions.id` and stores it on `session.laravel_id` (§7.2 explains
   why this one call is awaited when everything else in `api::ApiClient` is
   fire-and-forget).
7. Broadcasts `"started"` to any other control-socket clients registered on
   this stream_key.

**What `stop` does:** removes from `REGISTRY`, `pipeline::teardown` (sets
every output branch bin and the main pipeline to `NULL`), clears
`OVERLAY_STATES` and `FANOUT` entries for that key, tells Laravel the
session is `"finished"`.

**Inactivity reaper:** every 5s, any session whose `last_activity_ms` (bumped
by every ingest packet and every control command) is older than
`INACTIVITY_CLEANUP_MS` (15 min) gets force-torn-down the same way `stop`
does.

---

## 3. Control commands (TCP :5000, newline-delimited JSON)

Every command is `{"action": "...", "streamKey": "...", ...}` — `streamKey`
can be omitted after `register` since the connection remembers it.

| Action | Effect |
|---|---|
| `register` | Binds this TCP connection's sender IP to a stream_key, adds it to that stream's fan-out broadcast list. |
| `start` | Builds and starts the session (§2). |
| `stop` | Tears down the session (§2). |
| `switch` `{targetIp}` | Crossfades video from the currently active camera to `targetIp`'s compositor pad (`pipeline::cameras::switch_camera`), builds that camera's branch first if it doesn't exist yet, and updates audio authority in the same call. See §7.4 for the one known edge case around fallback interaction. |
| `add_output` `{id, kind: rtmp\|hls\|mp4, destination}` | Requests a new `tee` pad, builds an encoder/mux/sink branch, links it in live. No pipeline restart. |
| `remove_output` `{id}` | Blocks the tee pad, unlinks, releases the request pad, drops the branch to `NULL`. |
| `resize` `{x,y,width,height}` | Manually animates the live-video compositor pad's geometry — this is the **operator-triggered** resize path. The ads loop's image-ad resize (§6) calls the same underlying `transitions::resize_and_reposition` directly, bypassing the control socket entirely — two different callers, one mechanism. |
| `state` `{value: start\|live\|ht\|et\|ft\|stopped}` | Sets `SessionState`. Drives ads-loop eligibility (§6). Logged to Laravel. |
| `replay` `{speed?}` | Sets `REPLAY` banner flag, calls `pipeline::ads::set_playback_rate(rate)` (default `0.5` if `speed` omitted). See §7.3 for the honest caveat on what this does and doesn't achieve. |
| `replay_end` | Restores rate to `1.0`, clears the banner, frees `video_source_busy`. |
| `goal` `{side: home\|away}` | Increments the relevant `OverlayState` score counter, logs the event, broadcasts. |
| `lineup` / `card` / `substitute` | Logged and broadcast; overlay-state mutation for these three is not yet implemented beyond the pattern comment — same shape as `goal`, not duplicated in code (see §7.5). |

`video_source_busy` (an atomic bool on `StreamSession`) is the mutex
between video ads, image ads, and replay — whichever grabs it first wins;
the others get a clean error rather than corrupting pipeline state.

---

## 4. Media pipeline (GStreamer, one long-lived `Pipeline` per session)

```
 appsrc(video, H.264) ─ h264parse ─ avdec_h264 ─ videoconvert ─ videoscale ─┐
                                                                             ├→ comp.sink_1  (live, zorder 1)
 videotestsrc(fallback, black) ─────────────────────────────────────────────┼→ comp.sink_0  (zorder 0)
                                                                             │
 appsrc(overlay, RGBA 1280×720) ─ videoconvert ─────────────────────────────┴→ comp.sink_2  (zorder 2, topmost)

 compositor "comp" → videoconvert → I420 1280×720 → queue → tee "vtee" ──┬→ branch A (e.g. RTMP)
                                                                          ├→ branch B (e.g. HLS)
                                                                          └→ branch C (added live)

 appsrc(audio, PCM16) ─ audioconvert ─ audioresample ─┐
                                                       ├→ audiomixer "amix" → queue → tee "atee" → (same branches)
       (video/image ad audio requests their own       │
        audiomixer pad here at runtime — §6)          ┘
```

### 4.1 Three inputs
- **Video**: raw H.264 Annex-B NAL units, reassembled from RTP by
  `ingest::frame_assembler` (FU-A defragmentation) and pushed via
  `appsrc.push_buffer`.
- **Audio**: raw PCM16 mono, pushed directly (no reassembly needed — audio
  RTP payloads arrive as complete frames).
- **Overlay**: RGBA buffers rendered by `overlay::render_frame`, pushed at
  `OVERLAY_FPS` (30fps) regardless of ingest cadence.

### 4.2 Dynamic outputs
`add_output`/`remove_output` request/release `tee` pads at runtime — no
pipeline stop, no renegotiation of the existing branches. Each branch is
its own `gst::Bin` (queue → encoder → parse → mux → sink), added/removed as
one unit via ghost pads bridging the tee to the bin's internals.

### 4.3 Fallback crossfade
`pipeline::transitions::spawn_fallback_monitor` polls
`session.ms_since_last_video_buffer()` every 500ms. If it exceeds
`FALLBACK_TIMEOUT_MS` (4s) and the pipeline isn't already showing fallback,
it crossfades `live_video_pad` → `fallback_video_pad` (alpha 1→0 / 0→1 over
`TRANSITION_MS` = 400ms, stepped in software — see §7.1 for the
production-grade alternative). Reverses automatically once video buffers
resume.

### 4.4 Video/image ads and replay (the parts added most recently)
- **Video ad** (`pipeline::ads::start_video_ad`): builds a `uridecodebin`
  bin, requests a **new** `compositor` pad and a **new** `audiomixer` pad
  (not the three fixed ones), links in, crossfades it over `live_video_pad`
  exactly like §4.3's mechanism, ducks live audio volume to 0 while ad
  audio ramps to 1. `stop_video_ad` reverses both crossfades, then tears
  the bin down and releases both pads after giving the crossfade time to
  finish.
- **Image ad**: no new GStreamer elements at all — `live_video_pad` is
  resized/repositioned (same `transitions::resize_and_reposition` the
  `resize` control command uses) to open a left-hand gap, and the overlay
  renderer draws the ad art into that same pixel region on the RGBA layer
  that's already being composited on top. Simpler and cheaper than a
  fourth compositor input.
- **Replay**: `gst::event::Seek` with the requested `rate` sent to the
  pipeline. Honest caveat in §7.3.

---

## 5. Overlay rendering (`overlay::spawn_overlay_renderer`)

Runs a `tokio::time::interval` tick at `OVERLAY_FPS`, each tick calling
`render_frame(&StreamSession, &OverlayState) -> gst::Buffer` and pushing it
into the overlay `appsrc`. `OverlayState` holds a `MatchState` (scorebar
data) plus independent `Option<GoalState>`/`Option<CardState>`/
`Option<SubstitutionState>` slots the control socket populates directly —
no message passing, no locks held across an `.await`.

Draw order per frame (mirrors the real `OverlayRenderer.render()` z-order
from the Java source): scorebar → channel badge → goal/card/substitution
popups (each self-expiring via its own `is_active`/`phase`) → image-ad
panel (if active) → video-ad/replay center banner (if active).

**Rendering switched from tiny-skia to Cairo+Pango** (`overlay/graphics.rs`)
— real glyph rendering via Pango, not placeholder rects. See §7.6.

Geometry is pixel-for-pixel the same numbers used in the HTML preview
(`SB_X=20, SB_Y=20, SB_H=44`, channel badge `X=20`, `H=32`, bottom margin
`20`, center banner `BW=340` matching `EventBannerWidget.java`'s real
constant) — this used to be a mismatch; see §7.1.

---

## 6. Ads-management loop (`ads::spawn_ads_loop`)

One `tokio::spawn` per session, ticking every `TICK_INTERVAL_MS` (5s):

```
tick
 │
 ├─ session still registered? ── no → stop loop
 │
 ├─ state ∈ {Start, Stopped}? ── yes → skip (no ads before kickoff / after end)
 │
 ├─ video_source_busy? ── yes → skip (an ad or replay is already running)
 │
 ├─ within MIN_GAP_MS (20s) of the last ad ending? ── yes → skip
 │
 ├─ eligible_ads(state) empty? ── yes → skip
 │
 └─ pick next (round-robin) → inject_image_ad() or inject_video_ad() (§4.4)
     └─ after ad.duration_ms → automatically reverses (resize back / crossfade back)
```

`CATALOG` is a hardcoded stand-in (`ads/mod.rs`) — swap `eligible_ads()` for
an `api::CLIENT` call against a Laravel `ads` table and nothing else in the
loop changes.

---

## 7. Corrections made while writing this doc

These are real fixes applied to the source, not just notes — listed here so
the reasoning is on record.

### 7.1 Overlay canvas size mismatch (fixed)
`OVERLAY_W/H` were `640×360` while `CANVAS_W/H` were `1280×720`, with the
pipeline upscaling the overlay layer 2× during compositing. But every real
widget I read from your source (`LineupWidget`'s javadoc explicitly says
"canvas = 1280×720", and `OverlayRenderBridge` constructs the renderer with
`OVERLAY_W/H` rather than `CANVAS_W/H`) computes pixel positions assuming
the overlay **is** the full canvas, not a smaller layer. Fixed by setting
`OVERLAY_W = CANVAS_W` / `OVERLAY_H = CANVAS_H` in `config.rs`, dropping the
now-redundant `videoscale` from the overlay branch, and rewriting
`overlay::render_frame`'s hardcoded placeholder rects to the real widget
coordinates (`SB_X/Y/H`, channel badge geometry, `EventBannerWidget`'s
actual `BW=340`) instead of arbitrary numbers.

### 7.2 `session_id: 0` in event logging (fixed)
`goal`/`state`/`lineup`/`card`/`substitute` were all calling
`CLIENT.log_event(0, ...)` — a placeholder that was never replaced with a
real Laravel session id, because `create_session` was fire-and-forget and
never captured the response. Fixed by adding
`api::create_session_awaited` (the one deliberately-awaited call in an
otherwise fire-and-forget client), storing the returned id on
`StreamSession::laravel_id`, and changing every `log_event` call site to
look it up and **skip logging with a warning** rather than send a fake id
if it isn't available yet.

### 7.3 Replay speed — stated plainly, not fixed (can't be, not like this)
`set_playback_rate` sends a `Seek` event with the requested rate, which is
the idiomatic GStreamer approach — but it's designed for seekable sources,
and the three `appsrc` inputs in this pipeline are live/non-seekable. This
will behave correctly for a replay sourced from a buffered/recorded clip
(the same `uridecodebin` pattern `start_video_ad` uses) and likely won't
for a raw live source. This is the same PTS-remapping gap flagged in the
original migration analysis — restating it here rather than letting the
`replay` command's existence imply it's solved.

### 7.4 Multi-camera scene-switch crossfade (fixed)
`switch` used to only update audio authority — there was still only one
live-video compositor input, so "switching" did nothing visual. Fixed
properly, not just patched:

- **New `CameraInput` model** (`model/session.rs`): `Primary` (the camera
  wired in at pipeline-build time) or `Dynamic` (added later, each with its
  own small decode bin — appsrc → h264parse → avdec_h264 → videoconvert →
  videoscale — linked into a freshly requested `compositor` pad). Building
  this needed no `uridecodebin`/dynamic-pad handling like `ads::start_video_ad`
  does, since the input is already raw H.264 pushed via `appsrc` — every
  element has a static pad, so `gst::parse::bin_from_description(desc, true)`
  builds and auto-ghosts it in one call (`pipeline/cameras.rs`).
- **`switch` now genuinely crossfades** (`pipeline::cameras::switch_camera`)
  using the same `transitions::crossfade` primitive as fallback/ad
  transitions — no new animation mechanism needed, just a new pad to point
  it at.
- **Found and fixed a real bug while wiring the ingest side**:
  `ingest::udp_video::run` was pushing *every* recognized session's video
  packets into the single `session.video_appsrc`, regardless of which
  client sent them — meaning two camera operators on one session would
  have their independently-encoded H.264 interleaved into one appsrc,
  which is invalid input for `h264parse` downstream. Now routes by sender
  IP into that camera's own `CameraInput` appsrc, building the branch
  lazily on first packet if `register` didn't already.
- **Fallback monitor updated** to crossfade against whichever camera is
  actually active (`cameras::active_or_primary_pad`), not always the
  primary — otherwise fallback recovery would fade in the wrong camera
  after a `switch`.

**One known edge case, not fully solved:** if the primary camera goes
dead but a second, never-explicitly-`switch`ed-to camera keeps sending
video, `ms_since_last_video_buffer` (bumped by *any* camera's packets)
tells the fallback monitor "video is fine," so it won't engage fallback —
but it crossfades toward `active_camera_ip`'s pad, which is still the dead
primary, so the output goes black anyway despite the monitor believing
video is live. Correct fix is per-camera buffer freshness tracking, not
just session-wide; flagging rather than papering over it.

### 7.5 Duplicate `"replay"` match arm (fixed, was a real bug)
The catch-all arm `"lineup" | "card" | "substitute" | "replay" => { ... }`
still had `"replay"` in its pattern list from an early draft, *after* a
dedicated `"replay"` arm (calling `ads::start_replay`) had already been
added earlier in the same `match`. Rust match arms are checked in order,
so the dedicated arm always won and the `"replay"` branch of the catch-all
was unreachable dead code — harmless by luck (both arms would broadcast
something plausible), but not what was intended. Removed `"replay"` from
the catch-all's pattern.

### 7.6 Glyph rendering (fixed)
Flagged since the very first pipeline skeleton as solid-color placeholder
rects instead of real text. Fixed by switching the entire overlay renderer
from tiny-skia to Cairo+Pango (`overlay/graphics.rs`) — real text
throughout, plus a considerably richer widget set than what was hand-ported
from the Java source directly: 15 formations vs. 11 in `LineupWidget.java`,
a full multi-pass player-to-formation assignment algorithm, real
goal/card/substitution popup widgets driven by actual control-socket
payloads instead of the "same pattern, elided" placeholder that `goal`/
`card`/`substitute` used to be.

Two things this fix surfaced, not solved by it:
- **Premultiplied-alpha handling.** Cairo's `Format::ARgb32` is
  premultiplied; GStreamer's compositor expects straight alpha for `BGRA`.
  `render_frame` now un-premultiplies per-pixel while copying out of the
  Cairo surface — correct, but a real per-frame cost worth profiling
  before assuming it's free at scale (N concurrent sessions × 30fps ×
  1280×720 divides).
- **Lineup's full timing sequence still isn't wired into `OverlayState`.**
  `overlay/graphics.rs` has everything the HTML preview demonstrates
  (`draw_team_lineup`, formation assignment, the whole 10s sequence's
  drawing logic) — what's missing is the per-session phase/timing state
  machine (mirroring `LineupWidget.java`'s `paint()` control flow) to
  actually drive it frame-to-frame. Real remaining work, not hidden behind
  vague "TODO".

---

## 8. What to build/test first, given all of the above

1. **`cargo build`** — I still haven't gotten a clean build in my sandbox
   (frozen Rust 1.75, no path to a newer toolchain). Please run it on a
   real machine and send me errors before trusting any of this further.
2. Confirm `StreamConfig`'s real `OVERLAY_W`/`OVERLAY_H`/scorebar/badge
   pixel constants against §7.1's fix — I derived them from the widget
   source's own math, not from a `StreamConfig.java` file (it wasn't in
   either zip), so treat them as "consistent with the widgets" rather than
   "confirmed against your actual config file."
3. `replay`'s rate change (§7.3) still only really works against a
   seekable/buffered source — decide how replay footage actually gets
   sourced before trusting it end-to-end. `switch` (§7.4) is now real,
   modulo the fallback-interaction edge case noted there.
4. **New system deps**: `libcairo2-dev`, `libpango1.0-dev` (§7.6's fix).
   Make sure these are on whatever machine/CI image builds this — they
   weren't needed before this pass.
5. Profile the overlay renderer's per-pixel un-premultiply pass (§7.6)
   under real concurrent-session load before assuming it's free.
