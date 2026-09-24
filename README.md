# arena-stream-rs

Rust + GStreamer rewrite of the Java/FFmpeg-subprocess streaming server, per
`gstreamer-migration-analysis.md`. This is a **working skeleton**, not a
finished product — see "What's real vs. stubbed" below before treating any
piece as done.

## Build

```
cargo build --release
```

Requires system GStreamer 1.20+ dev packages, plus Cairo/Pango (the overlay
renderer switched from tiny-skia to Cairo+Pango for real glyph rendering —
see `src/overlay/graphics.rs`) (Ubuntu/Debian):

```
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev pkg-config \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
  gstreamer1.0-rtsp \
  libcairo2-dev libpango1.0-dev
```

### A note on the `Cargo.toml` version pins

The block at the bottom of `Cargo.toml` under "Pins below are only needed
to stay buildable on this sandbox's frozen rustc 1.75" exists **only**
because the sandbox this was built in has an apt-frozen Rust 1.75 toolchain
with no `rustup` access (network policy blocks `static.rust-lang.org`), and
current crates.io packages have started requiring Rust 1.85+ (the
`edition2024` Cargo feature). On a normal machine with a current stable
Rust toolchain (`rustup update stable`, or whatever your CI image ships),
**delete that pin block and run `cargo update`** — none of it is needed.

I was not able to get a full `cargo build` to a clean finish inside this
sandbox: dependency resolution kept landing on different transitive crate
versions across runs as the live crates.io index updated underneath me
(no lockfile existed yet to freeze it, and I couldn't get far enough to
commit one). I got past initial dependency resolution and into actual
compilation at least once (confirmed the `pkg-config`-discovered system
GStreamer 1.24 links correctly, and got as far as ordinary Rust type/borrow
errors in application code, not GStreamer binding errors) but didn't reach
a fully clean build here. **Please run `cargo build` yourself on a normal
toolchain and send me the output** — I'd expect it to be close, but I don't
want to claim "compiles cleanly" without having actually seen it happen.

## What's real vs. stubbed

**Structurally real, worth reviewing closely:**
- `pipeline/mod.rs` — the actual pipeline graph: 3 `appsrc` inputs →
  `compositor` → `tee` (video) / `audiomixer` → `tee` (audio) →
  dynamically-added output branches. `add_output`/`remove_output` use real
  `tee` request-pad add/remove while `PLAYING`.
- `pipeline/transitions.rs` — crossfade / resize-reposition / fallback
  monitor, via stepped property animation on compositor pads. Deliberately
  simpler than `gstreamer_controller::InterpolationControlSource` (noted in
  the module doc) — swap in the controller crate for frame-accurate timing
  when you're past the prototype stage.
- `ingest/frame_assembler.rs` — FU-A reassembly, ported logic from the Java
  version.
- `model/`, `api/`, `control/` — session registry, Laravel HTTP client,
  TCP control socket, structurally complete for the actions listed below.

**Explicitly stubbed / scoped out, flagged in comments where they occur:**
- **Live speed adjustment** — not implemented in general; `pipeline::ads::set_playback_rate`
  (used by `replay`) only really works against a seekable/buffered source,
  not a raw live `appsrc`. Needs its own PTS-remapping design spike for the
  general case.
- **Lineup widget's full timing sequence** — `overlay/graphics.rs` has
  everything needed (`draw_team_lineup`, formation assignment, the whole
  10s SuperSport-style sequence is demonstrated in the HTML preview), but
  the per-session phase/timing state machine to drive it from `OverlayState`
  isn't built yet.
- **Team/channel logo loading** — every widget function that takes an
  `image_cache: &HashMap<String, ImageSurface>` degrades gracefully on a
  miss, but nothing populates that cache. It's called with an empty map
  everywhere in `overlay/mod.rs`.
- **HLS/MP4 branch specifics** — built with `mpegtsmux`+`hlssink2` and
  `mp4mux`+`filesink` respectively; the MP4 "growing file isn't playable
  until finalized" caveat is called out in code comments, not solved.

**Fixed since earlier passes, no longer stubbed:**
- **Multi-camera crossfade on `switch`** — genuinely crossfades video now
  via `pipeline::cameras`, not just audio authority. See WORKFLOW.md §7.4.
- **Overlay text/glyph rendering** — switched from tiny-skia placeholder
  rects to Cairo+Pango (`overlay/graphics.rs`); real text throughout.

## Talking to the Laravel backend

`api/mod.rs` hits the routes in `stream_routes.php` (already delivered
separately). Set `ARENA_API_BASE_URL` and, if needed, `ARENA_API_TOKEN`.
