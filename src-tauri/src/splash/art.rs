//! Startup splash artwork: a pure rasteriser ported from the reviewed standalone Win32
//! demo. `(elapsed_ms, options)` fully determines a frame. There is no global, no clock,
//! no I/O, no thread and no Win32 in this file, so the art is unit-tested headlessly.
//!
//! # Buffer
//! `px` is a top-down 32-bit pixel buffer of `PIXEL_COUNT` values: exactly what a `BI_RGB`
//! 32bpp DIB with a negative `biHeight` holds. Each value is written verbatim — the high
//! byte is alpha, the remaining bytes are the colour channels in the DIB's own order.
//! Every colour below is opaque (alpha `0xFF`), and an opaque colour is already its own
//! premultiplied form, so an animated frame is valid input for `UpdateLayeredWindow` with
//! `AC_SRC_ALPHA`, and a high-contrast frame is valid input for a plain `BitBlt`. Pixels the
//! composition does not touch keep the backdrop value: `0` (transparent) for the animated
//! palette, opaque black for the high-contrast palette.
//!
//! # Guards
//! `draw_frame` and `draw_scene` require a slice of length exactly `PIXEL_COUNT`. Any other
//! length — empty, one element, one short, oversized — draws **nothing** and returns: no
//! partial frame, no panic, never an out-of-bounds index. `Canvas` repeats that protection
//! internally (clipped rectangles, every write through `get_mut`), so even a `Canvas`
//! constructed directly over an odd buffer cannot write outside it.
//!
//! # Scaling
//! Geometry is authored for a 960x600 canvas and is presented at an integer multiple (1x;
//! a 200% display gets 2x) with nearest-neighbour sampling. The rasteriser never
//! interpolates: every primitive is an integer rectangle or a whole glyph pixel, so scaling
//! is pixel repetition and the art stays crisp.

/// Canvas width in pixels.
pub const W: i32 = 960;
/// Canvas height in pixels.
pub const H: i32 = 600;
/// The animation reaches its final composition here. Nothing changes afterwards.
pub const DURATION_MS: u64 = 2800;
/// Required buffer length: `W * H`.
pub const PIXEL_COUNT: usize = (W as usize) * (H as usize);

const INK: u32 = 0xFFE3EAD2;
const GREEN: u32 = 0xFFA6D9AC;
const CYAN: u32 = 0xFF78BBC0;
const ORANGE: u32 = 0xFFE4A376;
const MUTED: u32 = 0xFF607A71;
const BG: u32 = 0xFF101519;
const SHADOW: u32 = 0xFF080D10;

/// Link thickness of a flow path.
const PATH_THICK: i32 = 4;
/// The title card starts growing here.
const CARD_AT_MS: u64 = 100;
/// The footer label appears here; the composition is complete at this point.
const FOOTER_AT_MS: u64 = 2550;
/// Per-node animation budget.
const FLOW_MS: f32 = 650.0;
/// Delay between consecutive flows.
const FLOW_STAGGER_MS: f32 = 180.0;
/// Panel pop-in duration after its path has finished.
const PANEL_MS: f32 = 260.0;

/// Pure inputs that change what is drawn. Both come from the OS in a later phase
/// (`window.rs` probes the system settings); this module only decides what to draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArtOptions {
    /// Draw the final composition immediately, ignoring `elapsed_ms`. The art is still a
    /// pure function of time — the request is simply always for the completed frame.
    pub reduced_motion: bool,
    /// Use the high-contrast palette: an opaque backdrop plus brighter inks, so the art
    /// stays readable when per-pixel alpha is unavailable or unwanted.
    pub high_contrast: bool,
}

impl ArtOptions {
    /// Full animation, standard contrast.
    pub const ANIMATED: Self = Self {
        reduced_motion: false,
        high_contrast: false,
    };
    /// Final composition on the first frame, standard contrast.
    pub const REDUCED_MOTION: Self = Self {
        reduced_motion: true,
        high_contrast: false,
    };
    /// Full animation on an opaque, brighter palette.
    pub const HIGH_CONTRAST: Self = Self {
        reduced_motion: false,
        high_contrast: true,
    };
}

/// The colour set for one frame. Geometry never depends on the palette, so both palettes
/// produce the identical layout; only colours (and the backdrop) differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Primary text.
    pub ink: u32,
    /// Accent for agent-family nodes, frames and the title card.
    pub green: u32,
    /// Accent for the second family and the workflow node.
    pub cyan: u32,
    /// Accent for terminals and the card's corner dots.
    pub orange: u32,
    /// Secondary labels ("NODE").
    pub muted: u32,
    /// Knock-out colour of panel and card interiors.
    pub bg: u32,
    /// Drop shadow behind panels and the card.
    pub shadow: u32,
    /// What `clear` fills the whole buffer with before drawing.
    pub backdrop: u32,
}

impl Palette {
    /// The reviewed demo's colours: transparent backdrop, so the layered window's alpha
    /// shows the desktop behind the art.
    pub const NORMAL: Self = Self {
        ink: INK,
        green: GREEN,
        cyan: CYAN,
        orange: ORANGE,
        muted: MUTED,
        bg: BG,
        shadow: SHADOW,
        backdrop: 0,
    };
    /// High contrast. Exactly four things change:
    /// 1. the backdrop becomes opaque black (`0xFF000000`) instead of transparent, so every
    ///    pixel of the frame has alpha `0xFF` and the art reads correctly even when the
    ///    layered window cannot do per-pixel alpha;
    /// 2. the knock-out colour becomes the same black, so panel and card interiors are pure
    ///    black and the coloured frames sit on maximum contrast;
    /// 3. drop shadows become black, i.e. they are suppressed — a near-black shadow on black
    ///    is invisible and would only soften the frame edge;
    /// 4. the inks brighten: `ink` to pure white, `green`/`cyan`/`orange` to lighter tints
    ///    and `muted` (a dim grey-green normally) to a light grey, because it carries the
    ///    small "NODE" labels and must stay legible on black.
    pub const HIGH_CONTRAST: Self = Self {
        ink: 0xFFFFFFFF,
        green: 0xFF9BFFB0,
        cyan: 0xFF9BEBF2,
        orange: 0xFFFFC48C,
        muted: 0xFFDCE8E0,
        bg: 0xFF000000,
        shadow: 0xFF000000,
        backdrop: 0xFF000000,
    };

    /// The palette an option set selects. `reduced_motion` never changes colours.
    pub fn for_options(options: ArtOptions) -> Self {
        if options.high_contrast {
            Self::HIGH_CONTRAST
        } else {
            Self::NORMAL
        }
    }
}

/// A drawing surface over an existing pixel buffer. All coordinates are canvas pixels and
/// all clipping is integer arithmetic, so nothing here can interpolate or index out of
/// bounds.
pub struct Canvas<'a> {
    px: &'a mut [u32],
    w: i32,
    h: i32,
    pal: Palette,
}

impl<'a> Canvas<'a> {
    /// `None` when the buffer cannot hold a `w` by `h` canvas, or when either dimension is
    /// not positive. Callers treat `None` as "draw nothing": no frame is ever drawn into a
    /// short buffer.
    pub fn new(px: &'a mut [u32], w: i32, h: i32, pal: Palette) -> Option<Self> {
        if w <= 0 || h <= 0 {
            return None;
        }
        let required = (w as usize).checked_mul(h as usize)?;
        if px.len() < required {
            return None;
        }
        Some(Self { px, w, h, pal })
    }

    /// The palette this canvas draws with.
    pub fn palette(&self) -> Palette {
        self.pal
    }

    /// Fill the entire buffer with the palette's backdrop.
    pub fn clear(&mut self) {
        self.px.fill(self.pal.backdrop);
    }

    /// Axis-aligned fill, clipped to the canvas. The write goes through `get_mut`, so an
    /// out-of-range rectangle is skipped instead of panicking.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = x.saturating_add(w).min(self.w);
        let y1 = y.saturating_add(h).min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let stride = self.w as usize;
        for yy in y0..y1 {
            let row = yy as usize * stride;
            let start = row + x0 as usize;
            let end = row + x1 as usize;
            if let Some(span) = self.px.get_mut(start..end) {
                span.fill(color);
            }
        }
    }

    /// Pixel-font text. `center` treats `x` as the centre of the string; `y` is always the
    /// top of the glyph cell, because every glyph is seven rows tall.
    pub fn text(&mut self, t: &str, x: i32, y: i32, scale: i32, color: u32, center: bool) {
        let up = t.to_uppercase();
        let mut cx = if center {
            x - text_width(&up, scale) / 2
        } else {
            x
        };
        for ch in up.chars() {
            let rows = glyph(ch);
            for (ry, row) in rows.iter().enumerate() {
                for (rx, bit) in row.chars().enumerate() {
                    if bit == '1' {
                        let gx = cx + rx as i32 * scale;
                        let gy = y + ry as i32 * scale;
                        self.rect(gx, gy, scale, scale, color);
                    }
                }
            }
            cx += glyph_width(ch, scale);
        }
    }

    /// Horizontal bar from `x1` to `x2` inclusive, `thick` pixels tall.
    pub fn bar_h(&mut self, x1: i32, x2: i32, y: i32, thick: i32, color: u32) {
        let a = x1.min(x2);
        let b = x1.max(x2);
        self.rect(a, y, b - a + thick, thick, color);
    }

    /// Vertical bar from `y1` to `y2` inclusive, `thick` pixels wide.
    pub fn bar_v(&mut self, x: i32, y1: i32, y2: i32, thick: i32, color: u32) {
        let a = y1.min(y2);
        let b = y1.max(y2);
        self.rect(x, a, thick, b - a + thick, color);
    }

    /// An orthogonal polyline drawn up to `progress` of its total manhattan length, with an
    /// arrow head once it is complete. `progress` is clamped to `0.0..=1.0`, so an
    /// under-running or over-running clock cannot draw a partial arrow head.
    pub fn path(&mut self, points: &[(i32, i32)], progress: f32, color: u32) {
        let total: i32 = points
            .windows(2)
            .map(|p| (p[1].0 - p[0].0).abs() + (p[1].1 - p[0].1).abs())
            .sum();
        let mut left = (total as f32 * progress.clamp(0.0, 1.0)) as i32;
        for pair in points.windows(2) {
            if left <= 0 {
                break;
            }
            let (x1, y1) = pair[0];
            let (x2, y2) = pair[1];
            let len = (x2 - x1).abs() + (y2 - y1).abs();
            let take = left.min(len);
            if x2 != x1 {
                let ex = x1 + (x2 - x1).signum() * take;
                self.bar_h(x1, ex, y1, PATH_THICK, color);
            } else {
                let ey = y1 + (y2 - y1).signum() * take;
                self.bar_v(x1, y1, ey, PATH_THICK, color);
            }
            left -= len;
        }
        if progress >= 1.0 {
            let tail = (points.get(points.len().saturating_sub(2)), points.last());
            if let (Some(a), Some(b)) = tail {
                let dx = (b.0 - a.0).signum();
                let dy = (b.1 - a.1).signum();
                for i in 0..3 {
                    let off = i * 4;
                    self.rect(b.0 - dx * off - 2, b.1 - dy * off - 2, 4, 4, color);
                    self.rect(
                        b.0 - dx * off + dy * i * 4 - 2,
                        b.1 - dy * off + dx * i * 4 - 2,
                        4,
                        4,
                        color,
                    );
                    self.rect(
                        b.0 - dx * off - dy * i * 4 - 2,
                        b.1 - dy * off - dx * i * 4 - 2,
                        4,
                        4,
                        color,
                    );
                }
            }
        }
    }

    /// The little pictogram inside a node panel. `kind` is the demo's numbering: 0 is a
    /// family agent, 1 a terminal, 2 the workflow hub. Interior details are knocked out in
    /// the palette's `bg`, which is why they read correctly in both palettes.
    pub fn icon(&mut self, x: i32, y: i32, kind: u8, color: u32) {
        let bg = self.pal.bg;
        if kind == 2 {
            self.rect(x + 61, y + 36, 4, 10, color);
            self.rect(x + 45, y + 43, 36, 3, color);
            self.rect(x + 45, y + 46, 3, 6, color);
            self.rect(x + 78, y + 46, 3, 6, color);
            for (nx, ny) in [(58, 27), (41, 50), (74, 50)] {
                self.rect(x + nx, y + ny, 10, 10, color);
                self.rect(x + nx + 3, y + ny + 3, 4, 4, bg);
            }
        } else if kind == 0 {
            self.rect(x + 48, y + 31, 30, 23, color);
            self.rect(x + 44, y + 38, 4, 12, color);
            self.rect(x + 78, y + 38, 4, 12, color);
            self.rect(x + 54, y + 38, 4, 5, bg);
            self.rect(x + 68, y + 38, 4, 5, bg);
            self.rect(x + 58, y + 48, 10, 2, bg);
            self.rect(x + 61, y + 25, 4, 6, color);
        } else {
            self.rect(x + 43, y + 30, 40, 27, color);
            self.rect(x + 46, y + 33, 34, 21, bg);
            self.rect(x + 51, y + 38, 3, 3, color);
            self.rect(x + 54, y + 41, 3, 3, color);
            self.rect(x + 51, y + 44, 3, 3, color);
            self.rect(x + 62, y + 46, 10, 2, color);
        }
    }

    /// One of the six node panels. `p` is that panel's own progress: it pops in at 32, grows
    /// to 96 and then to the full 126 before its text exists, so a half-grown panel never
    /// shows truncated content.
    pub fn panel(
        &mut self,
        (x, y): (i32, i32),
        p: f32,
        index: usize,
        color: u32,
        lines: [&str; 2],
        kind: u8,
    ) {
        if p <= 0.0 {
            return;
        }
        let pal = self.pal;
        let size = if p < 0.35 {
            32
        } else if p < 0.65 {
            96
        } else {
            126
        };
        let px = x + (126 - size) / 2;
        let py = y + (126 - size) / 2;
        self.rect(px + 6, py + 6, size, size, pal.shadow);
        self.rect(px, py, size, size, color);
        self.rect(px + 3, py + 3, size - 6, size - 6, pal.bg);
        if size != 126 {
            return;
        }
        self.rect(px + 3, py + 3, 120, 15, color);
        self.rect(px + 111, py + 7, 7, 7, pal.bg);
        self.text(&format!("0{}", index + 1), px + 9, py + 7, 1, pal.bg, false);
        self.icon(px, py, kind, color);
        for (i, line) in lines.iter().enumerate() {
            self.text(line, px + 63, py + 69 + i as i32 * 20, 2, pal.ink, true);
        }
        self.rect(px + 12, py + 112, 4, 4, color);
        self.text("NODE", px + 23, py + 111, 1, pal.muted, false);
    }

    /// The title card: a shadowed plate that pops in at `CARD_AT_MS` and overshoots slightly
    /// (p = 1.06) before settling at its final 550x98 size.
    pub fn title_card(&mut self, elapsed: u64) {
        if elapsed < CARD_AT_MS {
            return;
        }
        let pal = self.pal;
        let p = if elapsed < 210 {
            0.75
        } else if elapsed < 310 {
            1.06
        } else {
            1.0
        };
        let w = (550.0 * p) as i32;
        let h = (98.0 * p) as i32;
        let x = 480 - w / 2;
        let y = 300 - h / 2;
        self.rect(x + 7, y + 7, w, h, pal.shadow);
        self.rect(x, y, w, h, pal.green);
        self.rect(x + 3, y + 3, w - 6, h - 6, pal.bg);
        self.rect(x + 10, y + 10, 4, 4, pal.orange);
        self.rect(x + w - 14, y + h - 14, 4, 4, pal.orange);
        let ts = if p < 0.95 { 4 } else { 5 };
        self.text("AGENT WORKBENCH", 480, 300 - (7 * ts) / 2, ts, pal.ink, true);
    }

    /// The small footer label, plus its leading dot.
    pub fn footer(&mut self, elapsed: u64) {
        if elapsed < FOOTER_AT_MS {
            return;
        }
        let green = self.pal.green;
        self.text("NATIVE", 480, 562, 1, green, true);
        self.rect(425, 560, 3, 3, green);
    }
}

// Flow paths, in the demo's order. Each panel sits one slot after its path.
const P0: &[(i32, i32)] = &[(330, 290), (272, 290), (272, 157), (218, 157)];
const P1: &[(i32, i32)] = &[(480, 280), (480, 158)];
const P2: &[(i32, i32)] = &[(630, 290), (688, 290), (688, 157), (742, 157)];
const P3: &[(i32, i32)] = &[(380, 330), (332, 330), (332, 473), (282, 473)];
const P4: &[(i32, i32)] = &[(580, 330), (628, 330), (628, 473), (678, 473)];
const P5: &[(i32, i32)] = &[(480, 330), (480, 410)];

/// Draw one frame into `px` with the animated, standard-contrast composition.
///
/// Equivalent to `draw_frame(px, elapsed_ms, ArtOptions::ANIMATED)`; the buffer rules and
/// the "wrong size draws nothing" guard live on `draw_frame`.
pub fn draw_scene(px: &mut [u32], elapsed_ms: u64) {
    draw_frame(px, elapsed_ms, ArtOptions::ANIMATED);
}

/// Draw one frame. This is all of the art: a pure function of `elapsed_ms` and `options`,
/// with no state carried between calls.
///
/// * `elapsed_ms` is clamped to `DURATION_MS`, so a late tick renders the final frame
///   instead of drifting and every tick after the end is byte-identical.
/// * `ArtOptions::reduced_motion` renders the final composition regardless of `elapsed_ms`:
///   the very first frame already equals the last frame of the animation.
/// * `ArtOptions::high_contrast` selects `Palette::HIGH_CONTRAST` — an opaque backdrop,
///   brighter inks and suppressed shadows; see that constant for the exact changes.
/// * A slice whose length is not exactly `PIXEL_COUNT` is left untouched: nothing is drawn
///   and nothing panics.
pub fn draw_frame(px: &mut [u32], elapsed_ms: u64, options: ArtOptions) {
    if px.len() != PIXEL_COUNT {
        return;
    }
    let elapsed = if options.reduced_motion {
        DURATION_MS
    } else {
        elapsed_ms.min(DURATION_MS)
    };
    let Some(mut canvas) = Canvas::new(px, W, H, Palette::for_options(options)) else {
        return;
    };
    canvas.clear();
    draw_scene_on(&mut canvas, elapsed);
}

/// One of the scene's flows: the path it draws and the panel it grows into.
struct Flow<'a> {
    pts: &'a [(i32, i32)],
    at: (i32, i32),
    color: u32,
    label: [&'a str; 2],
    kind: u8,
}

/// The composition itself: six staggered flows, the title card, then the footer label.
fn draw_scene_on(canvas: &mut Canvas<'_>, elapsed: u64) {
    let pal = canvas.palette();
    // Each flow: its path, where its panel sits, its colour, the panel's two lines and the panel's art.
    let flows: [Flow<'_>; 6] = [
        Flow { pts: P0, at: (92, 94), color: pal.green, label: ["AGENT", "FAMILY 1"], kind: 0 },
        Flow { pts: P1, at: (417, 32), color: pal.ink, label: ["MODEL", "HARNESS 1"], kind: 0 },
        Flow { pts: P2, at: (742, 94), color: pal.cyan, label: ["AGENT", "FAMILY 2"], kind: 0 },
        Flow { pts: P3, at: (156, 410), color: pal.orange, label: ["TERMINAL", "1"], kind: 1 },
        Flow { pts: P4, at: (678, 410), color: pal.green, label: ["TERMINAL", "2"], kind: 1 },
        Flow { pts: P5, at: (417, 410), color: pal.cyan, label: ["AGENT", "WORKFLOW"], kind: 2 },
    ];
    let e = elapsed as f32;
    for (i, flow) in flows.iter().enumerate() {
        let start = FLOW_MS + i as f32 * FLOW_STAGGER_MS;
        canvas.path(flow.pts, ((e - start) / FLOW_MS).clamp(0.0, 1.0), flow.color);
        let panel_p = ((e - start - FLOW_MS) / PANEL_MS).clamp(0.0, 1.0);
        canvas.panel(flow.at, panel_p, i, flow.color, flow.label, flow.kind);
    }
    canvas.title_card(elapsed);
    canvas.footer(elapsed);
}

/// Order-sensitive checksum of a whole frame. The golden test pins a few timestamps with
/// it; it is also a cheap "did the art change?" probe for later phases. An all-zero buffer
/// checksums to `0`.
pub fn checksum(px: &[u32]) -> u64 {
    let mut acc: u64 = 0;
    for &value in px {
        acc = acc.wrapping_mul(0x0000_0100_0000_01b3).wrapping_add(value as u64);
    }
    acc
}

/// Width of one glyph cell, including the single-pixel gap after it.
fn glyph_width(c: char, scale: i32) -> i32 {
    let rows = glyph(c);
    (rows.first().map_or(0, |row| row.len()) as i32 + 1) * scale
}

/// Width of a whole string minus the trailing gap of the last glyph (the demo's rule, and
/// the reason the centring arithmetic is exactly reproducible).
pub fn text_width(t: &str, scale: i32) -> i32 {
    let mut total = 0;
    for c in t.to_uppercase().chars() {
        total += glyph_width(c, scale);
    }
    total - scale
}

/// The reviewed prototype's pixel font, kept verbatim so the art matches the demo. Every
/// glyph is seven rows. Unknown characters render as a blank cell of the placeholder width,
/// and the table intentionally has no `7`, `8` or `9`: the only digits drawn are the `0N`
/// node labels, `01` through `06`.
fn glyph(c: char) -> &'static [&'static str] {
    match c {
        '0' => &["111", "101", "101", "101", "101", "101", "111"],
        '1' => &["010", "110", "010", "010", "010", "010", "111"],
        '2' => &["11110", "00001", "00001", "01110", "10000", "10000", "11111"],
        '3' => &["11110", "00001", "00001", "01110", "00001", "00001", "11110"],
        '4' => &["10010", "10010", "10010", "11111", "00010", "00010", "00010"],
        '5' => &["11111", "10000", "10000", "11110", "00001", "00001", "11110"],
        '6' => &["01110", "10000", "10000", "11110", "10001", "10001", "01110"],
        'A' => &["01110", "10001", "10001", "11111", "10001", "10001", "10001"],
        'B' => &["11110", "10001", "10001", "11110", "10001", "10001", "11110"],
        'C' => &["01111", "10000", "10000", "10000", "10000", "10000", "01111"],
        'D' => &["11110", "10001", "10001", "10001", "10001", "10001", "11110"],
        'E' => &["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
        'F' => &["11111", "10000", "10000", "11110", "10000", "10000", "10000"],
        'G' => &["01111", "10000", "10000", "10111", "10001", "10001", "01110"],
        'H' => &["10001", "10001", "10001", "11111", "10001", "10001", "10001"],
        'I' => &["111", "010", "010", "010", "010", "010", "111"],
        'J' => &["00111", "00010", "00010", "00010", "10010", "10010", "01100"],
        'K' => &["10001", "10010", "10100", "11000", "10100", "10010", "10001"],
        'L' => &["10000", "10000", "10000", "10000", "10000", "10000", "11111"],
        'M' => &["10001", "11011", "10101", "10101", "10001", "10001", "10001"],
        'N' => &["10001", "11001", "10101", "10011", "10001", "10001", "10001"],
        'O' => &["01110", "10001", "10001", "10001", "10001", "10001", "01110"],
        'P' => &["11110", "10001", "10001", "11110", "10000", "10000", "10000"],
        'Q' => &["01110", "10001", "10001", "10001", "10101", "10010", "01101"],
        'R' => &["11110", "10001", "10001", "11110", "10100", "10010", "10001"],
        'S' => &["01111", "10000", "10000", "01110", "00001", "00001", "11110"],
        'T' => &["11111", "00100", "00100", "00100", "00100", "00100", "00100"],
        'U' => &["10001", "10001", "10001", "10001", "10001", "10001", "01110"],
        'V' => &["10001", "10001", "10001", "10001", "10001", "01010", "00100"],
        'W' => &["10001", "10001", "10001", "10101", "10101", "10101", "01010"],
        'X' => &["10001", "10001", "01010", "00100", "01010", "10001", "10001"],
        'Y' => &["10001", "10001", "01010", "00100", "00100", "00100", "00100"],
        'Z' => &["11111", "00001", "00010", "00100", "01000", "10000", "11111"],
        '/' => &["00001", "00001", "00010", "00100", "01000", "10000", "10000"],
        ' ' => &["000", "000", "000", "000", "000", "000", "000"],
        _ => &["000", "000", "000", "000", "000", "000", "000"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(elapsed: u64, options: ArtOptions) -> Vec<u32> {
        let mut px = vec![0u32; PIXEL_COUNT];
        draw_frame(&mut px, elapsed, options);
        px
    }

    fn px_at(f: &[u32], x: i32, y: i32) -> u32 {
        f[y as usize * W as usize + x as usize]
    }

    fn lit_count(f: &[u32]) -> usize {
        f.iter().filter(|&&p| p != 0).count()
    }

    /// Checksums of the reviewed composition, for deliberate change control. `checksum` is
    /// order-sensitive, so any change to the geometry, the palette or the glyph table moves
    /// these numbers. Every entry is pinned, so a change fails the test loudly and names the
    /// new value.
    ///
    /// Blessed from a real run on 2026-10-23 (this machine, this toolchain, this code).
    /// Changing the art requires re-blessing deliberately: render the frame, read the new
    /// checksum off the failure message, and edit this table by hand. Never re-bless to
    /// silence a failure you have not explained.
    const GOLDEN: [(u64, u64); 4] = [
        (0, 0),
        (100, 0x7c73_73bb_71c2_526c),
        (2550, 0x07a0_18cb_15bb_7205),
        (2800, 0x07a0_18cb_15bb_7205),
    ];

    #[test]
    fn the_same_elapsed_time_always_renders_the_same_bytes() {
        for elapsed in [0, 100, 650, 1000, 1400, 2200, 2550, 2800] {
            let first = frame(elapsed, ArtOptions::ANIMATED);
            let second = frame(elapsed, ArtOptions::ANIMATED);
            assert_eq!(first, second, "t={elapsed} ms must be deterministic");
        }
        let once = frame(900, ArtOptions::REDUCED_MOTION);
        let twice = frame(0, ArtOptions::REDUCED_MOTION);
        assert_eq!(once, twice, "reduced motion ignores the time it is given");
    }

    #[test]
    fn the_frame_checksums_are_unchanged() {
        for (elapsed, expected) in GOLDEN {
            let got = checksum(&frame(elapsed, ArtOptions::ANIMATED));
            assert_eq!(
                got, expected,
                "t={elapsed} ms: the artwork changed. If that was deliberate, \
                 update this GOLDEN entry to {got:#018x}"
            );
        }
    }

    #[test]
    fn the_first_frame_is_empty_and_the_composition_grows_from_there() {
        let zero = frame(0, ArtOptions::ANIMATED);
        assert!(
            lit_count(&zero) == 0,
            "at t=0 every path is at 0 progress, every panel is gated off and no card is \
             due yet, so the frame is the bare backdrop"
        );
        let card = frame(100, ArtOptions::ANIMATED);
        assert!(lit_count(&card) > 0, "the title card is due at 100 ms");
        let flow = frame(700, ArtOptions::ANIMATED);
        assert!(lit_count(&flow) > 0, "the first flow is running at 700 ms");
    }

    #[test]
    fn the_final_frame_shows_the_title_card() {
        let f = frame(DURATION_MS, ArtOptions::ANIMATED);
        // Settled size: 550x98 centred on (480, 300) -> origin (205, 251). The plate covers
        // 205..=754 by 251..=348 and its knocked-out interior 208..=751 by 254..=345. The
        // drop shadow is the same 550x98 offset by (7, 7), so it covers 212..=761 by
        // 258..=355; the plate hides all but a 7 px fringe of it on the right and bottom.
        assert_eq!(px_at(&f, 205, 251), GREEN, "top-left corner of the card frame");
        assert_eq!(px_at(&f, 206, 252), GREEN, "the frame is three pixels thick");
        assert_eq!(px_at(&f, 209, 255), BG, "inside the frame the interior is knocked out");
        // The accents and the bottom-right border sit over the shadow's footprint; they are
        // drawn after it, so the shadow must not show through them.
        assert_eq!(px_at(&f, 215, 261), ORANGE, "top-left corner accent");
        assert_eq!(px_at(&f, 742, 336), ORANGE, "bottom-right corner accent");
        assert_eq!(px_at(&f, 753, 347), GREEN, "bottom-right border");
        assert_eq!(px_at(&f, 755, 350), SHADOW, "the shadow's fringe past that corner");
        assert_eq!(px_at(&f, 204, 251), 0, "one pixel left of the card is untouched");
        assert_eq!(
            px_at(&f, 762, 356),
            0,
            "just past the bottom-right corner of the card and of its shadow"
        );
        // "AGENT WORKBENCH" at scale 5 is 435 px wide, so it starts at 480 - 217 = 263 and
        // its glyph rows begin at 300 - 17 = 283.
        assert_eq!(px_at(&f, 263, 283), BG, "left of the first glyph stroke");
        assert_eq!(px_at(&f, 268, 283), INK, "the 'A' of the title, in ink");
    }

    #[test]
    fn panels_and_footer_appear_only_when_their_time_arrives() {
        let early = frame(300, ArtOptions::ANIMATED);
        assert_eq!(px_at(&early, 92, 94), 0, "no panel frame at 300 ms");
        assert_eq!(px_at(&early, 425, 560), 0, "no footer dot at 300 ms");
        let done = frame(DURATION_MS, ArtOptions::ANIMATED);
        assert_eq!(px_at(&done, 92, 94), GREEN, "panel 1 frame corner");
        assert_eq!(px_at(&done, 217, 219), GREEN, "panel 1 frame bottom-right corner");
        assert_eq!(px_at(&done, 425, 560), GREEN, "footer dot");
        assert_eq!(px_at(&done, 464, 562), GREEN, "the 'N' that starts the centred label");
        assert_eq!(px_at(&done, 465, 562), 0, "its first row is 10001");
    }

    #[test]
    fn the_composition_is_static_from_the_last_animation_event() {
        let settled = frame(2550, ArtOptions::ANIMATED);
        let end = frame(DURATION_MS, ArtOptions::ANIMATED);
        assert_eq!(settled, end, "nothing moves between 2550 ms and the end");
        let late = frame(60_000, ArtOptions::ANIMATED);
        assert_eq!(settled, late, "elapsed is clamped, so a late tick cannot drift");
    }

    #[test]
    fn reduced_motion_draws_the_final_composition_immediately() {
        let final_frame = frame(DURATION_MS, ArtOptions::ANIMATED);
        let at_zero = frame(0, ArtOptions::REDUCED_MOTION);
        assert_eq!(at_zero, final_frame, "t=0 with reduced motion is the last frame");
        let mid = frame(1400, ArtOptions::REDUCED_MOTION);
        assert_eq!(mid, final_frame, "and no elapsed time changes it");
    }

    #[test]
    fn high_contrast_is_fully_opaque_and_differs_from_the_animated_palette() {
        let animated = frame(DURATION_MS, ArtOptions::ANIMATED);
        let high = frame(DURATION_MS, ArtOptions::HIGH_CONTRAST);
        assert!(
            high.iter().all(|&p| p >> 24 == 0xFF),
            "every pixel is opaque, so the art reads without per-pixel alpha"
        );
        assert!(lit_count(&animated) > 0, "the animated frame is not empty either");
        assert!(
            animated.iter().any(|&p| p >> 24 == 0),
            "the animated palette relies on transparency"
        );
        assert_ne!(high, animated, "the palettes must differ");
        // The layout is identical between palettes; only colours change.
        assert_eq!(px_at(&high, 205, 251), Palette::HIGH_CONTRAST.green, "same card corner");
        assert_eq!(px_at(&high, 204, 251), Palette::HIGH_CONTRAST.backdrop, "same card edge");
        assert_eq!(px_at(&high, 464, 562), Palette::HIGH_CONTRAST.green, "same footer label");
        let mut both = ArtOptions::REDUCED_MOTION;
        both.high_contrast = true;
        assert_eq!(frame(0, both), high, "reduced motion on top of high contrast agrees");
    }

    #[test]
    fn a_wrong_sized_buffer_draws_nothing_and_never_panics() {
        let mut empty: [u32; 0] = [];
        draw_frame(&mut empty, DURATION_MS, ArtOptions::ANIMATED);
        draw_scene(&mut empty, DURATION_MS);
        let mut one = [0u32; 1];
        draw_frame(&mut one, DURATION_MS, ArtOptions::HIGH_CONTRAST);
        draw_scene(&mut one, DURATION_MS);
        assert_eq!(one, [0], "a one-element buffer is left untouched");
        let mut short = vec![0u32; PIXEL_COUNT - 1];
        draw_frame(&mut short, DURATION_MS, ArtOptions::ANIMATED);
        assert_eq!(lit_count(&short), 0, "an almost-full buffer is not partially drawn");
        let mut long = vec![0u32; PIXEL_COUNT + 1];
        draw_frame(&mut long, DURATION_MS, ArtOptions::ANIMATED);
        assert_eq!(lit_count(&long), 0, "a mis-sized buffer is rejected, not clipped");
    }

    #[test]
    fn the_canvas_refuses_a_buffer_that_cannot_hold_the_frame() {
        let mut short = vec![0u32; PIXEL_COUNT - 1];
        assert!(Canvas::new(&mut short, W, H, Palette::NORMAL).is_none(), "one pixel short");
        let mut flat = vec![0u32; PIXEL_COUNT];
        assert!(Canvas::new(&mut flat, 0, H, Palette::NORMAL).is_none(), "zero width");
        let mut upside_down = vec![0u32; PIXEL_COUNT];
        let neg = Canvas::new(&mut upside_down, W, -1, Palette::NORMAL);
        assert!(neg.is_none(), "negative height");
        let mut exact = vec![0u32; PIXEL_COUNT];
        assert!(Canvas::new(&mut exact, W, H, Palette::NORMAL).is_some(), "the real frame");
    }

    #[test]
    fn rectangles_are_clipped_instead_of_panicking() {
        let mut px = vec![0u32; PIXEL_COUNT];
        // Each canvas borrows the buffer, so the buffer is inspected between blocks.
        {
            let Some(mut canvas) = Canvas::new(&mut px, W, H, Palette::NORMAL) else {
                unreachable!()
            };
            canvas.rect(-100, -100, 10, 10, INK);
            canvas.rect(W + 10, H + 10, 4, 4, INK);
            canvas.rect(i32::MAX, i32::MIN, i32::MAX, i32::MAX, INK);
            canvas.rect(10, 10, -5, -5, INK);
        }
        assert_eq!(lit_count(&px), 0, "every one of those rectangles is off-canvas");
        {
            let Some(mut canvas) = Canvas::new(&mut px, W, H, Palette::NORMAL) else {
                unreachable!()
            };
            canvas.rect(W - 2, H - 2, 500, 500, INK);
        }
        assert_eq!(lit_count(&px), 4, "the oversized rectangle keeps only its clipped corner");
        assert_eq!(px_at(&px, W - 1, H - 1), INK, "and that corner is where it belongs");
        let mut small = vec![0u32; 4];
        {
            let Some(mut tiny) = Canvas::new(&mut small, 2, 2, Palette::NORMAL) else {
                unreachable!()
            };
            tiny.rect(1, 1, 5, 5, INK);
        }
        assert_eq!(small, [0, 0, 0, INK], "clipped to the single bottom-right pixel");
    }
}
