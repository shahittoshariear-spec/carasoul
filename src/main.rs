//! carasoul — hold Shift on the desktop, glide through your wallpapers with the
//! arrow keys (or the mouse), Enter or click to apply. It lives in the
//! notification area and never shows a window of its own.
//!
//! Linked as a GUI app so that starting with Windows shows nothing at all; the
//! command-line modes borrow the console they were launched from (see
//! `sys::attach_parent_console`).
#![windows_subsystem = "windows"]

mod canvas;
mod library;
mod sys;
mod tray;

use canvas::{corner_inset, Canvas, Color, Xform};
use library::Library;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long Shift must be held before the strip appears. Long enough that
/// typing a capital letter never triggers it.
const HOLD_MS: u64 = 165;
/// Length of the "pour" animation after picking a wallpaper: the shelf gathers
/// into a stream and runs off the bottom of the band.
const COMMIT_SECS: f32 = 0.50;
/// Where in the commit the shelf starts fading out, so the pour has the stage to
/// itself before the whole thing disappears.
const COMMIT_FADE: f32 = 0.62;
/// Cards further than this from the focus are culled.
const CULL: f32 = 3.0;
/// How long a shelf opened from the tray ignores clicks, so the click that opened
/// it cannot immediately close it again.
const CLICK_SETTLE_MS: u64 = 250;

// Milestones of the two liquid morphs, as `(from, to)` pairs for `smoothstep`.
// The silhouette maths and the paint both read these, which is what keeps the
// shape and its colouring in step.
/// Entrance: how far up the front has come (`vis`).
const RISE_FRONT: (f32, f32) = (0.0, 0.90);
/// Entrance: where the dome settles into the panel's own outline.
const RISE_SET: (f32, f32) = (0.60, 0.98);
/// Entrance: where the liquid's colour gives way to the wallpaper wash.
const RISE_WASH: (f32, f32) = (0.88, 1.00);
/// Pour: where the panel gathers into the falling stream (`commit`).
const POUR_SET: (f32, f32) = (0.0, 0.72);
/// Pour: where the wash gives way to the liquid's colour.
const POUR_WASH: (f32, f32) = (0.0, 0.55);

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Strip {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    card_h: f32,
    cy_screen: f32,
}

#[derive(Clone, Copy)]
struct Layout {
    cx: f32,
    cy: f32,
    card_w: f32,
    card_h: f32,
    stride: f32,
    skew: f32,
    radius: f32,
}

fn card_size(mon_h: i32) -> f32 {
    (mon_h as f32 * 0.19).round().max(90.0)
}

/// Thumbnails are decoded to at least the size the focused card is drawn at, so
/// nothing is ever upscaled (which is what made them look soft).
fn thumb_size_for(mon_h: i32) -> (u32, u32) {
    let card_h = card_size(mon_h);
    let want = (card_h * 1.6 * 1.40).round() as u32;
    let w = ((want + 7) / 8 * 8).clamp(library::MIN_THUMB_W, library::MAX_THUMB_W);
    (w, w * 5 / 8)
}

fn strip_for(mon: sys::ScreenRect) -> Strip {
    let card_h = card_size(mon.h);
    // Kept as tight as the shelf allows: every pixel here is redrawn each frame.
    let h = (card_h * 2.25).round() as i32;
    let cy_screen = mon.y as f32 + mon.h as f32 * 0.58;
    let y = (cy_screen - h as f32 * 0.5).round() as i32;
    Strip {
        x: mon.x,
        y,
        w: mon.w,
        h,
        card_h,
        cy_screen,
    }
}

fn make_layout(strip: &Strip) -> Layout {
    let card_h = strip.card_h;
    Layout {
        cx: strip.w as f32 * 0.5,
        cy: strip.cy_screen - strip.y as f32,
        card_w: card_h * 1.6,
        card_h,
        stride: card_h * 1.60 * 0.80,
        skew: card_h * 0.20,
        radius: card_h * 0.055,
    }
}

// ---------------------------------------------------------------------------
// the liquid silhouette
// ---------------------------------------------------------------------------

/// The settled shelf: a full-width rounded rect. Both liquid morphs are defined
/// against it, so the last frame of the entrance — and the first frame of the
/// pour — land exactly on the shape `draw_shelf` paints.
#[derive(Clone, Copy)]
struct Shelf {
    cx: f32,
    cy: f32,
    h: f32,
    hw: f32,
    radius: f32,
}

impl Shelf {
    /// Half-width of the settled panel at canvas row `y`.
    fn hw_at(self, y: f32) -> f32 {
        let yl = (y - (self.cy - self.h * 0.5)).clamp(0.0, self.h);
        self.hw - corner_inset(yl, self.h, self.radius)
    }
}

/// Which way the shelf's liquid is going: welling up out of the bottom of the
/// band, or gathering into a stream and running off it.
#[derive(Clone, Copy)]
enum Morph {
    Rise(f32),
    Pour(f32),
}

/// The shelf's liquid silhouette. On the way in it is a body welling up out of
/// the bottom of the band; on the way out it is the same body gathered into a
/// neck with a stream running off the bottom of the band behind a bulge.
#[derive(Clone, Copy)]
struct Liquid {
    shelf: Shelf,
    band_h: f32,
    morph: Morph,
}

impl Liquid {
    /// Vertical extent of the body. Rows outside it are empty, which is what
    /// stops the settled panel's outline leaking in above a still-rising front.
    fn extents(self) -> (f32, f32) {
        let s = self.shelf;
        let floor = self.band_h + s.h * 0.06;
        let (top0, bot0) = (s.cy - s.h * 0.5, s.cy + s.h * 0.5);
        match self.morph {
            Morph::Pour(p) => {
                let q = smoothstep(POUR_SET.0, POUR_SET.1, p);
                // The top sinks away under the middle, so the body drains from above
                // as well as pouring below, and the tail reaches past the bottom of
                // the band — which is what reads as falling off the shelf rather
                // than shrinking inside it.
                let drain = smoothstep(0.0, 0.88, p);
                let dry_top = s.cy + s.h * 0.42;
                (top0 + drain * (dry_top - top0), bot0 + q * (floor - bot0))
            }
            Morph::Rise(p) => {
                let e = smoothstep(RISE_FRONT.0, RISE_FRONT.1, p);
                let yt = top0 + (1.0 - e) * (floor - top0);
                // The tail lifts off the floor late, so the body detaches from the
                // bottom of the band only once it is nearly the panel.
                let yb = bot0 + (1.0 - smoothstep(0.30, RISE_FRONT.1, p)) * (floor - bot0);
                (yt, yb)
            }
        }
    }

    /// Where the rising surface is, in canvas rows. Cards ride it.
    fn front(self) -> f32 {
        self.extents().0
    }

    /// The body's centre x, half-width and depth at canvas row `y`. Depth runs 0
    /// at the leading edge (which is where the liquid catches the light) to 1 at
    /// the trailing one.
    fn row(self, y: f32) -> (f32, f32, f32) {
        let (yt, yb) = self.extents();
        if y < yt || y > yb {
            return (self.shelf.cx, 0.0, 0.0);
        }
        let t = ((y - yt) / (yb - yt).max(1e-3)).clamp(0.0, 1.0);
        let s = self.shelf;
        match self.morph {
            Morph::Pour(p) => {
                let q = smoothstep(POUR_SET.0, POUR_SET.1, p);
                // A head that outruns the tail: the bulge is what reads as a drop
                // of liquid falling rather than a column sliding down.
                let head = 0.20 + 1.10 * smoothstep(0.0, 0.85, p);
                let bulge = 1.0 + 0.80 * (-((t - head) / 0.14).powi(2)).exp();
                let stream = s.h * 0.16 * bulge * smoothstep(0.0, 0.18, t);
                (s.cx, stream * q + s.hw_at(y) * (1.0 - q), 1.0 - t)
            }
            Morph::Rise(p) => {
                let e = smoothstep(RISE_FRONT.0, RISE_FRONT.1, p);
                let q = smoothstep(RISE_SET.0, RISE_SET.1, p);
                // The front is a meniscus: full width a little way behind the
                // edge, and sharpening into the panel's own corners as the body
                // settles, so the outline is never far from the wash's shape.
                let cap = 0.035 + 0.55 * (1.0 - e);
                let dome = smoothstep(0.0, cap, t);
                // A ripple and a sway, both confined to the tapered edge and both
                // fading out with the rise. Past the meniscus the body is already
                // the full width of the band, so wobbling it there would only open
                // gaps along the screen edges — and settling exactly onto the panel
                // is what lets the app stop repainting.
                let edge = 1.0 - dome;
                let ripple = 1.0 + 0.05 * (1.0 - p) * (t * 7.5).sin() * edge;
                let sway = 0.05 * s.h * (1.0 - p) * (t * 4.5).sin() * edge;
                let body = s.hw * dome * ripple;
                (s.cx + sway, body * (1.0 - q) + s.hw_at(y) * q, t)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// colours
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Palette {
    accent: Color,
    hot: Color,
    panel_top: Color,
    panel_bottom: Color,
}

fn palette_for(accent: Color) -> Palette {
    let deep = accent.scale(0.22);
    Palette {
        accent,
        hot: accent.scale(1.20).mix(Color::rgb(1.0, 1.0, 1.0), 0.10),
        panel_top: Color::rgb(0.045, 0.045, 0.062).mix(deep, 0.60).with_a(0.66),
        panel_bottom: Color::rgb(0.018, 0.018, 0.028).mix(deep, 0.32).with_a(0.42),
    }
}

/// Colour of the liquid at depth `t` (0 at the edge that is leading, 1 at the one
/// that trails): the panel's own gradient, opaque, lit up just behind the leading
/// edge so the surface catches light like a meniscus.
fn liquid_color(pal: &Palette, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let body = pal
        .panel_top
        .with_a(1.0)
        .mix(pal.panel_bottom.with_a(1.0), t);
    body.mix(pal.hot, 0.55 * (1.0 - smoothstep(0.0, 0.13, t)))
}

// ---------------------------------------------------------------------------
// easing
// ---------------------------------------------------------------------------

fn approach_color(current: &mut Color, target: Color, rate: f32, dt: f32) {
    let k = 1.0 - (-dt * rate).exp();
    let (r, g, b) = (
        target.r - current.r,
        target.g - current.g,
        target.b - current.b,
    );
    current.r += r * k;
    current.g += g * k;
    current.b += b * k;
    current.a = target.a;
    if r.abs() + g.abs() + b.abs() < 0.002 {
        *current = target;
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Maps any integer carousel slot onto a real item index, negative slots included.
fn wrap_index(slot: i32, n: usize) -> usize {
    let n = n as i32;
    (((slot % n) + n) % n) as usize
}

/// The instance of `idx` closest to the current position, so moving from the last
/// wallpaper to the first rotates one card instead of unwinding the whole strip.
fn nearest_virtual(current: f32, idx: usize, n: usize) -> f32 {
    let base = idx as f32;
    let n = n as f32;
    base + ((current - base) / n).round() * n
}

/// Semi-implicit spring, slightly under-damped so the carousel overshoots a touch
/// and settles with some life in it rather than sliding flatly into place.
fn step_spring(pos: &mut f32, vel: &mut f32, target: f32, dt: f32, freq: f32, damping: f32) {
    let dt = dt.clamp(0.0, 0.033);
    let omega = freq * std::f32::consts::TAU;
    let accel = omega * omega * (target - *pos) - 2.0 * damping * omega * *vel;
    *vel += accel * dt;
    *pos += *vel * dt;
    if (target - *pos).abs() < 0.0012 && vel.abs() < 0.02 {
        *pos = target;
        *vel = 0.0;
    }
}

// ---------------------------------------------------------------------------
// app
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Card {
    idx: usize,
    xf: Xform,
    alpha: f32,
    focus: f32,
}

struct App {
    overlay: sys::Overlay,
    input: sys::Input,
    strip: Strip,
    layout: Layout,
    lib: Library,
    palette: Palette,

    accent: Color,
    accent_target: Color,
    ever_shown: bool,

    target_idx: usize,
    /// Unwrapped carousel position. `focus_target` is kept as the nearest
    /// instance of `target_idx`, so the strip rotates the short way round.
    focus_pos: f32,
    focus_vel: f32,
    focus_target: f32,
    vis: f32,
    vis_vel: f32,
    commit: f32,
    applied: bool,

    open: bool,
    /// Opened from the tray rather than by holding Shift. Nothing is holding the
    /// shelf open in that case, so it lands on Enter or a click instead of on
    /// Shift being released.
    pinned: bool,
    showing: bool,
    keys_grabbed: bool,
    grab_count: u32,
    preview: bool,
    demo: bool,
    demo_t: f32,
    t_clear: f32,
    t_backdrop: f32,
    t_cards: f32,
    t_dress: f32,
    n_frames: u32,
    shift_since: Option<Instant>,
    /// Set when the shelf is opened from the tray: the click that opened it is
    /// still working its way through the polling loop, and must not come back as a
    /// click *on* the shelf, which would close it again.
    ignore_clicks_until: Option<Instant>,
    cursor: (i32, i32),
    geom: Vec<Card>,
    /// Monitor size the pre-transcoded wallpaper copies are made for.
    ready_w: u32,
    ready_h: u32,
    needs_paint: bool,
    quit: bool,
    timer_boost: bool,
}

impl App {
    fn fade(&self) -> f32 {
        self.vis * (1.0 - smoothstep(COMMIT_FADE, 1.0, self.commit.min(1.0)))
    }

    /// True while a tray-opened shelf is still ignoring clicks (see
    /// `ignore_clicks_until`).
    fn settling(&self, now: Instant) -> bool {
        self.ignore_clicks_until.is_some_and(|until| now < until)
    }

    /// The settled panel, in canvas coordinates.
    fn shelf(&self) -> Shelf {
        let panel_h = self.layout.card_h * 2.05;
        Shelf {
            cx: self.layout.cx,
            cy: self.layout.cy - self.layout.card_h * 0.03,
            h: panel_h,
            hw: self.strip.w as f32 * 0.5,
            radius: (self.layout.radius * 1.2).min(panel_h * 0.5),
        }
    }

    /// The liquid silhouette for this frame, driven by whichever animation is
    /// live: the entrance springs `vis` up, the pour runs `commit` down.
    fn liquid(&self) -> Liquid {
        Liquid {
            shelf: self.shelf(),
            band_h: self.strip.h as f32,
            morph: if self.commit > 0.0 {
                Morph::Pour(self.commit.clamp(0.0, 1.0))
            } else {
                Morph::Rise(self.vis.clamp(0.0, 1.0))
            },
        }
    }

    fn open_strip(&mut self, pinned: bool) {
        // Re-read the folder here rather than watching it: someone who drops an
        // image in and holds Shift should see it, and this costs one directory
        // listing when nothing has changed.
        self.refresh_library(false);
        self.open = true;
        self.pinned = pinned;
        self.commit = 0.0;
        self.applied = false;
        self.needs_paint = true;
        self.ever_shown = true;
        self.lib.start_prefetch();
        self.lib.request_ready(self.target_idx);
        self.set_grab(true);
        if !self.timer_boost {
            sys::set_timer_resolution(true);
            self.timer_boost = true;
        }
    }

    /// The first time the shelf is shown it snaps to the wallpaper Windows
    /// already has, so opening it is a no-op rather than a jump.
    fn first_appearance(&mut self) {
        if self.ever_shown {
            return;
        }
        self.focus_pos = self.target_idx as f32;
        self.focus_target = self.focus_pos;
        self.focus_vel = 0.0;
        if let Some(a) = self.lib.accent(self.target_idx) {
            self.accent = Color::from_rgb8(a[0], a[1], a[2]);
            self.accent_target = self.accent;
        }
    }

    fn close_strip(&mut self) {
        self.open = false;
        self.set_grab(false);
        if self.timer_boost {
            sys::set_timer_resolution(false);
            self.timer_boost = false;
        }
        self.lib.trim(self.target_idx);
    }

    /// While open, the navigation keys belong to us rather than to the desktop.
    fn set_grab(&mut self, on: bool) {
        if self.keys_grabbed == on {
            return;
        }
        self.grab_count = sys::grab_keys(on);
        self.keys_grabbed = on;
    }

    /// Points the spring at the nearest instance of the selected item.
    fn retarget(&mut self) {
        let n = self.lib.len();
        if n == 0 {
            return;
        }
        self.focus_target = nearest_virtual(self.focus_pos, self.target_idx, n);
    }

    fn select(&mut self, idx: usize) {
        self.target_idx = idx;
        if self.open {
            self.retarget();
        } else {
            // Nothing is drawn, so there is nothing to glide: land on it.
            self.focus_pos = idx as f32;
            self.focus_target = self.focus_pos;
            self.focus_vel = 0.0;
        }
        // Have the worker prepare this one next: it is what a commit would apply.
        self.lib.request_ready(self.target_idx);
    }

    /// Where the tray's tooltip points, and what the menu's "Next wallpaper" acts
    /// on: whichever wallpaper is under the focus marker.
    fn update_tooltip(&self) {
        let text = match self.lib.name(self.target_idx) {
            Some(name) => format!("carasoul — {name}"),
            None => "carasoul — no wallpapers found".to_string(),
        };
        tray::set_tooltip(&text);
    }

    /// Everything the notification-area icon can ask for. Split out of `step` so
    /// the frame loop stays a frame loop.
    fn tray_command(&mut self, cmds: u32) {
        if cmds & tray::CMD_OPEN != 0 {
            self.first_appearance();
            if self.open {
                self.needs_paint = true;
            } else {
                self.open_strip(true);
                self.ignore_clicks_until =
                    Some(Instant::now() + Duration::from_millis(CLICK_SETTLE_MS));
            }
        }
        if cmds & tray::CMD_NEXT != 0 && !self.lib.is_empty() {
            let next = wrap_index(self.target_idx as i32 + 1, self.lib.len());
            self.select(next);
            if self.open {
                // The shelf is up, so this reads as browsing: let it glide over and
                // leave the commit to Shift, Enter or a click.
                self.needs_paint = true;
            } else {
                // Nothing on screen to watch: rotate and land it.
                self.apply_selection();
                self.update_tooltip();
            }
        }
        if cmds & tray::CMD_FOLDER != 0 {
            let dir = self.lib.dir.clone();
            if !dir.is_dir() {
                // Sending the user to a folder that does not exist yet is useless.
                let _ = std::fs::create_dir_all(&dir);
            }
            std::process::Command::new("explorer.exe")
                .arg(&dir)
                .spawn()
                .ok();
        }
        if cmds & tray::CMD_RESCAN != 0 {
            self.refresh_library(true);
        }
        if cmds & tray::CMD_AUTOSTART != 0 {
            sys::register_autostart(!sys::autostart_enabled());
        }
        if cmds & tray::CMD_QUIT != 0 {
            self.quit = true;
        }
    }

    /// Re-reads the wallpaper folder. Called from the tray menu (which forces it)
    /// and every time the shelf opens (which only rebuilds when the folder has
    /// actually changed). The selection follows the wallpaper it was on by path,
    /// so an image appearing next to it never steals the focus.
    fn refresh_library(&mut self, force: bool) {
        let current = self
            .lib
            .items
            .get(self.target_idx)
            .map(|it| it.path.clone());
        let changed = if force {
            self.lib.rescan(current.as_deref());
            true
        } else {
            self.lib.refresh_if_changed(current.as_deref())
        };
        if !changed {
            return;
        }

        self.target_idx = library::index_after_refresh(&self.lib.items, current.as_deref());
        // `select` retargets when the shelf is on screen and lands the focus when
        // it is not — and either way asks for this wallpaper's copy next.
        self.select(self.target_idx);
        if self.lib.is_empty() {
            self.close_strip();
            self.target_idx = 0;
        } else {
            self.lib.start_prefetch();
        }
        tray::set_has_wallpapers(!self.lib.is_empty());
        self.update_tooltip();
        println!(
            "{} wallpaper(s) in {}",
            self.lib.len(),
            self.lib.dir.display()
        );
    }

    /// Applying a wallpaper and re-broadcasting the accent both block for a while
    /// (the colour broadcast alone can take hundreds of milliseconds), so they run
    /// off the UI thread: the shelf finishes its animation smoothly while the
    /// desktop changes behind it.
    fn apply_selection(&mut self) {
        let Some(item) = self.lib.items.get(self.target_idx) else {
            return;
        };
        let path = item.path.clone();
        let accent = item.accent;
        let (rw, rh) = (self.ready_w, self.ready_h);
        std::thread::Builder::new()
            .name("apply".into())
            .spawn(move || {
                sys::lower_current_thread_priority();
                // Nearly always already there: the worker prepares each card as it
                // is decoded, and the one the user lands on is requested first.
                let ready = library::ready_for(&path, rw, rh);
                sys::set_wallpaper(ready.as_deref().unwrap_or(&path));
                if let Some(a) = accent {
                    sys::apply_accent((a[0], a[1], a[2]));
                }
            })
            .ok();
    }

    /// Geometry for the current frame; also what hit-testing uses. This is a
    /// carousel: virtual slots are mapped onto items by modulo, so the strip
    /// rotates endlessly and never reaches an end.
    fn build_geometry(&mut self) {
        let l = self.layout;
        let n = self.lib.len();
        let global = self.fade();
        let mut cards: Vec<Card> = Vec::with_capacity(14);
        if n == 0 {
            self.geom.clear();
            return;
        }

        // Cards lean into the movement — the shear is what makes them parallelograms.
        let lean = (self.focus_vel * 0.05).clamp(-0.30, 0.30);
        let first = (self.focus_pos - CULL).floor() as i32;
        let last = (self.focus_pos + CULL).ceil() as i32;
        // Cards ride the liquid surface on the way in.
        let front = if self.commit > 0.0 {
            0.0
        } else {
            self.liquid().front()
        };

        for slot in first..=last {
            let d = slot as f32 - self.focus_pos;
            let ad = d.abs();
            let g = (-(d * d) * 1.35).exp();

            // Staggered entrance: distant cards land later, so the shelf unfurls
            // outward instead of fading in as one slab.
            let lag = (ad * 0.07).min(0.85);
            let vc = ((self.vis - lag) / (1.0 - lag).max(0.15)).clamp(0.0, 1.0);

            let mut scale = (1.0 + 0.30 * g) * (0.90 + 0.10 * vc);
            let mut x = l.cx + d * l.stride;
            let mut y = l.cy - 22.0 * g + (1.0 - vc) * l.card_h * 0.30;
            let mut alpha = (0.55 + 0.45 * g) * global * vc;
            let mut skew = l.skew * scale * (1.0 + lean);

            if self.commit > 0.0 {
                let p = self.commit.min(1.0);
                // Ease-in, so the collapse accelerates into the chosen card.
                let t = smoothstep(0.10, 1.0, p * p);
                x += (l.cx - x) * t * 0.90;
                // ... and then they go with the pour, dropping out of the bottom
                // of the band while they fade.
                y += t * t * l.card_h * 1.9;
                alpha *= 1.0 - t;
                scale *= 1.0 - 0.35 * t;
                skew *= 1.0 - 0.50 * t;
                if ad < 0.5 {
                    scale *= 1.0 + 0.18 * (p * std::f32::consts::PI).sin();
                }
            } else {
                // Hold each card under the surface until the rise has passed it,
                // so the strip is pushed up out of the liquid rather than fading
                // in ahead of it.
                y = y.max(front + l.card_h * scale * 0.30);
            }

            let hw = l.card_w * scale * 0.5;
            let hh = l.card_h * scale * 0.5;
            cards.push(Card {
                idx: wrap_index(slot, n),
                xf: Xform {
                    cx: x,
                    cy: y,
                    hw,
                    hh,
                    skew,
                },
                alpha: alpha.clamp(0.0, 1.0),
                focus: g,
            });
        }
        // Painted back-to-front so the focused card sits on top.
        cards.sort_by(|a, b| {
            a.focus
                .partial_cmp(&b.focus)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.geom = cards;
    }

    fn hovered(&self, cx: f32, cy: f32) -> Option<usize> {
        for card in self.geom.iter().rev() {
            if card.alpha < 0.30 {
                continue;
            }
            let (u, v) = card.xf.invert(cx, cy);
            if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
                return Some(card.idx);
            }
        }
        None
    }

    fn animating(&self) -> bool {
        let vis_target = if self.open { 1.0 } else { 0.0 };
        (self.vis - vis_target).abs() > 0.002
            || self.vis_vel.abs() > 0.02
            || (self.focus_pos - self.focus_target).abs() > 0.0015
            || self.focus_vel.abs() > 0.02
            || self.commit > 0.0
            || (self.accent.r - self.accent_target.r).abs() > 0.004
            || (self.accent.g - self.accent_target.g).abs() > 0.004
            || (self.accent.b - self.accent_target.b).abs() > 0.004
    }

    fn render(&mut self) {
        if !self.overlay.ensure_surface() {
            return;
        }
        let (w, h) = (self.overlay.w, self.overlay.h);
        let strip = self.strip;
        let l = self.layout;
        let pal = self.palette;
        let global = self.fade();
        let vis = self.vis;
        let accent_glow = self.palette.accent;
        let (tw, th) = (self.lib.thumb_w, self.lib.thumb_h);
        // Resolved before the surface is borrowed: the liquid is a `&self` read and
        // the pixels below are a `&mut self.overlay` one, and they cannot overlap.
        let liq = self.liquid();
        let (liq_y0, liq_y1) = liq.extents();
        // How much of the shelf is the wallpaper wash rather than liquid.
        let set = if self.commit > 0.0 {
            1.0 - smoothstep(POUR_WASH.0, POUR_WASH.1, self.commit.min(1.0))
        } else {
            smoothstep(RISE_WASH.0, RISE_WASH.1, vis.clamp(0.0, 1.0))
        };
        let wash_a = global * set;
        // The liquid goes down first and the panel over it, so the liquid's own
        // alpha has to be the remainder the panel's leaves — then the pair come out
        // at exactly `global` and the desktop never shows through the seam.
        let liquid_a = if wash_a >= 0.996 {
            0.0
        } else {
            global * (1.0 - set) / (1.0 - wash_a)
        };

        let px = self.overlay.pixels();
        let mut c = Canvas::new(px, w, h);
        let mut mark = Instant::now();
        c.clear();
        if self.demo {
            self.t_clear += mark.elapsed().as_secs_f32();
            mark = Instant::now();
        }

        // Backdrop shelf. Settled, this is one pass that paints the focused
        // wallpaper's wash, cross-faded toward the next one, tinted with the accent
        // and sinking into shadow — keeping it to a single pass is what makes the
        // motion smooth. While the shelf is arriving or pouring away the silhouette
        // is a liquid instead (see `Liquid`), and the wash is laid over it: the two
        // outlines coincide while the swap happens, so it reads as the liquid
        // setting rather than as one shape being replaced by another.
        if global > 0.002 {
            let panel_h = l.card_h * 2.05;
            let panel_cy = l.cy - l.card_h * 0.03;
            let panel = Xform::rect(l.cx, panel_cy, w as f32 * 0.5, panel_h * 0.5);
            let panel_radius = l.radius * 1.2;

            if liquid_a > 0.004 {
                // `over_clear`: the liquid is the first thing drawn into the freshly
                // cleared band, so it can be stored rather than blended, which is
                // most of what keeps the morph cheap in a half-lit frame.
                c.fill_profile(0.0, liq_y0, w as f32, liq_y1, true, |y| {
                    let (cx, hw, depth) = liq.row(y);
                    (cx, hw, liquid_color(&pal, depth).with_a(liquid_a))
                });
            }

            let n = self.lib.len();
            let mut painted = false;
            if n > 0 && wash_a > 0.004 {
                let base = self.focus_pos.floor();
                let t = (self.focus_pos - base).clamp(0.0, 1.0);
                let ia = wrap_index(base as i32, n);
                let ib = wrap_index(base as i32 + 1, n);
                let a = self
                    .lib
                    .items
                    .get(ia)
                    .and_then(|it| it.wash.as_deref())
                    .unwrap_or_default();
                let b = self
                    .lib
                    .items
                    .get(ib)
                    .and_then(|it| it.wash.as_deref())
                    .unwrap_or_default();
                if !a.is_empty() {
                    c.draw_shelf(
                        &panel,
                        panel_radius,
                        a,
                        b,
                        t,
                        library::WASH_W,
                        library::WASH_H,
                        pal.accent,
                        1.0,
                        0.22,
                        wash_a,
                    );
                    painted = true;
                }
            }
            if !painted && wash_a > 0.004 {
                // Thumbnails still decoding: a plain tinted shelf as a placeholder.
                let top = pal.panel_top.with_a(pal.panel_top.a * wash_a);
                let bottom = pal.panel_bottom.with_a(pal.panel_bottom.a * wash_a);
                c.fill_rounded_gradient(&panel, panel_radius, top, bottom);
            }

            // Accent hairlines top and bottom give the shelf an edge. They belong to
            // the panel, so they arrive and leave with the wash.
            let line_alpha = 0.55 * wash_a;
            let top_line = Xform::rect(l.cx, panel.cy - panel.hh + 1.5, panel.hw, 1.0);
            let bottom_line = Xform::rect(l.cx, panel.cy + panel.hh - 1.5, panel.hw, 1.0);
            c.fill_rounded(&top_line, 0.0, pal.hot.with_a(line_alpha));
            c.fill_rounded(&bottom_line, 0.0, accent_glow.with_a(line_alpha * 0.45));
        }

        if self.demo {
            self.t_backdrop += mark.elapsed().as_secs_f32();
            mark = Instant::now();
        }

        // Cards.
        for card in self.geom.iter() {
            if card.alpha <= 0.004 {
                continue;
            }
            let dim = 1.0 - card.focus;
            let rad = l.radius * (card.xf.hw / (l.card_w * 0.5)).max(0.6);
            if let Some(thumb) = self
                .lib
                .items
                .get(card.idx)
                .and_then(|it| it.thumb.as_ref())
            {
                // Point sampling for the small dimmed cards; smooth for the focus,
                // which is where sharpness actually shows.
                c.draw_texture_fast(
                    &card.xf,
                    rad,
                    thumb,
                    tw,
                    th,
                    card.alpha,
                    dim,
                    card.focus < 0.55,
                );
            } else {
                // Still decoding: a tinted placeholder.
                let base = accent_glow
                    .scale(0.22)
                    .mix(Color::rgb(0.06, 0.06, 0.09), 0.55);
                c.fill_rounded(&card.xf, rad, base.with_a(card.alpha));
            }
        }
        if self.demo {
            self.t_cards += mark.elapsed().as_secs_f32();
            mark = Instant::now();
        }

        // Focus dressing: glow, ring and the accent tab underneath.
        if let Some(front) = self.geom.iter().max_by(|a, b| {
            a.focus
                .partial_cmp(&b.focus)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) {
            let g = front.focus;
            if g > 0.35 && front.alpha > 0.02 {
                let pulse = if self.commit > 0.0 {
                    1.0 + 1.6 * (self.commit.min(1.0) * std::f32::consts::PI).sin()
                } else {
                    1.0
                };
                let r = l.radius * (front.xf.hw / (l.card_w * 0.5)).max(0.6);

                // Glow underneath.
                let glow = Xform {
                    cx: front.xf.cx,
                    cy: front.xf.cy + 10.0,
                    hw: front.xf.hw + 4.0,
                    hh: front.xf.hh + 4.0,
                    skew: front.xf.skew * 1.02,
                };
                c.stroke_rounded(
                    &glow,
                    r + 4.0,
                    9.0 * pulse,
                    accent_glow.with_a(0.30 * g * front.alpha),
                );

                // Ring around the focused card.
                c.stroke_rounded(
                    &front.xf,
                    r,
                    2.6 * pulse,
                    pal.hot.with_a(0.95 * g * front.alpha),
                );

                // Accent tab.
                let bar_w = front.xf.hw * 0.85;
                let bar_y = front.xf.cy + front.xf.hh + 15.0;
                let soft = Xform::rect(front.xf.cx, bar_y, bar_w * 1.5, 4.0);
                let crisp = Xform::rect(front.xf.cx, bar_y, bar_w, 2.4);
                c.fill_rounded(&soft, 4.0, accent_glow.with_a(0.18 * g * front.alpha));
                c.fill_rounded(&crisp, 2.4, pal.hot.with_a(0.92 * g * front.alpha));
            }
        }

        drop(c);
        if self.demo {
            self.t_dress += mark.elapsed().as_secs_f32();
            self.n_frames += 1;
        }
        self.overlay.present(strip.x, strip.y);
    }

    fn step(&mut self, e: &sys::Edges, dt: f32, now: Instant) -> bool {
        let mut dirty = self.lib.poll();
        let n = self.lib.len();
        // Refresh hit-test geometry first, then draw with the geometry computed
        // after the easing below, so input always matches what is on screen.
        self.build_geometry();

        if e.shift_down {
            self.shift_since = Some(now);
        }
        if e.shift_up {
            self.shift_since = None;
        }

        if self.open && e.ctrl_held && e.q {
            self.quit = true;
            return false;
        }

        // --preview: hold the strip open so the drawing path can be inspected
        // without pressing anything.
        // --preview / --demo force the strip open. Navigation is handled by the
        // shared branch below, so these modes exercise the same input path a real
        // keyboard does.
        let forced = self.preview || self.demo;
        if forced {
            if e.esc {
                self.set_grab(false);
                self.quit = true;
                return false;
            }
            if !self.open {
                self.first_appearance();
                self.open_strip(false);
            }
            // --demo cycles the carousel by itself, so the animation can be
            // watched (and measured) without touching the keyboard.
            if self.demo {
                self.demo_t += dt;
                if self.demo_t > 0.75 {
                    self.demo_t = 0.0;
                    // Driven through the real hook -> pending-bits path, so the
                    // keyboard wiring is exercised without a keyboard.
                    sys::inject_pending(sys::INJECT_NEXT);
                }
            }
        }

        if !self.open {
            if e.other_key() {
                self.shift_since = None; // typing, not asking for the strip
            }
            if let Some(t) = self.shift_since {
                let held = now.duration_since(t) >= Duration::from_millis(HOLD_MS);
                if held && !self.lib.is_empty() && sys::desktop_has_focus() {
                    self.shift_since = None;
                    self.first_appearance();
                    self.open_strip(false);
                }
            }
        } else if self.commit <= 0.0 {
            // A click is ignored for the moment after a tray-opened shelf appears;
            // see `ignore_clicks_until`.
            let click = e.click && !self.settling(now);
            let right_click = e.right_click && !self.settling(now);
            if e.esc || right_click {
                // Esc or a right-click abandons the selection.
                self.close_strip();
            } else if !forced && !self.pinned && !e.shift_held {
                // Letting go of Shift keeps whatever is under the focus marker.
                // A shelf opened from the tray has no Shift to release, so it
                // waits for Enter or a click instead.
                self.commit = 1e-4;
                self.applied = false;
            } else {
                if n > 0 && (e.left || e.up) {
                    self.select(wrap_index(self.target_idx as i32 - 1, n));
                }
                if n > 0 && (e.right || e.down) {
                    self.select(wrap_index(self.target_idx as i32 + 1, n));
                }

                let (mx, my) = sys::cursor_pos();
                let (lx, ly) = ((mx - self.strip.x) as f32, (my - self.strip.y) as f32);
                // Only follow the mouse once it actually moves, so the strip does
                // not jump to whatever happens to sit under a resting cursor.
                if (mx, my) != self.cursor {
                    self.cursor = (mx, my);
                    if let Some(hover) = self.hovered(lx, ly) {
                        if hover != self.target_idx {
                            self.select(hover);
                        }
                    }
                }

                if e.enter || click {
                    match self.hovered(lx, ly) {
                        Some(hit) => {
                            self.select(hit);
                            self.commit = 1e-4;
                            self.applied = false;
                        }
                        // Clicking the shelf itself cancels, like Esc.
                        None if click => self.close_strip(),
                        None => {}
                    }
                }
            }
        }

        if self.commit > 0.0 {
            self.commit += dt / COMMIT_SECS;
            // Hand the desktop its new wallpaper as the pour starts rather than
            // part-way through it: the work happens on another thread, and the
            // earlier it begins the sooner it lands behind the animation.
            if !self.applied && self.commit > 0.0 {
                self.apply_selection();
                self.applied = true;
                self.update_tooltip();
                dirty = true;
            }
            if self.commit >= 1.0 {
                self.commit = 0.0;
                self.vis = 0.0;
                self.vis_vel = 0.0;
                self.open = false;
                self.set_grab(false);
                if self.timer_boost {
                    sys::set_timer_resolution(false);
                    self.timer_boost = false;
                }
                self.lib.trim(self.target_idx);
                self.needs_paint = true;
                dirty = true;
            }
        }

        if let Some(a) = self.lib.accent(self.target_idx) {
            self.accent_target = Color::from_rgb8(a[0], a[1], a[2]);
        }
        if self.ever_shown {
            approach_color(&mut self.accent, self.accent_target, 7.0, dt);
        } else {
            self.accent = self.accent_target;
        }
        self.palette = palette_for(self.accent);

        let vt = if self.open { 1.0 } else { 0.0 };
        // Springy rather than eased: the shelf arrives with a bit of snap.
        step_spring(&mut self.vis, &mut self.vis_vel, vt, dt, 3.6, 0.74);
        step_spring(
            &mut self.focus_pos,
            &mut self.focus_vel,
            self.focus_target,
            dt,
            2.7,
            0.86,
        );

        dirty || self.animating() || self.needs_paint
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn usage() {
    println!(
        "carasoul [options]\n\n\
         Runs in the notification area — no window, no console. Hold Shift for\n\
         ~{HOLD_MS} ms on the desktop to open the shelf, Left/Right (or Up/Down) to\n\
         browse, release Shift (or click a card, or press Enter) to apply it.\n\
         Esc or a right-click cancels. Left-click the tray icon to open the shelf,\n\
         right-click it for the menu (including Quit).\n\n\
         Options:\n\
           --dir <path>      wallpaper folder (default: <exe>/wallpapers)\n\
           --no-autostart    do not register the Windows startup entry\n\
           --uninstall       remove the startup entry and exit\n\
           --list            list wallpapers with their index and exit\n\
           --apply <index>   apply one wallpaper + its accent colour and exit\n\
           --preview         hold the shelf open on startup (UI testing)\n\
           --demo            preview + auto-cycles and prints frame timings\n\
           -h, --help        this text"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir: Option<PathBuf> = None;
    let mut autostart = true;
    let mut apply: Option<usize> = None;
    let mut list = false;
    let mut preview = false;
    let mut demo = false;

    // The modes that print borrow the console they were launched from: this is
    // linked as a GUI app (see the crate docs), so it has none of its own.
    if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-h" | "--help" | "--list" | "--apply" | "--demo" | "--preview" | "--uninstall"
        )
    }) {
        sys::attach_parent_console();
    }

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" | "-d" => {
                i += 1;
                dir = args.get(i).map(PathBuf::from);
            }
            "--no-autostart" => autostart = false,
            "--uninstall" => {
                sys::register_autostart(false);
                println!("Removed the Windows startup entry.");
                return;
            }
            "--list" => list = true,
            "--preview" => preview = true,
            "--demo" => {
                demo = true;
                preview = true;
            }
            "--apply" => {
                i += 1;
                apply = args.get(i).and_then(|s| s.parse::<usize>().ok());
            }
            "-h" | "--help" => {
                usage();
                return;
            }
            other => eprintln!("ignoring unknown argument: {other}"),
        }
        i += 1;
    }

    let current = sys::current_wallpaper();

    if list || apply.is_some() {
        let (tw0, th0) = (library::MIN_THUMB_W, library::MIN_THUMB_W * 5 / 8);
        let lib = Library::load(dir, current.as_deref(), tw0, th0, 0, 0);
        if list {
            println!("folder: {}", lib.dir.display());
            for (i, it) in lib.items.iter().enumerate() {
                println!("{i:>3}  {}", it.name);
            }
            return;
        }
        let idx = apply.unwrap();
        let Some(item) = lib.items.get(idx) else {
            eprintln!("no wallpaper at index {idx} ({} found)", lib.len());
            std::process::exit(1);
        };
        println!("applying {}", item.name);
        sys::set_wallpaper(&item.path);
        if let Some(a) = library::accent_of(&item.path) {
            println!("accent: #{:02X}{:02X}{:02X}", a[0], a[1], a[2]);
            sys::apply_accent((a[0], a[1], a[2]));
        }
        return;
    }

    if !preview && !sys::claim_single_instance() {
        // Started twice — from the startup entry and from a shortcut, say. The
        // instance that is already there owns the tray icon, so hand it the
        // request that brought the user here and step aside.
        if !sys::wake_running_instance() {
            eprintln!("carasoul is already running");
        }
        return;
    }

    sys::enable_dpi_awareness();

    if autostart && !sys::register_autostart(true) {
        eprintln!("could not register the startup entry");
    }

    let mon = sys::monitor_rect_at(sys::cursor_pos());
    let strip = strip_for(mon);
    let layout = make_layout(&strip);

    let overlay = match sys::Overlay::create(strip.w, strip.h) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("could not create the overlay window: {e}");
            std::process::exit(1);
        }
    };

    // The tray icon is the app's front door: everything else is invisible.
    let tray_hwnd = overlay.hwnd;

    let (thumb_w, thumb_h) = thumb_size_for(mon.h);
    let lib = Library::load(
        dir,
        current.as_deref(),
        thumb_w,
        thumb_h,
        mon.w.max(1) as u32,
        mon.h.max(1) as u32,
    );
    let target_idx = match current.as_deref().and_then(|c| lib.index_of(c)) {
        Some(i) => i,
        None => 0,
    };
    if lib.is_empty() {
        eprintln!("no wallpapers found in {}", lib.dir.display());
        eprintln!("  put .jpg/.jpeg/.jfif/.png files there, or point elsewhere with --dir <path>");
    } else {
        println!("{} wallpaper(s) in {}", lib.len(), lib.dir.display());
    }

    let accent = Color::rgb(0.36, 0.55, 0.95);
    let mut app = App {
        overlay,
        input: sys::Input::new(),
        strip,
        layout,
        lib,
        palette: palette_for(accent),
        accent,
        accent_target: accent,
        ever_shown: false,
        target_idx,
        focus_pos: target_idx as f32,
        focus_vel: 0.0,
        focus_target: target_idx as f32,
        vis: 0.0,
        vis_vel: 0.0,
        commit: 0.0,
        applied: false,
        open: false,
        pinned: false,
        showing: false,
        keys_grabbed: false,
        grab_count: 0,
        preview,
        demo,
        demo_t: 0.0,
        t_clear: 0.0,
        t_backdrop: 0.0,
        t_cards: 0.0,
        t_dress: 0.0,
        n_frames: 0,
        shift_since: None,
        ignore_clicks_until: None,
        cursor: sys::cursor_pos(),
        geom: Vec::new(),
        ready_w: mon.w.max(1) as u32,
        ready_h: mon.h.max(1) as u32,
        needs_paint: false,
        quit: false,
        timer_boost: false,
    };

    tray::set_has_wallpapers(!app.lib.is_empty());
    app.update_tooltip();
    if !preview {
        // Started last, so the icon appears already carrying its tooltip.
        tray::init(tray_hwnd);
        if sys::claim_first_run() {
            // One balloon, ever, saying the one thing that is needed right now: a
            // windowless app has no other way to explain either of these.
            if app.lib.is_empty() {
                let dir = app.lib.dir.display().to_string();
                tray::notify(
                    "carasoul",
                    &format!(
                        "No wallpapers in {dir} yet. Right-click the tray icon to open the \
                         folder, then Rescan."
                    ),
                );
            } else {
                tray::notify(
                    "carasoul",
                    "In the tray. Hold Shift on the desktop to pick a wallpaper.",
                );
            }
        }
    }

    let mut last = Instant::now();
    let mut frames = 0u32;
    let mut acc = 0f32;
    let mut worst = 0f32;
    let mut loops = 0u32;
    let mut sample_at = Instant::now();
    let mut announced = false;
    loop {
        let now = Instant::now();
        let dt = (now - last).as_secs_f32().clamp(0.0, 0.1);
        last = now;
        loops += 1;

        let pump = app.overlay.pump();
        if pump.quit {
            break;
        }
        // A tray click or a menu pick arrives as a message, so it is drained here
        // and acted on outside the message proc.
        let cmds = tray::poll();
        if cmds != 0 {
            app.tray_command(cmds);
        }
        if pump.display_changed {
            let mon = sys::monitor_rect_at(sys::cursor_pos());
            let strip = strip_for(mon);
            // The pre-transcoded copies are monitor-sized; anything already in the
            // cache for the old size simply stops matching (and is swept later),
            // while the ones built from here on use the new size.
            app.ready_w = mon.w.max(1) as u32;
            app.ready_h = mon.h.max(1) as u32;
            app.lib.ready_w = app.ready_w;
            app.lib.ready_h = app.ready_h;
            if strip.w != app.strip.w || strip.h != app.strip.h {
                app.overlay.hide();
                app.showing = false;
                if app.overlay.resize(strip.w, strip.h).is_ok() {
                    app.strip = strip;
                    app.layout = make_layout(&app.strip);
                    app.needs_paint = true;
                }
            }
        }

        if app.demo && !announced && app.keys_grabbed {
            announced = true;
            println!(
                "input grab: {} hotkeys claimed, low-level hook: {}",
                app.grab_count,
                if sys::hook_installed() {
                    "installed"
                } else {
                    "NOT installed"
                }
            );
        }

        let edges = app.input.poll();
        let dirty = app.step(&edges, dt, now);

        let fade = app.fade();
        if fade > 0.004 {
            if dirty {
                let t0 = Instant::now();
                app.render();
                if app.demo {
                    let el = t0.elapsed().as_secs_f32();
                    frames += 1;
                    acc += el;
                    worst = worst.max(el);
                }
                app.needs_paint = false;
            }
            if !app.showing {
                app.overlay.show_at(app.strip.x, app.strip.y);
                app.showing = true;
            }
        } else if app.showing {
            app.overlay.hide();
            app.showing = false;
        }

        if app.quit {
            break;
        }

        if app.demo {
            let elapsed = sample_at.elapsed().as_secs_f32();
            if elapsed >= 1.0 {
                println!(
                    "render {:.0}/s  loop {:.0}/s  avg={:.2}ms worst={:.2}ms | clear={:.2} backdrop={:.2} cards={:.2} dress={:.2} ms/frame",
                    frames as f32 / elapsed,
                    loops as f32 / elapsed,
                    acc * 1000.0 / frames.max(1) as f32,
                    worst * 1000.0,
                    app.t_clear * 1000.0 / app.n_frames.max(1) as f32,
                    app.t_backdrop * 1000.0 / app.n_frames.max(1) as f32,
                    app.t_cards * 1000.0 / app.n_frames.max(1) as f32,
                    app.t_dress * 1000.0 / app.n_frames.max(1) as f32,
                );
                app.t_clear = 0.0;
                app.t_backdrop = 0.0;
                app.t_cards = 0.0;
                app.t_dress = 0.0;
                app.n_frames = 0;
                frames = 0;
                acc = 0.0;
                worst = 0.0;
                loops = 0;
                sample_at = Instant::now();
            }
        }

        std::thread::sleep(Duration::from_millis(if app.showing { 3 } else { 12 }));
    }

    app.set_grab(false);
    app.overlay.hide();
    tray::remove();
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// The liquid is only convincing if the hand-over to the wash panel cannot be
/// seen, and that rests on one property the maths alone does not guarantee: both
/// morphs have to land on the panel's own outline exactly. Hence these two.
#[cfg(test)]
mod liquid_geometry {
    use super::*;

    fn shelf_and_strip() -> (Shelf, Strip) {
        let strip = strip_for(sys::ScreenRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        });
        let l = make_layout(&strip);
        let h = l.card_h * 2.05;
        let shelf = Shelf {
            cx: l.cx,
            cy: l.cy - l.card_h * 0.03,
            h,
            hw: strip.w as f32 * 0.5,
            radius: (l.radius * 1.2).min(h * 0.5),
        };
        (shelf, strip)
    }

    #[test]
    fn both_morphs_land_on_the_panel() {
        let (shelf, strip) = shelf_and_strip();
        let settled = (shelf.cy - shelf.h * 0.5, shelf.cy + shelf.h * 0.5);
        for (tag, morph) in [("rise", Morph::Rise(1.0)), ("pour", Morph::Pour(0.0))] {
            let liq = Liquid {
                shelf,
                band_h: strip.h as f32,
                morph,
            };
            assert_eq!(liq.extents(), settled, "{tag}");
            for row in 0..strip.h {
                let y = row as f32 + 0.5;
                let (_, hw, _) = liq.row(y);
                if y > settled.0 && y < settled.1 {
                    assert!((hw - shelf.hw_at(y)).abs() < 1e-4, "{tag} @{y}: {hw}");
                }
            }
        }
    }

    #[test]
    fn morph_frames_stay_inside_the_band() {
        let (shelf, strip) = shelf_and_strip();
        for step in 0..=100 {
            let p = step as f32 / 100.0;
            for (tag, morph) in [("rise", Morph::Rise(p)), ("pour", Morph::Pour(p))] {
                let liq = Liquid {
                    shelf,
                    band_h: strip.h as f32,
                    morph,
                };
                let (yt, yb) = liq.extents();
                assert!(yt.is_finite() && yb.is_finite() && yb >= yt, "{tag} {p}");
                for row in 0..strip.h {
                    let (cx, hw, d) = liq.row(row as f32 + 0.5);
                    assert!(d.is_finite() && hw.is_finite(), "{tag} {p} @{row}");
                    assert!(hw >= 0.0 && hw <= shelf.hw + 1e-3, "{tag} {p} @{row}: {hw}");
                    // A row that reaches the band's edge has to stay touching it, or
                    // the liquid would show a notch down the screen edge.
                    assert!(
                        cx - hw >= -1.0 && cx + hw <= strip.w as f32 + 1.0,
                        "{tag} {p} @{row}"
                    );
                }
            }
        }
    }

    /// What the liquid costs, since the shape is a per-pixel span fill rather than
    /// the wash's rounded rect: run `cargo test --release cost -- --nocapture`. It
    /// times the one `fill_profile` call the frame makes, so it is that call and
    /// nothing else — the frame adds the wash, the cards and the surface clear on
    /// top of it. The `blended` line is the same shape without `over_clear`, and is
    /// what says whether that flag is still earning its keep.
    #[test]
    fn cost() {
        let (shelf, strip) = shelf_and_strip();
        let (w, h) = (strip.w, strip.h);
        let pal = palette_for(Color::from_rgb8(90, 140, 240));
        let mut px = vec![0u32; (w * h) as usize];
        let cases = [
            ("rise 0.8, stored", Morph::Rise(0.8), true),
            ("rise 0.8, blended", Morph::Rise(0.8), false),
            ("pour 0.5, stored", Morph::Pour(0.5), true),
            ("settled panel, stored", Morph::Rise(1.0), true),
        ];
        for (tag, morph, over_clear) in cases {
            let liq = Liquid {
                shelf,
                band_h: h as f32,
                morph,
            };
            let (y0, y1) = liq.extents();
            let mut c = Canvas::new(&mut px, w, h);
            let mut n = 0u32;
            let t0 = Instant::now();
            while t0.elapsed().as_millis() < 400 {
                // Half-lit, as the liquid is for most of the entrance.
                c.fill_profile(0.0, y0, w as f32, y1, over_clear, |y| {
                    let (cx, hw, d) = liq.row(y);
                    (cx, hw, liquid_color(&pal, d).with_a(0.7))
                });
                n += 1;
            }
            println!(
                "{tag}: {:.2} ms/frame ({w}x{h})",
                t0.elapsed().as_secs_f64() * 1000.0 / n as f64
            );
        }
    }
}
