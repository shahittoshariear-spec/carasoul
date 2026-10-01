//! A deliberately tiny software renderer. Everything is drawn as an
//! anti-aliased rounded rectangle pushed through an affine shear, which is all
//! the "parallelogram" card look needs. Output is premultiplied BGRA, matching
//! what `UpdateLayeredWindow` wants.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn from_rgb8(r: u8, g: u8, b: u8) -> Self {
        Self::rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
    }

    pub fn with_a(self, a: f32) -> Self {
        Self { a, ..self }
    }

    pub fn scale(self, k: f32) -> Self {
        Self { r: self.r * k, g: self.g * k, b: self.b * k, a: self.a }
    }

    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }
}

/// A card-space rectangle mapped to the screen:
/// `x = cx - hw + u*2hw + skew*(0.5 - v)`, `y = cy - hh + v*2hh`.
#[derive(Clone, Copy, Debug)]
pub struct Xform {
    pub cx: f32,
    pub cy: f32,
    pub hw: f32,
    pub hh: f32,
    pub skew: f32,
}

impl Xform {
    pub fn rect(cx: f32, cy: f32, hw: f32, hh: f32) -> Self {
        Self { cx, cy, hw, hh, skew: 0.0 }
    }

    /// Inverse mapping: screen point -> card-space (u, v), both 0..1 inside.
    pub fn invert(&self, x: f32, y: f32) -> (f32, f32) {
        let h = 2.0 * self.hh;
        let v = (y - (self.cy - self.hh)) / h;
        let left = self.cx - self.hw + self.skew * (0.5 - v);
        let u = (x - left) / (2.0 * self.hw);
        (u, v)
    }
}

#[derive(Clone, Copy, Debug)]
struct RectI {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

pub struct Canvas<'a> {
    pub w: i32,
    pub h: i32,
    px: &'a mut [u32],
    cov: Vec<f32>,
}

impl<'a> Canvas<'a> {
    pub fn new(px: &'a mut [u32], w: i32, h: i32) -> Self {
        Self { w, h, px, cov: Vec::new() }
    }

    pub fn clear(&mut self) {
        for v in self.px.iter_mut() {
            *v = 0;
        }
    }

    // -- public drawing ----------------------------------------------------

    pub fn fill_rounded(&mut self, xf: &Xform, radius: f32, c: Color) {
        let r = self.bounds(xf);
        self.coverage(xf, radius, 0.0, r);
        self.blend(r, |_, _| c);
    }

    /// Vertical gradient, used for the backdrop shelf. The colour is worked out
    /// once per row rather than per pixel, which matters at full screen width.
    pub fn fill_rounded_gradient(&mut self, xf: &Xform, radius: f32, top: Color, bottom: Color) {
        let r = self.bounds(xf);
        self.coverage(xf, radius, 0.0, r);
        let (y0, span) = (xf.cy - xf.hh, (2.0 * xf.hh).max(1.0));
        let bw = (r.x1 - r.x0) as usize;
        if bw == 0 || r.y1 <= r.y0 {
            return;
        }
        for row in 0..(r.y1 - r.y0) as usize {
            let y = (r.y0 + row as i32) as f32 + 0.5;
            let c = top.mix(bottom, (y - y0) / span);
            let cov_row = row * bw;
            let px_row = (r.y0 + row as i32) as usize * self.w as usize;
            for col in 0..bw {
                let cov = self.cov[cov_row + col];
                if cov <= 0.002 {
                    continue;
                }
                self.blend_at(px_row + (r.x0 + col as i32) as usize, c, cov);
            }
        }
    }

    pub fn stroke_rounded(&mut self, xf: &Xform, radius: f32, thickness: f32, c: Color) {
        let r = self.bounds(xf);
        self.coverage(xf, radius, thickness, r);
        self.blend(r, |_, _| c);
    }

    /// Fills a liquid silhouette: one span per row, whose centre, half-width and
    /// colour come from `f(y)`, anti-aliased the same analytic way the rounded
    /// rects are. A row is accumulated and consumed before the next one starts, so
    /// only a single row's coverage is ever live, and the run of pixels a row's two
    /// sub-samples agree on is written in one go — which is what keeps a full-width
    /// liquid off the per-pixel path, whether it is opaque or fading in.
    ///
    /// `f` returns `(centre_x, half_width, colour)`. The centre is per row so the
    /// body can sway, and a `half_width` of zero or less means the row is empty.
    ///
    /// `over_clear` says the surface under the shape is still untouched — every
    /// pixel zero, as it is straight after `clear()` — which lets the whole shape
    /// be written as premultiplied pixels instead of blended over what is there.
    /// Blending over clear produces those same bytes, so this is a speed decision
    /// rather than a correctness one, and it is a large one: at 1920x1080 a
    /// half-lit body costs ~1.3 ms stored against ~10 ms blended.
    pub fn fill_profile<F>(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        over_clear: bool,
        f: F,
    ) where
        F: Fn(f32) -> (f32, f32, Color),
    {
        let r = RectI {
            x0: (x0.floor() as i32).clamp(0, self.w),
            y0: (y0.floor() as i32).clamp(0, self.h),
            x1: (x1.ceil() as i32).clamp(0, self.w),
            y1: (y1.ceil() as i32).clamp(0, self.h),
        };
        if r.x1 <= r.x0 || r.y1 <= r.y0 {
            return;
        }
        let bw = (r.x1 - r.x0) as usize;
        if self.cov.len() < bw {
            self.cov.resize(bw, 0.0);
        }
        const SUB: usize = 2;
        let wgt = 1.0 / SUB as f32;

        for row in 0..(r.y1 - r.y0) as usize {
            let py = (r.y0 + row as i32) as f32;
            let mut spans = [(0.0f32, 0.0f32); SUB];
            let mut col = Color::rgba(0.0, 0.0, 0.0, 1.0);
            let mut lo = r.x1;
            let mut hi = r.x0 - 1;
            let mut il = f32::NEG_INFINITY;
            let mut ir = f32::INFINITY;
            let mut any = false;
            // A pixel is only fully covered when every sub-sample spans it, so an
            // empty sub-sample makes the whole row partial — which is exactly the
            // anti-aliased top and bottom of the shape.
            let mut solid_row = true;
            for (k, span) in spans.iter_mut().enumerate() {
                let y = py + (k as f32 + 0.5) * wgt;
                let (cx, hw, c) = f(y);
                col = c;
                let (a, b) = (cx - hw, cx + hw);
                *span = (a, b);
                if hw <= 0.0 {
                    solid_row = false;
                    continue;
                }
                any = true;
                lo = lo.min((a - 0.5).floor() as i32);
                hi = hi.max((b + 0.5).floor() as i32);
                il = il.max(a);
                ir = ir.min(b);
            }
            if !any {
                continue;
            }
            let (lo, hi) = (lo.max(r.x0), hi.min(r.x1 - 1));
            if hi < lo {
                continue;
            }
            for v in &mut self.cov[(lo - r.x0) as usize..(hi - r.x0 + 1) as usize] {
                *v = 0.0;
            }
            for (a, b) in spans {
                self.add_span(0, bw, r, a, b, wgt);
            }
            let row_base = (r.y0 + row as i32) as usize * self.w as usize;
            let (ia, ib) = if solid_row {
                (
                    ((il + 0.5).ceil() as i32).clamp(lo, hi + 1),
                    ((ir - 0.5).floor() as i32 + 1).clamp(lo, hi + 1),
                )
            } else {
                (lo, lo)
            };
            // Storing is only valid over clear, and blending a fully opaque colour
            // is a store anyway, so either makes the interior a single `fill`.
            let store = over_clear;
            if (store || col.a >= 0.996) && ib > ia {
                let v = if store { premul_px(col, 1.0) } else { pack(1.0, col.r, col.g, col.b) };
                self.px[row_base + (ia - r.x0) as usize..row_base + (ib - r.x0) as usize].fill(v);
                for xi in lo..ia {
                    let (idx, cov) = (row_base + (xi - r.x0) as usize, self.cov[(xi - r.x0) as usize]);
                    self.fill_px(idx, col, cov, store);
                }
                for xi in ib..=hi {
                    let (idx, cov) = (row_base + (xi - r.x0) as usize, self.cov[(xi - r.x0) as usize]);
                    self.fill_px(idx, col, cov, store);
                }
            } else {
                for xi in lo..=hi {
                    let (idx, cov) = (row_base + (xi - r.x0) as usize, self.cov[(xi - r.x0) as usize]);
                    self.fill_px(idx, col, cov, store);
                }
            }
        }
    }

    /// Composes the whole shelf backdrop in a single pass. Two things keep it
    /// cheap. The rounded-rect mask is derived analytically per row rather than
    /// rasterised into a coverage buffer first — that buffer spans the whole shelf
    /// (megabytes on a 4K display) so it was pure memory traffic, and it was the
    /// largest single item in the frame. And on rows that are fully opaque and
    /// fully covered vertically the interior is written one wash texel at a time
    /// with a straight `fill` of the pixels that texel covers, which is a memset
    /// per block and leaves only a couple of anti-aliased pixels per row to the
    /// per-pixel path.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_shelf(
        &mut self,
        xf: &Xform,
        radius: f32,
        wash_a: &[u32],
        wash_b: &[u32],
        t: f32,
        tw: u32,
        th: u32,
        tint: Color,
        shade_top: f32,
        shade_bottom: f32,
        alpha: f32,
    ) {
        if wash_a.is_empty() || tw == 0 || th == 0 || alpha <= 0.004 {
            return;
        }
        let cw = 2.0 * xf.hw;
        let ch = 2.0 * xf.hh;
        if cw <= 1.0 || ch <= 1.0 {
            return;
        }
        let radius = radius.min(0.5 * cw).min(0.5 * ch).max(0.0);
        let (inv_cw, inv_ch) = (1.0 / cw, 1.0 / ch);
        let use_b = !wash_b.is_empty() && t > 0.002;
        let t = t.clamp(0.0, 1.0);
        let solid = alpha >= 0.996;
        // The on-screen width one wash texel covers: the wash is a blur, so a
        // whole block of pixels shares a colour.
        let tex_w = cw / tw as f32;
        let (ytop, ybot) = (xf.cy - xf.hh, xf.cy + xf.hh);
        let y0 = (ytop.floor() as i32).clamp(0, self.h);
        let y1 = (ybot.ceil() as i32).clamp(0, self.h);

        for py in y0..y1 {
            let cov_y = ((py as f32 + 0.5).min(ybot) - (py as f32 - 0.5).max(ytop)).clamp(0.0, 1.0);
            if cov_y <= 0.004 {
                continue;
            }
            let yl = ((py as f32 + 0.5) - ytop).clamp(0.0, ch);
            let ins = corner_inset(yl, ch, radius);
            let v = yl * inv_ch;
            let base = xf.cx - xf.hw + xf.skew * (0.5 - v);
            let (sx0, sx1) = (base + ins, base + cw - ins);
            if sx1 <= sx0 {
                continue;
            }
            let shade = shade_top + (shade_bottom - shade_top) * v;
            let ty = ((v * th as f32) as u32).min(th - 1);
            let tex_row = ty * tw;
            let xlo = ((sx0 - 0.5).floor() as i32).clamp(0, self.w);
            let xhi = ((sx1 + 0.5).floor() as i32).clamp(0, self.w);
            let px_row = py as usize * self.w as usize;
            // Pixels (and hence rows) whose whole extent lies inside the shape.
            let ia = ((sx0 + 0.5).ceil() as i32).clamp(xlo, xhi);
            let ib = ((sx1 - 0.5).floor() as i32 + 1).clamp(xlo, xhi);

            if solid && cov_y >= 0.996 && ib > ia {
                // The pixels this row covers solidly, written a wash texel at a
                // time: each texel owns a run of them, so the run is one fill.
                let dst = &mut self.px[px_row + ia as usize..px_row + ib as usize];
                for k in 0..tw {
                    let a = ((base - 0.5 + k as f32 * tex_w).ceil() as i32).clamp(ia, ib);
                    let b = ((base - 0.5 + (k + 1) as f32 * tex_w).ceil() as i32).clamp(ia, ib);
                    if b <= a {
                        continue;
                    }
                    let col = wash_color(wash_a, wash_b, use_b, t, (tex_row + k) as usize, shade, tint);
                    dst[(a - ia) as usize..(b - ia) as usize]
                        .fill(pack(1.0, col.r, col.g, col.b));
                }
                // The pixels straddling the span's ends still need fractional
                // horizontal coverage.
                for (ra, rb) in [(xlo, ia), (ib, xhi)] {
                    for px in ra..rb {
                        let l = (px as f32 - 0.5).max(sx0);
                        let rr = (px as f32 + 0.5).min(sx1);
                        if rr <= l {
                            continue;
                        }
                        let u = ((px as f32 + 0.5) - base) * inv_cw;
                        let tx = ((u * tw as f32) as u32).min(tw - 1);
                        let col = wash_color(wash_a, wash_b, use_b, t, (tex_row + tx) as usize, shade, tint);
                        self.blend_at(px_row + px as usize, col, (rr - l) * cov_y * alpha);
                    }
                }
                continue;
            }

            // Fading in/out, or an anti-aliased top/bottom row: per pixel.
            let mut cur_tx = u32::MAX;
            let mut col = Color::rgba(0.0, 0.0, 0.0, 1.0);
            for px in xlo..xhi {
                let cov = if px >= ia && px < ib {
                    cov_y
                } else {
                    let l = (px as f32 - 0.5).max(sx0);
                    let rr = (px as f32 + 0.5).min(sx1);
                    if rr <= l {
                        continue;
                    }
                    (rr - l) * cov_y
                };
                if cov <= 0.004 {
                    continue;
                }
                let u = ((px as f32 + 0.5) - base) * inv_cw;
                let tx = ((u * tw as f32) as u32).min(tw - 1);
                if tx != cur_tx {
                    cur_tx = tx;
                    col = wash_color(wash_a, wash_b, use_b, t, (tex_row + tx) as usize, shade, tint);
                }
                self.blend_at(px_row + px as usize, col, cov * alpha);
            }
        }
    }

    /// Span-based texture draw — the hot path. One pass, no scratch coverage
    /// buffer: horizontal anti-aliasing is exact against the sheared edges, and the
    /// flat top/bottom edges get analytic vertical coverage. Fully opaque pixels are
    /// stored rather than blended.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_texture_fast(
        &mut self,
        xf: &Xform,
        radius: f32,
        tex: &[u32],
        tw: u32,
        th: u32,
        opacity: f32,
        dim: f32,
        nearest: bool,
    ) {
        if tex.is_empty() || tw == 0 || th == 0 || opacity <= 0.004 {
            return;
        }
        let cw = 2.0 * xf.hw;
        let ch = 2.0 * xf.hh;
        if cw <= 1.0 || ch <= 1.0 {
            return;
        }
        let radius = radius.min(0.5 * cw).min(0.5 * ch).max(0.0);
        let (inv_cw, inv_ch) = (1.0 / cw, 1.0 / ch);
        let k = 1.0 - 0.62 * dim.clamp(0.0, 1.0);
        let (ytop, ybot) = (xf.cy - xf.hh, xf.cy + xf.hh);
        let y0 = (ytop.floor() as i32).clamp(0, self.h);
        let y1 = (ybot.ceil() as i32).clamp(0, self.h);

        for py in y0..y1 {
            let cov_y = ((py as f32 + 0.5).min(ybot) - (py as f32 - 0.5).max(ytop)).clamp(0.0, 1.0);
            if cov_y <= 0.002 {
                continue;
            }
            let yl = ((py as f32 + 0.5) - ytop).clamp(0.0, ch);
            let ins = corner_inset(yl, ch, radius);
            let v = yl * inv_ch;
            let base = xf.cx - xf.hw + xf.skew * (0.5 - v);
            let (sx0, sx1) = (base + ins, base + cw - ins);
            if sx1 <= sx0 {
                continue;
            }
            let xlo = ((sx0 - 0.5).floor() as i32).clamp(0, self.w);
            let xhi = ((sx1 + 0.5).floor() as i32).clamp(0, self.w);
            let px_row = py as usize * self.w as usize;
            // Pixels whose full width lies inside the sheared span: no per-pixel
            // clipping needed for those.
            let ia = ((sx0 + 0.5).ceil() as i32).clamp(xlo, xhi);
            let ib = ((sx1 - 0.5).floor() as i32 + 1).clamp(xlo, xhi);
            // Point-sampled rows always come from one texture row, so its offset is
            // resolved once here rather than per pixel; the dimming factor becomes a
            // plain integer scale at the same time.
            let row_off = ((v.clamp(0.0, 1.0) * (th - 1) as f32) as u32) * tw;
            let k255 = (k * 255.0 + 0.5) as u32;
            for px in xlo..xhi {
                let cov = if px >= ia && px < ib {
                    cov_y
                } else {
                    let l = (px as f32 - 0.5).max(sx0);
                    let rr = (px as f32 + 0.5).min(sx1);
                    if rr <= l {
                        continue;
                    }
                    (rr - l) * cov_y
                };
                if cov <= 0.002 {
                    continue;
                }
                let u = ((px as f32 + 0.5) - base) * inv_cw;
                let aa = cov * opacity;
                let idx = px_row + px as usize;
                if nearest {
                    // Dim, premultiply by coverage and blend entirely in 8-bit
                    // integers: this is the bulk of the card pixels, and the float
                    // round-trip here showed up clearly in the frame cost. The dim
                    // and the coverage premultiply fold into one scale factor, so
                    // the channels cost a single multiply-shift each.
                    let p = tex[nearest_index(tex, tw, row_off, u)];
                    let a8 = (aa * 255.0 + 0.5) as u32;
                    let m = (k255 * a8 + 127) / 255;
                    let sr = ((((p >> 16) & 0xff) * m + 127) / 255).min(255);
                    let sg = ((((p >> 8) & 0xff) * m + 127) / 255).min(255);
                    let sb = (((p & 0xff) * m + 127) / 255).min(255);
                    if a8 >= 255 {
                        self.px[idx] = 0xff00_0000 | (sr << 16) | (sg << 8) | sb;
                    } else {
                        self.blend_premul(idx, sr, sg, sb, a8);
                    }
                } else {
                    let (tr, tg, tb) = sample(tex, tw, th, u, v);
                    let c = Color::rgba(tr * k, tg * k, tb * k, 1.0);
                    if aa >= 0.996 {
                        self.put_at(idx, c);
                    } else {
                        self.blend_at(idx, c, aa);
                    }
                }
            }
        }
    }

    /// Stores an opaque pixel, skipping all blending work.
    #[inline]
    fn put_at(&mut self, idx: usize, c: Color) {
        self.px[idx] = pack(1.0, c.r, c.g, c.b);
    }

    /// Blends an already-premultiplied 8-bit source pixel over the surface. The
    /// point-sampled card path reaches this with pure integer values, so it never
    /// has to round-trip through 0..1 floats.
    #[inline]
    fn blend_premul(&mut self, idx: usize, sr: u32, sg: u32, sb: u32, a: u32) {
        if a == 0 {
            return;
        }
        let ia = 255 - a;
        let d = self.px[idx];
        let da = (d >> 24) & 0xff;
        let dr = (d >> 16) & 0xff;
        let dg = (d >> 8) & 0xff;
        let db = d & 0xff;
        let out_a = a + (da * ia + 127) / 255;
        let out_r = (sr + (dr * ia + 127) / 255).min(255);
        let out_g = (sg + (dg * ia + 127) / 255).min(255);
        let out_b = (sb + (db * ia + 127) / 255).min(255);
        self.px[idx] = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
    }

    // -- internals ---------------------------------------------------------

    /// Bounding box clamped to the canvas. Both ends are clamped, so an off-screen
    /// shape yields an empty box rather than a negative width (which previously
    /// wrapped into a colossal allocation and killed the process).
    fn bounds(&self, xf: &Xform) -> RectI {
        let (w, h) = (self.w.max(0), self.h.max(0));
        let pad = xf.skew.abs() * 0.5 + 1.0;
        let x0 = ((xf.cx - xf.hw - pad).floor() as i32).clamp(0, w);
        let x1 = ((xf.cx + xf.hw + pad).ceil() as i32).clamp(0, w);
        let y0 = ((xf.cy - xf.hh).floor() as i32).clamp(0, h);
        let y1 = ((xf.cy + xf.hh).ceil() as i32).clamp(0, h);
        RectI { x0: x0.min(x1), y0: y0.min(y1), x1, y1 }
    }

    /// Accumulates sub-scanline coverage into `self.cov` (holes are handled by
    /// letting the inner rounded rect contribute negative coverage).
    fn coverage(&mut self, xf: &Xform, radius: f32, thickness: f32, r: RectI) {
        if r.x1 <= r.x0 || r.y1 <= r.y0 {
            return;
        }
        let bw = (r.x1 - r.x0) as usize;
        let bh = (r.y1 - r.y0) as usize;
        let need = bw * bh;
        if need == 0 {
            return;
        }
        if self.cov.len() < need {
            self.cov.resize(need, 0.0);
        }
        for v in self.cov[..need].iter_mut() {
            *v = 0.0;
        }

        let cw = 2.0 * xf.hw;
        let ch = 2.0 * xf.hh;
        let radius = radius.min(0.5 * cw).min(0.5 * ch).max(0.0);
        let thickness = thickness.clamp(0.0, 0.5 * ch.min(cw));
        let sub = 2usize;
        let wgt = 1.0 / sub as f32;

        for row in 0..bh {
            let py = (r.y0 + row as i32) as f32;
            for k in 0..sub {
                let y = py + (k as f32 + 0.5) * wgt;
                let yl = y - (xf.cy - xf.hh);
                if yl < 0.0 || yl > ch {
                    continue;
                }
                let shear = xf.skew * (0.5 - yl / ch);
                let base = xf.cx - xf.hw + shear;
                for span in spans_at(yl, cw, ch, radius, thickness).iter().flatten() {
                    self.add_span(row, bw, r, base + span.0, base + span.1, wgt);
                }
            }
        }
    }

    fn add_span(&mut self, row: usize, bw: usize, r: RectI, xa: f32, xb: f32, wgt: f32) {
        if xb <= xa {
            return;
        }
        let lo = ((xa - 0.5).floor() as i32).max(r.x0);
        let hi = ((xb + 0.5).floor() as i32).min(r.x1 - 1);
        if hi < lo {
            return;
        }
        let base = row * bw;
        // Only the two pixels straddling the span's ends can be partially covered;
        // everything in between is full, so it is added as one contiguous run that
        // the compiler vectorises. Walking span pixels one at a time is the wrong
        // shape for a fill that is usually hundreds of pixels wide.
        let full_lo = lo.max((xa + 0.5).ceil() as i32);
        let full_hi = hi.min((xb - 0.5).floor() as i32);
        if full_lo <= full_hi {
            let a = base + (full_lo - r.x0) as usize;
            let b = base + (full_hi - r.x0) as usize;
            for v in &mut self.cov[a..=b] {
                *v += wgt;
            }
            for xi in lo..full_lo {
                self.add_partial(base, r, xi, xa, xb, wgt);
            }
            for xi in full_hi + 1..=hi {
                self.add_partial(base, r, xi, xa, xb, wgt);
            }
        } else {
            // Span narrower than a pixel: none of it is fully covered.
            for xi in lo..=hi {
                self.add_partial(base, r, xi, xa, xb, wgt);
            }
        }
    }

    /// Adds the fraction of a single straddling pixel's area that the span covers.
    #[inline]
    fn add_partial(&mut self, base: usize, r: RectI, xi: i32, xa: f32, xb: f32, wgt: f32) {
        let l = (xi as f32 - 0.5).max(xa);
        let rr = (xi as f32 + 0.5).min(xb);
        if rr > l {
            self.cov[base + (xi - r.x0) as usize] += wgt * (rr - l);
        }
    }

    /// Writes or blends one pixel of the row currently accumulated in `self.cov`.
    /// Storing is only right when nothing has been drawn under the shape yet.
    #[inline]
    fn fill_px(&mut self, idx: usize, c: Color, cov: f32, store: bool) {
        if cov <= 0.002 {
            return;
        }
        if store {
            self.px[idx] = premul_px(c, cov);
        } else {
            self.blend_at(idx, c, cov);
        }
    }

    fn blend<F: Fn(f32, f32) -> Color>(&mut self, r: RectI, f: F) {
        if r.x1 <= r.x0 || r.y1 <= r.y0 {
            return;
        }
        let bw = (r.x1 - r.x0) as usize;
        for row in 0..(r.y1 - r.y0) as usize {
            for col in 0..bw {
                let cov = self.cov[row * bw + col];
                if cov <= 0.002 {
                    continue;
                }
                let c = f((r.x0 + col as i32) as f32 + 0.5, (r.y0 + row as i32) as f32 + 0.5);
                let idx = (r.y0 + row as i32) as usize * self.w as usize
                    + (r.x0 + col as i32) as usize;
                self.blend_at(idx, c, cov);
            }
        }
    }

    #[inline]
    fn blend_at(&mut self, idx: usize, c: Color, a: f32) {
        // 8-bit integer maths throughout: this runs for every non-opaque pixel in
        // the frame, and converting the destination to 0..1 floats and back was
        // costing more than the blend itself. Alpha is premultiplied, as the
        // surface format requires.
        let sa = (a * c.a).clamp(0.0, 1.0);
        let sa8 = (sa * 255.0 + 0.5) as u32;
        if sa8 == 0 {
            return;
        }
        let ia = 255 - sa8;
        let d = self.px[idx];
        let da = (d >> 24) & 0xff;
        let dr = (d >> 16) & 0xff;
        let dg = (d >> 8) & 0xff;
        let db = d & 0xff;
        let mix = |s: f32, dc: u32| {
            let src = ((s * sa).clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
            (src + (dc * ia + 127) / 255).min(255)
        };
        let out_a = sa8 + (da * ia + 127) / 255;
        let out_r = mix(c.r, dr);
        let out_g = mix(c.g, dg);
        let out_b = mix(c.b, db);
        self.px[idx] = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
    }
}

/// Fixed-point reciprocal of 255, used instead of a division in the per-pixel
/// paths: float division by a constant is not strength-reduced by the compiler.
const INV255: f32 = 1.0 / 255.0;

/// Texture index for point sampling: `row_off` is `ty * tw`, resolved once per
/// destination row by the caller.
#[inline]
fn nearest_index(tex: &[u32], tw: u32, row_off: u32, u: f32) -> usize {
    let ti = row_off + (u.clamp(0.0, 1.0) * (tw - 1) as f32) as u32;
    (ti as usize).min(tex.len() - 1)
}

#[inline]
fn unpack(p: u32) -> (f32, f32, f32) {
    (
        ((p >> 16) & 0xff) as f32 * INV255,
        ((p >> 8) & 0xff) as f32 * INV255,
        (p & 0xff) as f32 * INV255,
    )
}

#[inline]
fn pack(a: f32, r: f32, g: f32, b: f32) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    (q(a) << 24) | (q(r) << 16) | (q(g) << 8) | q(b)
}

/// Premultiplied BGRA for a colour at coverage `cov`, ready to store into the
/// surface: exactly the bytes `blend_at` would leave if it blended over a zeroed
/// pixel, which is why it may only be used where nothing is underneath.
#[inline]
fn premul_px(c: Color, cov: f32) -> u32 {
    let sa = (cov * c.a).clamp(0.0, 1.0);
    let q = |v: f32| ((v * sa).clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    ((sa * 255.0 + 0.5) as u32) << 24 | q(c.r) << 16 | q(c.g) << 8 | q(c.b)
}

/// One wash texel, cross-faded toward the second wash, shaded down the shelf and
/// tinted with the accent. Shared by both shelf paths so they cannot drift.
#[inline]
#[allow(clippy::too_many_arguments)]
fn wash_color(
    wash_a: &[u32],
    wash_b: &[u32],
    use_b: bool,
    t: f32,
    ti: usize,
    shade: f32,
    tint: Color,
) -> Color {
    let (mut cr, mut cg, mut cb) = unpack(wash_a[ti.min(wash_a.len() - 1)]);
    if use_b {
        let (br, bg, bb) = unpack(wash_b[ti.min(wash_b.len() - 1)]);
        cr += (br - cr) * t;
        cg += (bg - cg) * t;
        cb += (bb - cb) * t;
    }
    Color::rgba(cr * shade, cg * shade, cb * shade, 1.0).mix(tint, 0.30)
}

#[inline]
fn sample(tex: &[u32], tw: u32, th: u32, u: f32, v: f32) -> (f32, f32, f32) {
    let fx = (u.clamp(0.0, 1.0) * (tw - 1) as f32).clamp(0.0, (tw - 1) as f32);
    let fy = (v.clamp(0.0, 1.0) * (th - 1) as f32).clamp(0.0, (th - 1) as f32);
    let x0 = fx.floor() as u32;
    let y0 = fy.floor() as u32;
    let x1 = (x0 + 1).min(tw - 1);
    let y1 = (y0 + 1).min(th - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;

    let p00 = tex[(y0 * tw + x0) as usize];
    let p10 = tex[(y0 * tw + x1) as usize];
    let p01 = tex[(y1 * tw + x0) as usize];
    let p11 = tex[(y1 * tw + x1) as usize];

    let mut out = [0.0f32; 3];
    for c in 0..3 {
        let sh = (2 - c) * 8;
        let a = ((p00 >> sh) & 0xff) as f32;
        let b = ((p10 >> sh) & 0xff) as f32;
        let cc = ((p01 >> sh) & 0xff) as f32;
        let d = ((p11 >> sh) & 0xff) as f32;
        let top = a + (b - a) * tx;
        let bot = cc + (d - cc) * tx;
        out[c] = (top + (bot - top) * ty) * INV255;
    }
    (out[0], out[1], out[2])
}

/// How far the rounded-rect outline is inset from the shape's left/right edge at
/// local height `y` of a shape `h` tall with corner radius `r`.
pub fn corner_inset(y: f32, h: f32, r: f32) -> f32 {
    if r <= 0.0 {
        return 0.0;
    }
    if y < r {
        let d = r - y;
        return r - (r * r - d * d).max(0.0).sqrt();
    }
    if y > h - r {
        let d = y - (h - r);
        return r - (r * r - d * d).max(0.0).sqrt();
    }
    0.0
}

/// Horizontal spans of a rounded rect (or its ring) at local height `y`.
fn spans_at(y: f32, w: f32, h: f32, radius: f32, thickness: f32) -> [Option<(f32, f32)>; 2] {
    if y < 0.0 || y > h {
        return [None, None];
    }
    let ins = corner_inset(y, h, radius);
    let (o0, o1) = (ins, w - ins);
    if thickness <= 0.0 {
        return [Some((o0, o1)), None];
    }
    let t = thickness;
    if y < t || y > h - t {
        return [Some((o0, o1)), None];
    }
    let ci = corner_inset(y - t, h - 2.0 * t, (radius - t).max(0.0));
    let (i0, i1) = (t + ci, w - t - ci);
    if i1 <= i0 {
        return [Some((o0, o1)), None];
    }
    [Some((o0, i0)), Some((i1, o1))]
}
