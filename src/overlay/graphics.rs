//! Cairo+Pango overlay rendering primitives and widget draw functions.
//!
//! Replaces the earlier tiny-skia-based `overlay::render_frame`, which drew
//! solid-color placeholder rects instead of real text — flagged as a known
//! gap since the first pipeline pass (WORKFLOW.md §7.6). This gives real
//! glyph rendering via Pango, plus considerably richer widget logic than
//! what was hand-rolled from the Java source directly: 15 formations here
//! vs. 11 in `LineupWidget.java`, a full multi-pass player-to-formation
//! assignment algorithm, and jersey/player rendering with real names.
//!
//! **Dropped on integration:** `FFmpegProcess` — a leftover struct from the
//! old FFmpeg-subprocess architecture (`current_team_a`, `pipe1: Option<String>`,
//! etc.) with no meaning in a GStreamer pipeline. Everything else is kept
//! as given.
//!
//! **Not yet wired in `overlay/mod.rs`:** `draw_match_performance_widget` —
//! a full-screen "match center" popup with its own phase timing, distinct
//! from anything in the original `overlay.zip` widget set. It's exposed
//! here in case it maps to a real product surface, but nothing calls it
//! yet; flagging rather than silently activating a widget nobody asked to
//! see on stream.
//!
//! **`image_cache: &HashMap<String, ImageSurface>` is passed as an empty
//! map everywhere it's called from `overlay/mod.rs`** — there's no asset
//! loading system in this codebase yet (team logos, channel logos). Every
//! function here already degrades gracefully when a lookup misses (logo
//! just doesn't render), so this is safe, just incomplete. Wiring real
//! logo loading is a separate piece of work.
//!
//! Uses the current pango-rs/cairo-rs API: `Layout::extents()`/`baseline()`
//! (not the older `get_`-prefixed gtk-rs names), and `Context::translate`/
//! `scale` which return `()` rather than `Result` in this crate version.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use cairo::{Context, Format, ImageSurface, LinearGradient};
use lazy_static::lazy_static;
use pango::{FontDescription, Layout, Weight};
use pangocairo::functions::{create_layout, show_layout};

// =====================================================================
// Core Types & Wrappers
// =====================================================================

#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r: r as f64 / 255.0, g: g as f64 / 255.0, b: b as f64 / 255.0, a: a as f64 / 255.0 }
    }
    pub fn from_argb(argb: u32) -> Self {
        let a = ((argb >> 24) & 0xFF) as f64 / 255.0;
        let r = ((argb >> 16) & 0xFF) as f64 / 255.0;
        let g = ((argb >> 8) & 0xFF) as f64 / 255.0;
        let b = (argb & 0xFF) as f64 / 255.0;
        Self { r, g, b, a }
    }
    pub const WHITE: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    pub const BLACK: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const LIGHT_GRAY: Color = Color { r: 0.75, g: 0.75, b: 0.75, a: 1.0 };
    pub const RED: Color = Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const GREEN: Color = Color { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
}

#[derive(Clone, Debug)]
pub struct Font {
    pub family: String,
    pub style: FontStyle,
    pub size: i32,
}

#[derive(Clone, Debug)]
pub enum FontStyle {
    Plain,
    Bold,
}

impl Font {
    pub fn new(family: &str, style: FontStyle, size: i32) -> Self {
        Self { family: family.to_string(), style, size }
    }
    pub fn derive_font(&self, size: f32) -> Self {
        Self { family: self.family.clone(), style: FontStyle::Bold, size: size as i32 }
    }
}

pub struct FontMetrics {
    pub layout: Layout,
}

impl FontMetrics {
    pub fn string_width(&self, text: &str) -> i32 {
        self.layout.set_text(text);
        let (_, logical_rect) = self.layout.extents();
        (logical_rect.width() / pango::SCALE) as i32
    }
    pub fn get_height(&self) -> i32 {
        let (_, logical_rect) = self.layout.extents();
        (logical_rect.height() / pango::SCALE) as i32
    }
    pub fn get_ascent(&self) -> i32 {
        (self.layout.baseline() / pango::SCALE) as i32
    }
}

pub struct BasicStroke {
    pub width: f64,
}
impl BasicStroke {
    pub fn new(width: f64) -> Self {
        Self { width }
    }
}

pub struct GradientPaint {
    pub x1: f64,
    pub y1: f64,
    pub c1: Color,
    pub x2: f64,
    pub y2: f64,
    pub c2: Color,
}

// Java Graphics2D equivalent Wrapper
pub struct Graphics2D<'a> {
    pub cr: &'a Context,
    pub current_font: Font,
    pub global_alpha: f64,
}

impl<'a> Graphics2D<'a> {
    pub fn new(cr: &'a Context) -> Self {
        Self { cr, current_font: Font::new("Arial", FontStyle::Plain, 12), global_alpha: 1.0 }
    }

    pub fn set_color(&self, c: Color) {
        self.cr.set_source_rgba(c.r, c.g, c.b, c.a * self.global_alpha);
    }

    pub fn set_stroke(&self, s: BasicStroke) {
        self.cr.set_line_width(s.width);
    }
    pub fn set_font(&mut self, f: Font) {
        self.current_font = f;
    }

    pub fn get_font_metrics(&self) -> FontMetrics {
        let layout = create_layout(self.cr);
        let mut desc = FontDescription::new();
        desc.set_family(&self.current_font.family);
        desc.set_size(self.current_font.size * pango::SCALE);
        if let FontStyle::Bold = self.current_font.style {
            desc.set_weight(Weight::Bold);
        }
        layout.set_font_description(Some(&desc));
        FontMetrics { layout }
    }

    pub fn draw_string(&self, text: &str, x: f64, y: f64) {
        let layout = create_layout(self.cr);
        let mut desc = FontDescription::new();
        desc.set_family(&self.current_font.family);
        desc.set_size(self.current_font.size * pango::SCALE);
        if let FontStyle::Bold = self.current_font.style {
            desc.set_weight(Weight::Bold);
        }
        layout.set_font_description(Some(&desc));
        layout.set_text(text);

        let ascent = (layout.baseline() / pango::SCALE) as f64;
        self.cr.move_to(x, y - ascent);
        show_layout(self.cr, &layout);
    }

    pub fn draw_image(&self, img: &ImageSurface, x: f64, y: f64) {
        self.cr.set_source_surface(img, x, y).unwrap();
        self.cr.paint_with_alpha(self.global_alpha).unwrap();
    }

    pub fn set_paint(&self, grad: GradientPaint) {
        let pattern = LinearGradient::new(grad.x1, grad.y1, grad.x2, grad.y2);
        pattern.add_color_stop_rgba(0.0, grad.c1.r, grad.c1.g, grad.c1.b, grad.c1.a * self.global_alpha);
        pattern.add_color_stop_rgba(1.0, grad.c2.r, grad.c2.g, grad.c2.b, grad.c2.a * self.global_alpha);
        self.cr.set_source(&pattern).unwrap();
    }

    pub fn set_composite_alpha(&mut self, opacity: f64) {
        self.global_alpha = opacity;
    }

    pub fn fill_round_rect(&self, x: f64, y: f64, w: f64, h: f64, r: f64) {
        rounded_rectangle(self.cr, x, y, w, h, r / 2.0);
        self.cr.fill().unwrap();
    }

    pub fn draw_round_rect(&self, x: f64, y: f64, w: f64, h: f64, r: f64) {
        rounded_rectangle(self.cr, x, y, w, h, r / 2.0);
        self.cr.stroke().unwrap();
    }

    pub fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.cr.rectangle(x, y, w, h);
        self.cr.fill().unwrap();
    }

    pub fn draw_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.cr.rectangle(x, y, w, h);
        self.cr.stroke().unwrap();
    }

    pub fn draw_line(&self, x1: f64, y1: f64, x2: f64, y2: f64) {
        self.cr.move_to(x1, y1);
        self.cr.line_to(x2, y2);
        self.cr.stroke().unwrap();
    }

    pub fn fill_oval(&self, x: f64, y: f64, w: f64, h: f64) {
        self.cr.save().unwrap();
        self.cr.translate(x + w / 2.0, y + h / 2.0);
        self.cr.scale(w / 2.0, h / 2.0);
        self.cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
        self.cr.fill().unwrap();
        self.cr.restore().unwrap();
    }

    pub fn draw_oval(&self, x: f64, y: f64, w: f64, h: f64) {
        self.cr.save().unwrap();
        self.cr.translate(x + w / 2.0, y + h / 2.0);
        self.cr.scale(w / 2.0, h / 2.0);
        self.cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
        self.cr.stroke().unwrap();
        self.cr.restore().unwrap();
    }

    pub fn draw_arc(&self, x: f64, y: f64, w: f64, h: f64, start_deg: f64, extent_deg: f64) {
        self.cr.save().unwrap();
        self.cr.translate(x + w / 2.0, y + h / 2.0);
        self.cr.scale(w / 2.0, h / 2.0);
        let start_rad = -start_deg.to_radians();
        let end_rad = -(start_deg + extent_deg).to_radians();
        if extent_deg > 0.0 {
            self.cr.arc_negative(0.0, 0.0, 1.0, start_rad, end_rad);
        } else {
            self.cr.arc(0.0, 0.0, 1.0, start_rad, end_rad);
        }
        self.cr.stroke().unwrap();
        self.cr.restore().unwrap();
    }

    pub fn fill_polygon(&self, x_points: &[f64], y_points: &[f64], num_points: usize) {
        if num_points == 0 {
            return;
        }
        self.cr.move_to(x_points[0], y_points[0]);
        for i in 1..num_points {
            self.cr.line_to(x_points[i], y_points[i]);
        }
        self.cr.close_path();
        self.cr.fill().unwrap();
    }
}

fn rounded_rectangle(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -90.0_f64.to_radians(), 0.0_f64.to_radians());
    cr.arc(x + w - r, y + h - r, r, 0.0_f64.to_radians(), 90.0_f64.to_radians());
    cr.arc(x + r, y + h - r, r, 90.0_f64.to_radians(), 180.0_f64.to_radians());
    cr.arc(x + r, y + r, r, 180.0_f64.to_radians(), 270.0_f64.to_radians());
    cr.close_path();
}

// =====================================================================
// State Models
// =====================================================================

pub struct MatchState {
    pub is_active: bool,
    pub animation_start_time: u128,
    pub phase: i32,
    pub team_a_name: String,
    pub team_b_name: String,
    pub team_a_score: i32,
    pub team_b_score: i32,
    pub current_time: String,
}

pub struct GoalState {
    pub is_active: bool,
    pub animation_start_time: u128,
    pub phase: i32,
    pub team_name: String,
    pub player_name: String,
    pub new_score: i32,
}

pub struct CardState {
    pub is_active: bool,
    pub animation_start_time: u128,
    pub phase: i32,
    pub team_name: String,
    pub player_name: String,
    pub card_type: CardType,
    pub reason: Option<String>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum CardType {
    YellowCard,
    RedCard,
}

pub struct SubstitutionState {
    pub is_active: bool,
    pub animation_start_time: u128,
    pub phase: i32,
    pub team_name: String,
    pub player_out: String,
    pub player_in: String,
}

#[derive(Clone)]
pub struct SubstitutionTable {
    pub is_active: bool,
    pub animation_start_time: u128,
    pub phase: i32,
    pub team_name: String,
    pub team_color: u32,
    pub substitutes: Vec<SubstitutePlayer>,
}

#[derive(Clone)]
pub struct SubstitutePlayer {
    pub name: String,
    pub jersey_number: i32,
    pub position: String,
    pub is_goalkeeper: bool,
}

pub struct PlayerAssignment {
    pub player: HashMap<String, String>,
    pub assigned_position: FormationPosition,
    pub exact_match: bool,
    pub score: i32,
}

pub struct FormationPosition {
    pub position: String,
    pub group: String,
    pub x: i32,
    pub y: i32,
}

lazy_static! {
    static ref POSITION_GROUPS: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("GK", "GK");
        m.insert("CB", "DEF");
        m.insert("LB", "DEF");
        m.insert("RB", "DEF");
        m.insert("LWB", "DEF");
        m.insert("RWB", "DEF");
        m.insert("CM", "MID");
        m.insert("CDM", "MID");
        m.insert("CAM", "MID");
        m.insert("LM", "MID");
        m.insert("RM", "MID");
        m.insert("DM", "MID");
        m.insert("CF", "FWD");
        m.insert("ST", "FWD");
        m.insert("LW", "FWD");
        m.insert("RW", "FWD");
        m
    };
}

// =====================================================================
// Helper Functions
// =====================================================================

pub fn current_time_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

pub fn create_rounded_image(img: &ImageSurface, size: i32) -> Option<ImageSurface> {
    let surface = ImageSurface::create(Format::ARgb32, size, size).ok()?;
    let cr = Context::new(&surface).ok()?;
    rounded_rectangle(&cr, 0.0, 0.0, size as f64, size as f64, 15.0);
    cr.clip();
    cr.set_source_surface(img, 0.0, 0.0).unwrap();
    cr.paint().unwrap();
    Some(surface)
}

pub fn truncate_name(name: &str, metrics: &FontMetrics, max_width: i32) -> String {
    if metrics.string_width(name) <= max_width {
        return name.to_string();
    }
    let parts: Vec<&str> = name.split(' ').collect();
    if parts.len() > 1 {
        let last_initial = parts.last().unwrap().chars().next().unwrap_or('?');
        let shortened = format!("{} {}.", parts[0], last_initial);
        if metrics.string_width(&shortened) <= max_width {
            return shortened;
        }
    }
    let mut truncated = name.to_string();
    while metrics.string_width(&format!("{}...", truncated)) > max_width && !truncated.is_empty() {
        truncated.pop();
    }
    format!("{}...", truncated)
}

// =====================================================================
// Widget Renderers
// =====================================================================

pub fn draw_main_widget(
    g: &mut Graphics2D,
    team_a: &str,
    team_b: &str,
    score_a: i32,
    score_b: i32,
    time_string: &str,
    widget_x: f64,
    widget_y: f64,
    widget_width: f64,
    widget_height: f64,
    bg_color: Color,
    border_color: Color,
    team_a_color: Color,
    team_b_color: Color,
    score_color: Color,
    time_color: Color,
    period: &str,
    image_cache: &HashMap<String, ImageSurface>,
) {
    g.set_color(bg_color);
    g.fill_round_rect(widget_x, widget_y, widget_width, widget_height, 15.0);
    g.set_color(border_color);
    g.set_stroke(BasicStroke::new(1.0));
    g.draw_round_rect(widget_x, widget_y, widget_width, widget_height, 15.0);

    let team_font = Font::new("Arial", FontStyle::Bold, 18);
    let score_font = Font::new("Arial", FontStyle::Bold, 24);
    let vs_font = Font::new("Arial", FontStyle::Bold, 16);
    let time_font = Font::new("Courier New", FontStyle::Bold, 18);
    let period_font = Font::new("Arial", FontStyle::Plain, 14);

    let center_y = widget_y + (widget_height / 2.0) + 6.0;

    let logo_a = image_cache.get(team_a).and_then(|img| create_rounded_image(img, 30));
    let logo_b = image_cache.get(team_b).and_then(|img| create_rounded_image(img, 30));

    g.set_font(team_font.clone());
    let team_metrics = g.get_font_metrics();
    g.set_font(score_font.clone());
    let score_metrics = g.get_font_metrics();
    g.set_font(vs_font.clone());
    let vs_metrics = g.get_font_metrics();
    g.set_font(time_font.clone());
    let time_metrics = g.get_font_metrics();
    g.set_font(period_font.clone());
    let period_metrics = g.get_font_metrics();

    let score_a_str = score_a.to_string();
    let score_b_str = score_b.to_string();
    let vs_str = "VS";
    let logo_size = 30.0;
    let component_spacing = 15.0;

    let team_a_width =
        (if logo_a.is_some() { logo_size + 5.0 } else { 0.0 }) + team_metrics.string_width(team_a) as f64;
    let score_a_width = score_metrics.string_width(&score_a_str) as f64;
    let vs_width = vs_metrics.string_width(vs_str) as f64;
    let score_b_width = score_metrics.string_width(&score_b_str) as f64;
    let team_b_width =
        (if logo_b.is_some() { logo_size + 5.0 } else { 0.0 }) + team_metrics.string_width(team_b) as f64;
    let time_width = time_metrics.string_width(time_string) as f64;
    let period_width = period_metrics.string_width(period) as f64;

    let total_content_width = team_a_width
        + score_a_width
        + vs_width
        + score_b_width
        + team_b_width
        + time_width
        + period_width
        + (component_spacing * 6.0);

    let start_x = widget_x + (widget_width - total_content_width) / 2.0;
    let mut current_x = start_x;

    if let Some(la) = &logo_a {
        g.draw_image(la, current_x, center_y - logo_size / 2.0 - 3.0);
        current_x += logo_size + 5.0;
    }
    g.set_font(team_font.clone());
    g.set_color(team_a_color);
    g.draw_string(team_a, current_x, center_y);
    current_x += team_metrics.string_width(team_a) as f64 + component_spacing;

    g.set_font(score_font.clone());
    g.set_color(score_color);
    g.draw_string(&score_a_str, current_x, center_y);
    current_x += score_a_width + component_spacing;

    g.set_font(vs_font.clone());
    g.set_color(Color::LIGHT_GRAY);
    g.draw_string(vs_str, current_x, center_y);
    current_x += vs_width + component_spacing;

    g.set_font(score_font.clone());
    g.set_color(score_color);
    g.draw_string(&score_b_str, current_x, center_y);
    current_x += score_b_width + component_spacing;

    if let Some(lb) = &logo_b {
        g.draw_image(lb, current_x, center_y - logo_size / 2.0 - 3.0);
        current_x += logo_size + 5.0;
    }
    g.set_font(team_font.clone());
    g.set_color(team_b_color);
    g.draw_string(team_b, current_x, center_y);
    current_x += team_metrics.string_width(team_b) as f64 + component_spacing;

    g.set_font(period_font.clone());
    g.set_color(Color::LIGHT_GRAY);
    g.draw_string(period, current_x, center_y);
    current_x += period_width + component_spacing;

    g.set_font(time_font.clone());
    g.set_color(time_color);
    g.draw_string(time_string, current_x, center_y);
}

pub fn draw_match_performance_widget(
    current_match: &mut MatchState,
    g: &mut Graphics2D,
    screen_width: f64,
    screen_height: f64,
    match_widget_width: f64,
    match_widget_height: f64,
    _bg_color: Color,
    _border_color: Color,
    period: &str,
    image_cache: &HashMap<String, ImageSurface>,
) {
    if !current_match.is_active {
        return;
    }
    let elapsed = current_time_millis() - current_match.animation_start_time;

    if elapsed > 9000 {
        current_match.is_active = false;
        return;
    } else if elapsed > 8000 {
        current_match.phase = 2;
    } else if elapsed > 1000 {
        current_match.phase = 1;
    } else {
        current_match.phase = 0;
    }

    let base_x = (screen_width - match_widget_width) / 2.0;
    let base_y = (screen_height - match_widget_height) / 2.0;

    let mut scale = 1.0;
    let mut alpha = 1.0;

    if current_match.phase == 0 {
        let progress = elapsed as f64 / 1000.0;
        scale = 0.1 + (0.9 * progress);
        alpha = progress;
    } else if current_match.phase == 2 {
        let progress = (elapsed - 8000) as f64 / 1000.0;
        scale = 1.0 + (progress * 2.0);
        alpha = 1.0 - progress;
    }

    let current_width = match_widget_width * scale;
    let current_height = match_widget_height * scale;

    let match_x = base_x - (current_width - match_widget_width) / 2.0;
    let match_y = base_y - (current_height - match_widget_height) / 2.0;

    let alpha_value = (alpha * 220.0) as u8;

    let gradient = GradientPaint {
        x1: match_x,
        y1: match_y,
        c1: Color::new(25, 50, 100, alpha_value),
        x2: match_x,
        y2: match_y + current_height,
        c2: Color::new(15, 30, 80, alpha_value),
    };
    g.set_paint(gradient);
    g.fill_round_rect(match_x, match_y, current_width, current_height, 15.0);

    g.set_stroke(BasicStroke::new(2.0));
    g.set_color(Color::new(255, 255, 255, (alpha * 150.0) as u8));
    g.draw_round_rect(match_x, match_y, current_width, current_height, 15.0);

    g.set_stroke(BasicStroke::new(1.0));
    g.set_color(Color::new(100, 150, 255, (alpha * 100.0) as u8));
    g.draw_round_rect(match_x + 3.0, match_y + 3.0, current_width - 6.0, current_height - 6.0, 12.0);

    let font_scale = if current_match.phase == 2 { current_width / match_widget_width } else { 1.0 };
    let team_font = Font::new("Arial", FontStyle::Bold, (20.0 * font_scale) as i32);
    let score_font = Font::new("Arial", FontStyle::Bold, (36.0 * font_scale) as i32);
    let time_font = Font::new("Arial", FontStyle::Bold, (24.0 * font_scale) as i32);
    let period_font = Font::new("Arial", FontStyle::Bold, (16.0 * font_scale) as i32);
    let vs_font = Font::new("Arial", FontStyle::Bold, (14.0 * font_scale) as i32);

    let mut content_y = match_y + (30.0 * font_scale);
    let center_x = match_x + current_width / 2.0;

    g.set_font(time_font.clone());
    g.set_color(Color::new(255, 255, 100, (alpha * 255.0) as u8));
    let time_text = &current_match.current_time;
    g.set_font(time_font.clone());
    let time_metrics = g.get_font_metrics();
    let time_width = time_metrics.string_width(time_text) as f64;
    g.draw_string(time_text, center_x - time_width / 2.0, content_y);

    content_y += 25.0 * font_scale;
    g.set_font(period_font.clone());
    g.set_color(Color::new(200, 200, 200, (alpha * 255.0) as u8));
    let period_text = period;
    g.set_font(period_font.clone());
    let period_metrics = g.get_font_metrics();
    let period_width = period_metrics.string_width(period_text) as f64;
    g.draw_string(period_text, center_x - period_width / 2.0, content_y);

    content_y += 35.0 * font_scale;

    let team_a_x = match_x + (20.0 * font_scale);
    let team_b_x = match_x + current_width - (20.0 * font_scale);

    g.set_font(team_font.clone());
    g.set_color(Color::new(255, 255, 255, (alpha * 255.0) as u8));

    let mut team_a_text_x = team_a_x;
    if let Some(team_a_logo) = image_cache.get(&current_match.team_a_name) {
        if let Some(rounded_logo) = create_rounded_image(team_a_logo, 45) {
            let logo_size = 45.0 * font_scale;
            g.cr.save().unwrap();
            g.cr.translate(team_a_x, content_y - (20.0 * font_scale));
            g.cr.scale(logo_size / 45.0, logo_size / 45.0);
            g.draw_image(&rounded_logo, 0.0, 0.0);
            g.cr.restore().unwrap();
            team_a_text_x += 55.0 * font_scale;
        }
    }

    g.set_font(team_font.clone());
    let _team_metrics = g.get_font_metrics();
    let team_a_text = if current_match.team_a_name.len() > 12 {
        format!("{}...", &current_match.team_a_name.chars().take(12).collect::<String>())
    } else {
        current_match.team_a_name.clone()
    };
    g.draw_string(&team_a_text, team_a_text_x, content_y);

    g.set_font(vs_font.clone());
    g.set_color(Color::new(180, 180, 180, (alpha * 255.0) as u8));
    let vs_text = "VS";
    g.set_font(vs_font.clone());
    let vs_metrics = g.get_font_metrics();
    let vs_width = vs_metrics.string_width(vs_text) as f64;
    g.draw_string(vs_text, center_x - vs_width / 2.0, content_y - (5.0 * font_scale));

    g.set_font(team_font.clone());
    g.set_color(Color::new(255, 255, 255, (alpha * 255.0) as u8));

    let team_b_text = if current_match.team_b_name.len() > 12 {
        format!("{}...", &current_match.team_b_name.chars().take(12).collect::<String>())
    } else {
        current_match.team_b_name.clone()
    };

    g.set_font(team_font.clone());
    let team_b_metrics = g.get_font_metrics();
    let team_b_text_width = team_b_metrics.string_width(&team_b_text) as f64;

    let logo_size = 45.0 * font_scale;
    let spacing = 10.0 * font_scale;
    let total_team_b_width = if image_cache.contains_key(&current_match.team_b_name) {
        logo_size + spacing + team_b_text_width
    } else {
        team_b_text_width
    };

    let team_b_start_x = team_b_x - total_team_b_width;

    if let Some(team_b_logo) = image_cache.get(&current_match.team_b_name) {
        if let Some(rounded_logo) = create_rounded_image(team_b_logo, 45) {
            g.cr.save().unwrap();
            g.cr.translate(team_b_start_x, content_y - (20.0 * font_scale));
            g.cr.scale(logo_size / 45.0, logo_size / 45.0);
            g.draw_image(&rounded_logo, 0.0, 0.0);
            g.cr.restore().unwrap();

            let team_b_text_x = team_b_start_x + logo_size + spacing;
            g.draw_string(&team_b_text, team_b_text_x, content_y);
        }
    } else {
        g.draw_string(&team_b_text, team_b_start_x, content_y);
    }

    content_y += 40.0 * font_scale;
    g.set_font(score_font.clone());
    g.set_color(Color::new(255, 255, 255, (alpha * 255.0) as u8));

    let score_a_text = current_match.team_a_score.to_string();
    g.set_font(score_font.clone());
    let score_metrics = g.get_font_metrics();
    let score_a_width = score_metrics.string_width(&score_a_text) as f64;
    let score_a_x = center_x - (30.0 * font_scale) - score_a_width;
    g.draw_string(&score_a_text, score_a_x, content_y);

    g.set_color(Color::new(200, 200, 200, (alpha * 255.0) as u8));
    g.draw_string("-", center_x - (8.0 * font_scale), content_y);

    g.set_color(Color::new(255, 255, 255, (alpha * 255.0) as u8));
    let score_b_text = current_match.team_b_score.to_string();
    g.draw_string(&score_b_text, center_x + (20.0 * font_scale), content_y);

    if current_match.phase == 1 {
        let pulse_alpha = 0.3 + 0.2 * ((elapsed - 1000) as f64 / 300.0).sin();
        g.set_color(Color::new(255, 255, 100, (pulse_alpha * 255.0) as u8));
        g.set_stroke(BasicStroke::new(2.0));

        let pulse_x = score_a_x - (10.0 * font_scale);
        let pulse_y = content_y - score_metrics.get_height() as f64 + (5.0 * font_scale);
        let pulse_width =
            score_metrics.string_width(&format!("{} - {}", score_a_text, score_b_text)) as f64 + (40.0 * font_scale);
        let pulse_height = score_metrics.get_height() as f64 + (10.0 * font_scale);

        g.draw_round_rect(pulse_x, pulse_y, pulse_width, pulse_height, 8.0);
    }
}

pub fn draw_goal_widget(
    current_goal: &mut GoalState,
    g: &mut Graphics2D,
    screen_width: f64,
    screen_height: f64,
    goal_widget_width: f64,
    goal_widget_height: f64,
    _bg_color: Color,
    _border_color: Color,
    image_cache: &HashMap<String, ImageSurface>,
) {
    if !current_goal.is_active {
        return;
    }
    let elapsed = current_time_millis() - current_goal.animation_start_time;

    if elapsed > 5000 {
        current_goal.is_active = false;
        return;
    } else if elapsed > 4000 {
        current_goal.phase = 2;
    } else if elapsed > 1000 {
        current_goal.phase = 1;
    } else {
        current_goal.phase = 0;
    }

    let base_x = (screen_width - goal_widget_width) / 2.0;
    let base_y = (screen_height - goal_widget_height) / 2.0;

    let goal_x = base_x;
    let mut goal_y = base_y;

    if current_goal.phase == 0 {
        let progress = elapsed as f64 / 1000.0;
        goal_y = base_y - (goal_widget_height + 100.0) * (1.0 - progress);
    } else if current_goal.phase == 2 {
        let progress = (elapsed - 4000) as f64 / 1000.0;
        goal_y = base_y + (goal_widget_height + 100.0) * progress;
    }

    let gradient = GradientPaint {
        x1: goal_x,
        y1: goal_y,
        c1: Color::new(255, 215, 0, 200),
        x2: goal_x,
        y2: goal_y + goal_widget_height,
        c2: Color::new(255, 140, 0, 200),
    };
    g.set_paint(gradient);
    g.fill_round_rect(goal_x, goal_y, goal_widget_width, goal_widget_height, 20.0);

    g.set_stroke(BasicStroke::new(3.0));
    g.set_color(Color::new(255, 255, 255, 180));
    g.draw_round_rect(goal_x, goal_y, goal_widget_width, goal_widget_height, 20.0);

    g.set_stroke(BasicStroke::new(1.0));
    g.set_color(Color::new(255, 215, 0, 100));
    g.draw_round_rect(goal_x + 5.0, goal_y + 5.0, goal_widget_width - 10.0, goal_widget_height - 10.0, 15.0);

    let team_logo = image_cache.get(&current_goal.team_name).and_then(|img| create_rounded_image(img, 50));

    let title_font = Font::new("Arial", FontStyle::Bold, 28);
    let team_font = Font::new("Arial", FontStyle::Bold, 22);
    let player_font = Font::new("Arial", FontStyle::Bold, 20);
    let score_font = Font::new("Arial", FontStyle::Bold, 32);

    let mut content_y = goal_y + 35.0;
    let center_x = goal_x + goal_widget_width / 2.0;

    g.set_font(title_font.clone());
    g.set_color(Color::WHITE);
    let goal_text = "GOAL!";
    g.set_font(title_font.clone());
    let title_metrics = g.get_font_metrics();
    let title_width = title_metrics.string_width(goal_text) as f64;
    g.draw_string(goal_text, center_x - title_width / 2.0, content_y);

    content_y += 35.0;

    g.set_font(team_font.clone());
    g.set_font(team_font.clone());
    let team_metrics = g.get_font_metrics();
    let team_width = team_metrics.string_width(&current_goal.team_name) as f64;
    let logo_space = if team_logo.is_some() { 60.0 } else { 0.0 };
    let total_team_width = team_width + logo_space;

    let mut team_start_x = center_x - total_team_width / 2.0;

    if let Some(tl) = &team_logo {
        g.draw_image(tl, team_start_x, content_y - 25.0);
        team_start_x += 60.0;
    }

    g.set_color(Color::WHITE);
    g.draw_string(&current_goal.team_name, team_start_x, content_y);

    content_y += 30.0;

    g.set_font(player_font.clone());
    g.set_color(Color::new(255, 255, 200, 255));
    g.set_font(player_font.clone());
    let player_metrics = g.get_font_metrics();
    let player_width = player_metrics.string_width(&current_goal.player_name) as f64;
    g.draw_string(&current_goal.player_name, center_x - player_width / 2.0, content_y);

    if current_goal.phase == 1 {
        let pulse_scale = 1.0 + 0.2 * ((elapsed - 1000) as f64 / 200.0).sin();
        let pulsed_score_font = score_font.derive_font((score_font.size as f32) * pulse_scale as f32);
        g.set_font(pulsed_score_font.clone());
        g.set_color(Color::new(255, 255, 255, 220));
        let score_text = current_goal.new_score.to_string();
        g.set_font(pulsed_score_font.clone());
        let score_metrics = g.get_font_metrics();
        let score_width = score_metrics.string_width(&score_text) as f64;
        g.draw_string(&score_text, goal_x + goal_widget_width - score_width - 20.0, goal_y + 45.0);
    }
}

pub fn draw_card_widget(
    current_card: &mut Option<CardState>,
    g: &mut Graphics2D,
    screen_width: f64,
    _screen_height: f64,
    card_widget_width: f64,
    card_widget_height: f64,
    bg_color: Color,
    border_color: Color,
    image_cache: &HashMap<String, ImageSurface>,
) {
    if let Some(card) = current_card {
        if !card.is_active {
            *current_card = None;
            return;
        }

        let card_x = screen_width - card_widget_width - 10.0;
        let card_y = 65.0;

        let elapsed = current_time_millis() - card.animation_start_time;
        if elapsed > 4000 {
            card.is_active = false;
            *current_card = None;
            return;
        } else if elapsed > 3000 {
            card.phase = 1;
        }

        let mut opacity = 1.0;
        if card.phase == 1 {
            opacity = 1.0 - ((elapsed - 3000) as f64 / 1000.0);
            opacity = opacity.max(0.0).min(1.0);
        }

        g.set_composite_alpha(opacity);

        g.set_color(bg_color);
        g.fill_round_rect(card_x, card_y, card_widget_width, card_widget_height, 10.0);
        g.set_color(border_color);
        g.set_stroke(BasicStroke::new(1.0));
        g.draw_round_rect(card_x, card_y, card_widget_width, card_widget_height, 10.0);

        let team_logo = image_cache.get(&card.team_name).and_then(|img| create_rounded_image(img, 25));

        let player_font = Font::new("Arial", FontStyle::Bold, 14);
        let reason_font = Font::new("Arial", FontStyle::Plain, 12);
        g.set_font(player_font.clone());

        let content_y = card_y + card_widget_height / 2.0 + 5.0;
        let mut current_pos_x = card_x + 10.0;

        if let Some(tl) = &team_logo {
            g.draw_image(tl, current_pos_x, card_y + 7.0);
            current_pos_x += 35.0;
        }

        draw_card(g, current_pos_x, card_y + 10.0, card.card_type);
        current_pos_x += 25.0;

        g.set_color(Color::WHITE);
        g.set_font(player_font.clone());
        g.draw_string(&card.player_name, current_pos_x, content_y);

        if let Some(reason) = &card.reason {
            if !reason.is_empty() {
                g.set_font(reason_font.clone());
                g.set_color(Color::GREEN);
                g.set_font(reason_font.clone());
                let fm = g.get_font_metrics();
                let reason_y = content_y + fm.get_height() as f64;
                g.draw_string(reason, current_pos_x, reason_y);
            }
        }

        g.set_composite_alpha(1.0);
    }
}

pub fn draw_card(g: &mut Graphics2D, x: f64, y: f64, card_type: CardType) {
    let card_width = 15.0;
    let card_height = 20.0;

    let card_color = if card_type == CardType::YellowCard {
        Color::new(255, 215, 0, 255)
    } else {
        Color::new(220, 20, 20, 255)
    };

    g.set_color(card_color);
    g.fill_round_rect(x, y, card_width, card_height, 3.0);

    g.set_color(Color::BLACK);
    g.set_stroke(BasicStroke::new(1.0));
    g.draw_round_rect(x, y, card_width, card_height, 3.0);

    g.set_color(Color::new(0, 0, 0, 50));
    g.fill_round_rect(x + 1.0, y + 1.0, card_width, card_height, 3.0);

    g.set_color(card_color);
    g.fill_round_rect(x, y, card_width, card_height, 3.0);
    g.set_color(Color::BLACK);
    g.draw_round_rect(x, y, card_width, card_height, 3.0);
}

pub fn draw_substitution_widget(
    current_substitution: &mut Option<SubstitutionState>,
    g: &mut Graphics2D,
    screen_width: f64,
    _screen_height: f64,
    sub_widget_width: f64,
    sub_widget_height: f64,
    bg_color: Color,
    border_color: Color,
    image_cache: &HashMap<String, ImageSurface>,
) {
    if let Some(sub) = current_substitution {
        if !sub.is_active {
            *current_substitution = None;
            return;
        }

        let sub_x = screen_width - sub_widget_width - 10.0;
        let sub_y = 65.0;

        let elapsed = current_time_millis() - sub.animation_start_time;
        if elapsed > 6000 {
            sub.is_active = false;
            *current_substitution = None;
            return;
        } else if elapsed > 4000 {
            sub.phase = 2;
        } else if elapsed > 2000 {
            sub.phase = 1;
        }

        g.set_color(bg_color);
        g.fill_round_rect(sub_x, sub_y, sub_widget_width, sub_widget_height, 10.0);
        g.set_color(border_color);
        g.set_stroke(BasicStroke::new(1.0));
        g.draw_round_rect(sub_x, sub_y, sub_widget_width, sub_widget_height, 10.0);

        let team_logo = image_cache.get(&sub.team_name).and_then(|img| create_rounded_image(img, 25));

        let player_font = Font::new("Arial", FontStyle::Bold, 14);
        g.set_font(player_font.clone());

        let content_y = sub_y + sub_widget_height / 2.0 + 5.0;
        let mut current_pos_x = sub_x + 10.0;

        if let Some(tl) = &team_logo {
            g.draw_image(tl, current_pos_x, sub_y + 7.0);
            current_pos_x += 30.0;
        }

        if sub.phase == 0 {
            g.set_color(Color::RED);
            g.draw_string(&sub.player_out, current_pos_x, content_y);
            g.set_font(player_font.clone());
            let fm = g.get_font_metrics();
            current_pos_x += fm.string_width(&sub.player_out) as f64 + 10.0;
            draw_arrow(g, current_pos_x, sub_y + 15.0, true, Color::RED);
        } else if sub.phase == 1 {
            g.set_color(Color::RED);
            g.draw_string(&sub.player_out, current_pos_x, content_y - 5.0);
            g.set_font(player_font.clone());
            let fm = g.get_font_metrics();
            draw_arrow(g, current_pos_x + fm.string_width(&sub.player_out) as f64 + 5.0, sub_y + 10.0, true, Color::RED);

            g.set_color(Color::GREEN);
            g.draw_string(&sub.player_in, current_pos_x, content_y + 10.0);
            g.set_font(player_font.clone());
            let fm = g.get_font_metrics();
            draw_arrow(g, current_pos_x + fm.string_width(&sub.player_in) as f64 + 5.0, sub_y + 25.0, false, Color::GREEN);
        } else {
            g.set_color(Color::GREEN);
            g.draw_string(&sub.player_in, current_pos_x, content_y);
            g.set_font(player_font.clone());
            let fm = g.get_font_metrics();
            current_pos_x += fm.string_width(&sub.player_in) as f64 + 10.0;
            draw_arrow(g, current_pos_x, sub_y + 15.0, false, Color::GREEN);
        }
    }
}

pub fn draw_arrow(g: &mut Graphics2D, x: f64, y: f64, up: bool, color: Color) {
    g.set_color(color);
    let (x_points, y_points) = if up {
        ([x, x + 5.0, x + 10.0], [y + 10.0, y, y + 10.0])
    } else {
        ([x, x + 5.0, x + 10.0], [y, y + 10.0, y])
    };
    g.fill_polygon(&x_points, &y_points, 3);
}

pub fn draw_platform_logo(_g: &mut Graphics2D, _screen_width: f64, _screen_height: f64) {
    // Implementation depends on external asset loading
}

pub fn calculate_period(minutes: i64) -> &'static str {
    if minutes < 45 {
        "1st"
    } else if minutes < 46 {
        "HT"
    } else if minutes < 90 {
        "2nd"
    } else if minutes < 91 {
        "FT"
    } else if minutes < 105 {
        "ET1"
    } else if minutes < 106 {
        "ET HT"
    } else if minutes < 120 {
        "ET2"
    } else {
        "PEN"
    }
}

// =====================================================================
// Team Lineup & Formations
// =====================================================================

pub fn draw_team_lineup(
    g: &mut Graphics2D,
    players: &[HashMap<String, String>],
    formation: &str,
    jersey_color: u32,
    screen_width: f64,
    screen_height: f64,
    is_home_team: bool,
) {
    if players.is_empty() {
        return;
    }

    let field_width = 800.0;//supposed to scale based on the canvas width
    let field_height = 500.0;//supposed to scale based on the canvas height
    let player_size = 40.0;
    let jersey_size = 35.0;
    let font_size = 12.0;
    let field_color = Color::new(34, 139, 34, 180);
    let line_color = Color::new(255, 255, 255, 200);

    let field_x = (screen_width - field_width) / 2.0;
    let field_y = (screen_height - field_height) / 2.0;

    draw_football_field(g, field_x, field_y, field_width, field_height, field_color, line_color);

    let team_jersey_color = parse_jersey_color(jersey_color);

    let sorted_and_assigned_players =
        sort_and_assign_players_to_formation(players, formation, field_width as i32, field_height as i32, is_home_team);

    for assignment in sorted_and_assigned_players {
        let player_x = field_x + assignment.assigned_position.x as f64 - player_size / 2.0;
        let player_y = field_y + assignment.assigned_position.y as f64 - player_size / 2.0;

        // Dim positions the assignment algorithm had to guess at
        // (`exact_match: false`, lower `score` — see assign_by_role_group
        // and friends in the multi-pass assignment chain) so a
        // low-confidence placement reads as slightly less certain on
        // screen instead of looking identical to a clean number/position
        // match.
        let confidence_alpha = if assignment.exact_match {
            1.0
        } else {
            (0.55 + 0.45 * (assignment.score as f64 / 100.0)).clamp(0.55, 1.0)
        };
        g.set_composite_alpha(confidence_alpha);
        draw_player(g, &assignment.player, player_x, player_y, player_size, jersey_size, team_jersey_color, font_size);
        g.set_composite_alpha(1.0);
    }

    draw_team_header(g, players, field_x, field_y - 40.0, field_width, team_jersey_color);
}

fn sort_and_assign_players_to_formation(
    players: &[HashMap<String, String>],
    formation: &str,
    field_width: i32,
    field_height: i32,
    is_home_team: bool,
) -> Vec<PlayerAssignment> {
    if players.is_empty() {
        return Vec::new();
    }

    let mut sorted_players = sort_players_by_number(players.to_vec());
    let base_positions = get_formation_positions(formation, field_width, field_height, is_home_team);
    let mut available_positions = base_positions;

    if !validate_formation_player_count(sorted_players.len(), available_positions.len()) {
        eprintln!("Warning: Player count doesn't match formation positions");
    }

    let assignments = perform_multi_pass_assignment(&mut sorted_players, &mut available_positions);
    validate_final_assignments(&assignments, formation);
    assignments
}

fn perform_multi_pass_assignment(
    players: &mut Vec<HashMap<String, String>>,
    positions: &mut Vec<FormationPosition>,
) -> Vec<PlayerAssignment> {
    let mut assignments = Vec::new();
    assignments.extend(assign_exact_matches(players, positions));
    assignments.extend(assign_by_role_group(players, positions));
    assignments.extend(assign_by_jersey_number(players, positions));
    assignments.extend(assign_remaining(players, positions));
    assignments
}

fn assign_exact_matches(
    players: &mut Vec<HashMap<String, String>>,
    positions: &mut Vec<FormationPosition>,
) -> Vec<PlayerAssignment> {
    let mut assignments = Vec::new();
    let mut i = 0;
    while i < players.len() {
        let player_pos = normalize_position(&get_player_position(&players[i]));
        let mut found = false;
        for j in 0..positions.len() {
            if positions[j].position == player_pos {
                let pos = positions.remove(j);
                assignments.push(PlayerAssignment { player: players[i].clone(), assigned_position: pos, exact_match: true, score: 100 });
                players.remove(i);
                found = true;
                break;
            }
        }
        if !found {
            i += 1;
        }
    }
    assignments
}

fn assign_by_role_group(
    players: &mut Vec<HashMap<String, String>>,
    positions: &mut Vec<FormationPosition>,
) -> Vec<PlayerAssignment> {
    let mut assignments = Vec::new();
    let mut i = 0;
    while i < players.len() {
        let player_pos = normalize_position(&get_player_position(&players[i]));
        let player_group = get_role_group(&player_pos);
        let mut best_match_idx = None;
        let mut best_score = 0;
        for (j, pos) in positions.iter().enumerate() {
            if pos.group == player_group {
                let score = calculate_position_score(&player_pos, &pos.position);
                if score > best_score {
                    best_score = score;
                    best_match_idx = Some(j);
                }
            }
        }
        if let Some(idx) = best_match_idx {
            let pos = positions.remove(idx);
            assignments.push(PlayerAssignment { player: players[i].clone(), assigned_position: pos, exact_match: false, score: best_score });
            players.remove(i);
        } else {
            i += 1;
        }
    }
    assignments
}

fn assign_by_jersey_number(
    players: &mut Vec<HashMap<String, String>>,
    positions: &mut Vec<FormationPosition>,
) -> Vec<PlayerAssignment> {
    let mut assignments = Vec::new();
    let mut i = 0;
    while i < players.len() {
        let player_number = get_player_number(&players[i]);
        if let Some(idx) = suggest_position_by_number(player_number, positions) {
            let pos = positions.remove(idx);
            assignments.push(PlayerAssignment { player: players[i].clone(), assigned_position: pos, exact_match: false, score: 50 });
            players.remove(i);
        } else {
            i += 1;
        }
    }
    assignments
}

fn assign_remaining(
    players: &mut Vec<HashMap<String, String>>,
    positions: &mut Vec<FormationPosition>,
) -> Vec<PlayerAssignment> {
    let mut assignments = Vec::new();
    let count = players.len().min(positions.len());
    for _ in 0..count {
        assignments.push(PlayerAssignment { player: players.remove(0), assigned_position: positions.remove(0), exact_match: false, score: 25 });
    }
    assignments
}

fn sort_players_by_number(mut players: Vec<HashMap<String, String>>) -> Vec<HashMap<String, String>> {
    players.sort_by_key(|p| get_player_number(p));
    players
}

fn normalize_position(position: &str) -> String {
    match position.to_uppercase().as_str() {
        "LW" => "LM".to_string(),
        "RW" => "RM".to_string(),
        "DM" | "CDM" | "CAM" => "CM".to_string(),
        "LWB" => "LB".to_string(),
        "RWB" => "RB".to_string(),
        other => other.to_string(),
    }
}

fn get_role_group(position: &str) -> String {
    let norm = normalize_position(position);
    POSITION_GROUPS.get(norm.as_str()).unwrap_or(&"MID").to_string()
}

fn calculate_position_score(player_pos: &str, formation_pos: &str) -> i32 {
    if player_pos == formation_pos {
        return 100;
    }
    let p_group = get_role_group(player_pos);
    let f_group = get_role_group(formation_pos);
    if p_group == f_group {
        return calculate_sub_compatibility(player_pos, formation_pos);
    }
    0
}

fn calculate_sub_compatibility(player_pos: &str, formation_pos: &str) -> i32 {
    let group = get_role_group(player_pos);
    if group == "DEF" {
        if (player_pos == "CB" && formation_pos == "CB")
            || (player_pos == "LB" && formation_pos == "LB")
            || (player_pos == "RB" && formation_pos == "RB")
        {
            return 90;
        }
        return 70;
    }
    if group == "MID" {
        if (player_pos == "CM" && formation_pos == "CM")
            || (player_pos == "LM" && formation_pos == "LM")
            || (player_pos == "RM" && formation_pos == "RM")
        {
            return 90;
        }
        return 75;
    }
    if group == "FWD" {
        return 85;
    }
    60
}

fn suggest_position_by_number(number: i32, positions: &[FormationPosition]) -> Option<usize> {
    for (i, pos) in positions.iter().enumerate() {
        if number <= 2 && pos.group == "GK" {
            return Some(i);
        }
        if number >= 3 && number <= 6 && pos.group == "DEF" {
            return Some(i);
        }
        if number >= 7 && number <= 8 && pos.group == "MID" {
            return Some(i);
        }
        if number >= 9 && number <= 11 && pos.group == "FWD" {
            return Some(i);
        }
    }
    if !positions.is_empty() {
        Some(0)
    } else {
        None
    }
}

fn validate_formation_player_count(player_count: usize, formation_positions: usize) -> bool {
    (player_count as i32 - formation_positions as i32).abs() <= 1
}

fn validate_final_assignments(assignments: &[PlayerAssignment], _formation: &str) {
    let mut used_positions = HashSet::new();
    for assignment in assignments {
        let pos_key = format!("{},{}", assignment.assigned_position.x, assignment.assigned_position.y);
        if !used_positions.insert(pos_key) {
            eprintln!("Warning: Duplicate position assignment detected");
        }
    }
    let mut role_count = HashMap::new();
    for assignment in assignments {
        let group = &assignment.assigned_position.group;
        *role_count.entry(group.clone()).or_insert(0) += 1;
    }
    if *role_count.get("GK").unwrap_or(&0) == 0 {
        eprintln!("Warning: No goalkeeper assigned");
    }
}

fn get_player_position(player: &HashMap<String, String>) -> String {
    player.get("position").cloned().unwrap_or_else(|| "CM".to_string())
}

fn get_player_number(player: &HashMap<String, String>) -> i32 {
    if let Some(num_str) = player.get("number") {
        num_str.parse::<i32>().unwrap_or(99)
    } else {
        99
    }
}

fn get_formation_positions(formation: &str, field_width: i32, field_height: i32, is_home_team: bool) -> Vec<FormationPosition> {
    let mut positions = Vec::new();
    let formation = if formation.is_empty() { "4-4-2" } else { formation };
    let percentage_positions = get_formation_percentages(formation);
    let position_labels = get_formation_position_labels(formation);

    for i in 0..percentage_positions.len().min(position_labels.len()) {
        let (x, y) = if is_home_team {
            ((percentage_positions[i][0] * field_width) / 100, (percentage_positions[i][1] * field_height) / 100)
        } else {
            (((100 - percentage_positions[i][0]) * field_width) / 100, (percentage_positions[i][1] * field_height) / 100)
        };
        positions.push(FormationPosition { position: position_labels[i].clone(), group: get_role_group(&position_labels[i]), x, y });
    }
    positions
}

pub fn draw_football_field(g: &mut Graphics2D, x: f64, y: f64, width: f64, height: f64, field_color: Color, line_color: Color) {
    g.set_color(field_color);
    g.fill_rect(x, y, width, height);
    g.set_color(line_color);
    g.set_stroke(BasicStroke::new(3.0));
    g.draw_rect(x, y, width, height);
    g.draw_line(x + width / 2.0, y, x + width / 2.0, y + height);

    let center_x = x + width / 2.0;
    let center_y = y + height / 2.0;
    let center_circle_radius = height / 8.0;
    g.draw_oval(center_x - center_circle_radius, center_y - center_circle_radius, center_circle_radius * 2.0, center_circle_radius * 2.0);

    let center_spot_radius = 4.0;
    g.fill_oval(center_x - center_spot_radius, center_y - center_spot_radius, center_spot_radius * 2.0, center_spot_radius * 2.0);

    let penalty_box_width = width / 7.0;
    let penalty_box_height = height * 0.55;
    g.draw_rect(x, center_y - penalty_box_height / 2.0, penalty_box_width, penalty_box_height);
    g.draw_rect(x + width - penalty_box_width, center_y - penalty_box_height / 2.0, penalty_box_width, penalty_box_height);

    let goal_box_width = width / 20.0;
    let goal_box_height = height * 0.25;
    g.draw_rect(x, center_y - goal_box_height / 2.0, goal_box_width, goal_box_height);
    g.draw_rect(x + width - goal_box_width, center_y - goal_box_height / 2.0, goal_box_width, goal_box_height);

    let penalty_spot_radius = 4.0;
    let penalty_spot_distance = penalty_box_width * 0.65;
    g.fill_oval(x + penalty_spot_distance - penalty_spot_radius, center_y - penalty_spot_radius, penalty_spot_radius * 2.0, penalty_spot_radius * 2.0);
    g.fill_oval(x + width - penalty_spot_distance - penalty_spot_radius, center_y - penalty_spot_radius, penalty_spot_radius * 2.0, penalty_spot_radius * 2.0);

    let arc_radius = center_circle_radius * 2.0;
    let arc_center_distance = (penalty_box_width * 0.3) - 12.0;
    g.draw_arc(x + arc_center_distance - arc_radius, center_y - arc_radius, arc_radius * 2.0, arc_radius * 2.0, 320.0, 80.0);
    g.draw_arc(x + width - arc_center_distance - arc_radius, center_y - arc_radius, arc_radius * 2.0, arc_radius * 2.0, 140.0, 80.0);

    let corner_arc_radius = 15.0;
    g.draw_arc(x - corner_arc_radius, y - corner_arc_radius, corner_arc_radius * 2.0, corner_arc_radius * 2.0, 270.0, 90.0);
    g.draw_arc(x + width - corner_arc_radius, y - corner_arc_radius, corner_arc_radius * 2.0, corner_arc_radius * 2.0, 180.0, 90.0);
    g.draw_arc(x - corner_arc_radius, y + height - corner_arc_radius, corner_arc_radius * 2.0, corner_arc_radius * 2.0, 0.0, 90.0);
    g.draw_arc(x + width - corner_arc_radius, y + height - corner_arc_radius, corner_arc_radius * 2.0, corner_arc_radius * 2.0, 90.0, 90.0);
}

fn get_formation_percentages(formation: &str) -> Vec<[i32; 2]> {
    match formation {
        "4-4-2" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [50, 20], [50, 38], [50, 62], [50, 80], [75, 35], [75, 65]],
        "4-3-3" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [40, 50], [55, 30], [55, 70], [75, 25], [75, 50], [75, 75]],
        "4-2-3-1" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [38, 40], [38, 60], [62, 25], [62, 50], [62, 75], [78, 50]],
        "3-5-2" => vec![[8, 50], [25, 25], [25, 50], [25, 75], [50, 15], [50, 32], [50, 50], [50, 68], [50, 85], [75, 35], [75, 65]],
        "4-1-4-1" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [38, 50], [55, 20], [55, 40], [55, 60], [55, 80], [78, 50]],
        "5-3-2" => vec![[8, 50], [25, 12], [25, 28], [25, 50], [25, 72], [25, 88], [50, 35], [50, 50], [50, 65], [75, 35], [75, 65]],
        "3-4-3" => vec![[8, 50], [25, 25], [25, 50], [25, 75], [45, 40], [45, 60], [55, 20], [55, 80], [75, 25], [75, 50], [75, 75]],
        "4-4-1-1" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [45, 20], [45, 38], [45, 62], [45, 80], [65, 50], [80, 50]],
        "4-5-1" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [50, 15], [50, 30], [50, 50], [50, 70], [50, 85], [75, 50]],
        "3-5-1-1" => vec![[8, 50], [25, 25], [25, 50], [25, 75], [45, 12], [45, 35], [45, 50], [45, 65], [45, 88], [65, 50], [80, 50]],
        "4-2-1-2" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [40, 35], [40, 65], [60, 50], [75, 35], [75, 65], [75, 50]],
        "5-4-1" => vec![[8, 50], [25, 12], [25, 28], [25, 50], [25, 72], [25, 88], [52, 25], [52, 42], [52, 58], [52, 75], [75, 50]],
        "4-4-2-diamond" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [40, 50], [50, 35], [50, 65], [60, 50], [75, 35], [75, 65]],
        "4-3-2-1" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [42, 50], [50, 32], [50, 68], [65, 38], [65, 62], [78, 50]],
        "4-3-3-false9" => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [40, 50], [50, 35], [50, 65], [75, 25], [65, 50], [75, 75]],
        _ => vec![[8, 50], [25, 15], [25, 35], [25, 65], [25, 85], [50, 20], [50, 38], [50, 62], [50, 80], [75, 35], [75, 65]],
    }
}

fn get_formation_position_labels(formation: &str) -> Vec<String> {
    let labels = match formation {
        "4-4-2" => vec!["GK", "RB", "CB", "CB", "LB", "RM", "CM", "CM", "LM", "CF", "CF"],
        "4-3-3" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "CM", "CM", "RW", "CF", "LW"],
        "4-2-3-1" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "DM", "RM", "CM", "LM", "CF"],
        "3-5-2" => vec!["GK", "CB", "CB", "CB", "RM", "CM", "CM", "CM", "LM", "CF", "CF"],
        "4-1-4-1" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "RM", "CM", "CM", "LM", "CF"],
        "5-3-2" => vec!["GK", "RB", "CB", "CB", "CB", "LB", "CM", "CM", "CM", "CF", "CF"],
        "3-4-3" => vec!["GK", "CB", "CB", "CB", "CM", "CM", "RM", "LM", "RW", "CF", "LW"],
        "4-4-1-1" => vec!["GK", "RB", "CB", "CB", "LB", "RM", "CM", "CM", "LM", "CM", "CF"],
        "4-5-1" => vec!["GK", "RB", "CB", "CB", "LB", "RM", "CM", "CM", "CM", "LM", "CF"],
        "3-5-1-1" => vec!["GK", "CB", "CB", "CB", "RM", "CM", "CM", "CM", "LM", "CM", "CF"],
        "4-2-1-2" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "DM", "CM", "CF", "CF", "CF"],
        "5-4-1" => vec!["GK", "RB", "CB", "CB", "CB", "LB", "RM", "CM", "CM", "LM", "CF"],
        "4-4-2-diamond" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "RM", "LM", "CM", "CF", "CF"],
        "4-3-2-1" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "CM", "CM", "RM", "LM", "CF"],
        "4-3-3-false9" => vec!["GK", "RB", "CB", "CB", "LB", "DM", "CM", "CM", "RW", "CF", "LW"],
        _ => vec!["GK", "RB", "CB", "CB", "LB", "RM", "CM", "CM", "LM", "CF", "CF"],
    };
    labels.into_iter().map(String::from).collect()
}

pub fn draw_player(g: &mut Graphics2D, player: &HashMap<String, String>, x: f64, y: f64, player_size: f64, jersey_size: f64, jersey_color: Color, font_size: f64) {
    let player_name = player.get("name").map(|s| s.as_str()).unwrap_or("Unknown");
    let player_number = player.get("number").map(|s| s.as_str()).unwrap_or("0");
    let position = player.get("position").map(|s| s.as_str()).unwrap_or("");

    g.set_color(Color::new(255, 220, 177, 255));
    g.fill_oval(x + (player_size - 20.0) / 2.0, y, 20.0, 20.0);
    g.set_color(Color::BLACK);
    g.set_stroke(BasicStroke::new(1.0));
    g.draw_oval(x + (player_size - 20.0) / 2.0, y, 20.0, 20.0);

    draw_jersey(g, x + (player_size - jersey_size) / 2.0, y + 15.0, jersey_size, jersey_color, player_number);

    let name_font = Font::new("Arial", FontStyle::Bold, font_size as i32);
    g.set_font(name_font.clone());
    g.set_color(Color::WHITE);
    let name_metrics = g.get_font_metrics();

    let display_name = truncate_name(player_name, &name_metrics, (player_size + 20.0) as i32);
    let name_width = name_metrics.string_width(&display_name) as f64;

    g.set_color(Color::new(0, 0, 0, 150));
    g.fill_round_rect(x + (player_size - name_width) / 2.0 - 3.0, y + player_size + 2.0, name_width + 6.0, font_size + 4.0, 3.0);

    g.set_color(Color::WHITE);
    g.draw_string(&display_name, x + (player_size - name_width) / 2.0, y + player_size + font_size + 2.0);

    if !position.is_empty() {
        let pos_font = Font::new("Arial", FontStyle::Plain, (font_size - 2.0) as i32);
        g.set_font(pos_font.clone());
        g.set_color(Color::new(200, 200, 200, 255));
        let pos_metrics = g.get_font_metrics();
        let pos_width = pos_metrics.string_width(position) as f64;
        g.draw_string(position, x + (player_size - pos_width) / 2.0, y + player_size + font_size + 15.0);
    }
}

pub fn draw_jersey(g: &mut Graphics2D, x: f64, y: f64, size: f64, jersey_color: Color, number: &str) {
    g.set_color(jersey_color);
    g.fill_round_rect(x, y, size, size - 5.0, 8.0);
    g.set_color(Color::WHITE);
    g.set_stroke(BasicStroke::new(2.0));
    g.draw_round_rect(x, y, size, size - 5.0, 8.0);

    g.set_color(jersey_color);
    g.fill_oval(x - 5.0, y + 3.0, 12.0, 18.0);
    g.fill_oval(x + size - 7.0, y + 3.0, 12.0, 18.0);
    g.set_color(Color::WHITE);
    g.set_stroke(BasicStroke::new(1.0));
    g.draw_oval(x - 5.0, y + 3.0, 12.0, 18.0);
    g.draw_oval(x + size - 7.0, y + 3.0, 12.0, 18.0);

    let number_font = Font::new("Arial", FontStyle::Bold, (16.0_f64).min(size / 2.0) as i32);
    g.set_font(number_font.clone());
    g.set_color(Color::WHITE);
    let number_metrics = g.get_font_metrics();
    let number_width = number_metrics.string_width(number) as f64;
    let number_height = number_metrics.get_height() as f64;

    g.set_color(Color::new(0, 0, 0, 100));
    g.fill_oval(x + (size - number_width) / 2.0 - 2.0, y + (size - number_height) / 2.0 - 1.0, number_width + 4.0, number_height);

    g.set_color(Color::WHITE);
    g.draw_string(number, x + (size - number_width) / 2.0, y + (size - number_height) / 2.0 + number_metrics.get_ascent() as f64 - 2.0);
}

pub fn parse_jersey_color(argb_value: u32) -> Color {
    if argb_value == 0 {
        return Color::new(0, 100, 200, 255);
    }
    Color::from_argb(argb_value)
}

pub fn draw_team_header(g: &mut Graphics2D, players: &[HashMap<String, String>], x: f64, y: f64, width: f64, team_color: Color) {
    if players.is_empty() {
        return;
    }
    let team_name = players[0].get("team").map(|s| s.as_str()).unwrap_or("Team Lineup");
    let header_font = Font::new("Arial", FontStyle::Bold, 24);
    g.set_font(header_font.clone());
    let header_metrics = g.get_font_metrics();
    let header_width = header_metrics.string_width(team_name) as f64;

    g.set_color(Color::new(team_color.r as u8, team_color.g as u8, team_color.b as u8, 200));
    g.fill_round_rect(x + (width - header_width) / 2.0 - 15.0, y - 5.0, header_width + 30.0, 30.0, 10.0);
    g.set_color(Color::WHITE);
    g.set_stroke(BasicStroke::new(2.0));
    g.draw_round_rect(x + (width - header_width) / 2.0 - 15.0, y - 5.0, header_width + 30.0, 30.0, 10.0);
    g.set_color(Color::WHITE);
    g.draw_string(team_name, x + (width - header_width) / 2.0, y + 18.0);
}

pub fn draw_player_position(g: &mut Graphics2D, player: &SubstitutePlayer, x: f64, y: f64, position_font: &Font) {
    g.set_font(position_font.clone());
    let position_color = if player.is_goalkeeper {
        Color::new(255, 165, 0, 255)
    } else if player.position.contains("DEF") || player.position.contains("CB") || player.position.contains("LB") || player.position.contains("RB") {
        Color::new(0, 191, 255, 255)
    } else if player.position.contains("MID") || player.position.contains("CM") || player.position.contains("CAM") || player.position.contains("CDM") {
        Color::new(50, 205, 50, 255)
    } else {
        Color::new(255, 69, 0, 255)
    };

    g.set_font(position_font.clone());
    let pos_metrics = g.get_font_metrics();
    let pos_width = pos_metrics.string_width(&player.position) as f64;

    g.set_color(Color::new(position_color.r as u8, position_color.g as u8, position_color.b as u8, 100));
    g.fill_round_rect(x - pos_width / 2.0 - 3.0, y - 8.0, pos_width + 6.0, 12.0, 6.0);

    g.set_color(position_color);
    g.draw_string(&player.position, x - pos_width / 2.0, y + 2.0);
}

pub fn draw_substitutes_table_header(g: &mut Graphics2D, sub_table: &SubstitutionTable, x: f64, y: f64, width: f64, height: f64, team_color: Color, image_cache: &HashMap<String, ImageSurface>) {
    let header_gradient = GradientPaint {
        x1: x,
        y1: y,
        c1: team_color,
        x2: x,
        y2: y + height,
        c2: Color::new((team_color.r * 127.5) as u8, (team_color.g * 127.5) as u8, (team_color.b * 127.5) as u8, 255),
    };
    g.set_paint(header_gradient);
    g.fill_round_rect(x + 5.0, y + 5.0, width - 10.0, height - 5.0, 10.0);

    g.set_stroke(BasicStroke::new(1.0));
    g.set_color(Color::new(255, 255, 255, 150));
    g.draw_round_rect(x + 5.0, y + 5.0, width - 10.0, height - 5.0, 10.0);

    let mut text_x = x + 15.0;
    if let Some(team_logo) = image_cache.get(&sub_table.team_name) {
        if let Some(resized_logo) = create_rounded_image(team_logo, 30) {
            g.draw_image(&resized_logo, x + 10.0, y + 10.0);
            text_x = x + 50.0;
        }
    }

    let header_font = Font::new("Arial", FontStyle::Bold, 16);
    g.set_font(header_font.clone());
    g.set_color(Color::WHITE);
    let header_text = "SUBSTITUTES";
    g.set_font(header_font.clone());
    let header_metrics = g.get_font_metrics();
    let text_y = y + (height + header_metrics.get_ascent() as f64) / 2.0 - 2.0;
    g.draw_string(header_text, text_x, text_y);

    let team_font = Font::new("Arial", FontStyle::Bold, 12);
    g.set_font(team_font.clone());
    g.set_color(Color::new(255, 255, 255, 200));
    let team_text_y = text_y + 15.0;
    g.draw_string(&sub_table.team_name, text_x, team_text_y);

    draw_column_headers(g, x, y + height - 15.0, width);
}

pub fn draw_column_headers(g: &mut Graphics2D, x: f64, y: f64, width: f64) {
    let column_font = Font::new("Arial", FontStyle::Bold, 10);
    g.set_font(column_font.clone());
    g.set_color(Color::new(200, 200, 200, 255));
    let pos_col = x + width - 40.0;
    g.draw_string("POS", pos_col, y);
}

pub fn draw_substitutes_table_rows(g: &mut Graphics2D, sub_table: &SubstitutionTable, x: f64, y: f64, width: f64, row_height: f64, team_color: Color) {
    let player_font = Font::new("Arial", FontStyle::Plain, 12);
    let number_font = Font::new("Arial", FontStyle::Bold, 14);
    let position_font = Font::new("Arial", FontStyle::Bold, 10);

    for (i, player) in sub_table.substitutes.iter().enumerate() {
        let row_y = y + (i as f64 * row_height);
        let row_bg = if i % 2 == 0 { Color::new(255, 255, 255, 20) } else { Color::new(255, 255, 255, 10) };

        g.set_color(row_bg);
        g.fill_rect(x + 5.0, row_y, width - 10.0, row_height);

        if i > 0 {
            g.set_color(Color::new(255, 255, 255, 30));
            g.draw_line(x + 10.0, row_y, x + width - 10.0, row_y);
        }

        draw_jersey_number(g, player, x + 15.0, row_y + row_height / 2.0, team_color, &number_font);

        g.set_font(player_font.clone());
        g.set_color(Color::WHITE);
        g.set_font(player_font.clone());
        let metrics = g.get_font_metrics();
        let display_name = truncate_name(&player.name, &metrics, 120);
        g.draw_string(&display_name, x + 50.0, row_y + row_height / 2.0 + 4.0);

        draw_player_position(g, player, x + width - 35.0, row_y + row_height / 2.0 + 3.0, &position_font);
    }
}

pub fn draw_jersey_number(g: &mut Graphics2D, player: &SubstitutePlayer, x: f64, y: f64, team_color: Color, number_font: &Font) {
    let jersey_bg = if player.is_goalkeeper {
        Color::new(255, 165, 0, 180)
    } else {
        Color::new(team_color.r as u8, team_color.g as u8, team_color.b as u8, 180)
    };

    g.set_color(jersey_bg);
    g.fill_oval(x - 12.0, y - 10.0, 24.0, 20.0);
    g.set_stroke(BasicStroke::new(1.0));
    g.set_color(Color::WHITE);
    g.draw_oval(x - 12.0, y - 10.0, 24.0, 20.0);

    g.set_font(number_font.clone());
    g.set_color(Color::WHITE);
    let number_str = player.jersey_number.to_string();
    g.set_font(number_font.clone());
    let number_metrics = g.get_font_metrics();
    let number_width = number_metrics.string_width(&number_str) as f64;
    g.draw_string(&number_str, x - number_width / 2.0, y + 4.0);
}

pub fn draw_substitutes_table_widget(sub_table: &mut SubstitutionTable, g: &mut Graphics2D, screen_width: f64, screen_height: f64, image_cache: &HashMap<String, ImageSurface>) {
    if !sub_table.is_active || sub_table.substitutes.is_empty() {
        return;
    }
    let elapsed = current_time_millis() - sub_table.animation_start_time;

    if elapsed > 9000 {
        sub_table.is_active = false;
        return;
    }

    if elapsed <= 1000 {
        sub_table.phase = 0;
    } else if elapsed <= 8000 {
        sub_table.phase = 1;
    } else {
        sub_table.phase = 2;
    }

    let table_width = 300.0;
    let header_height = 45.0;
    let row_height = 35.0;
    let table_height = header_height + (sub_table.substitutes.len() as f64 * row_height) + 10.0;

    let table_x = (screen_width - table_width) / 2.0;
    let table_y = (screen_height - table_height) / 2.0;

    let mut opacity = 1.0;
    if sub_table.phase == 0 {
        opacity = elapsed as f64 / 1000.0;
    } else if sub_table.phase == 2 {
        opacity = 1.0 - ((elapsed - 8000) as f64 / 1000.0);
    }

    let alpha = (255.0 * opacity) as u8;
    let team_accent_color = parse_jersey_color(sub_table.team_color);

    let gradient = GradientPaint {
        x1: table_x,
        y1: table_y,
        c1: Color::new(0, 0, 0, (230.0 * opacity) as u8),
        x2: table_x,
        y2: table_y + table_height,
        c2: Color::new(20, 20, 20, (230.0 * opacity) as u8),
    };
    g.set_paint(gradient);
    g.fill_round_rect(table_x, table_y, table_width, table_height, 15.0);

    g.set_stroke(BasicStroke::new(2.0));
    g.set_color(Color::new(255, 255, 255, (180.0 * opacity) as u8));
    g.draw_round_rect(table_x, table_y, table_width, table_height, 15.0);

    g.set_stroke(BasicStroke::new(3.0));
    g.set_color(Color::new(team_accent_color.r as u8, team_accent_color.g as u8, team_accent_color.b as u8, alpha));
    g.draw_round_rect(table_x + 2.0, table_y + 2.0, table_width - 4.0, table_height - 4.0, 12.0);

    draw_substitutes_table_header(g, sub_table, table_x, table_y, table_width, header_height, team_accent_color, image_cache);
    draw_substitutes_table_rows(g, sub_table, table_x, table_y + header_height, table_width, row_height, team_accent_color);
}

//add 3d graphics instead of the lineup being a 2d football field make it 3d same as what supersport does and animations like when changing the scores you can add bounce where the score changes when with a a bounce and enlarge.
//add spin animations to the platform logo.
//add league logo to top left and also when there is a transition after every every event ie substituion, goal, or match perfomance widget.
// on every match performance widget add the scores, team logos, league logo, and time and the scorers with the time(minute) they scored. remove the card style layout use a single widget style stitched together.
// do this to all the widgets make them float. do not use cards ie ,
//|[league logo] [time][team logo][0][vs][0][team logo]           [ads logo]
//|
//|
//|            //goal
//|       [team logo][0][vs][team logo]
//|               [player name]  
//|
//|
//|
//|
//|
//|[platform logo]                           [payment banners for each match if other match is true]    

