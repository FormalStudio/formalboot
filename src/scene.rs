use std::path::Path;

use crate::canvas::{Canvas, Colour, Face, Image, parse_colour};
use crate::jobs::{Failure, Row};

pub struct Theme {
    pub fg: Colour,
    pub muted: Colour,
    pub accent: Colour,
    pub error: Colour,
    pub track: Colour,
    pub backdrop: Colour,
    pub logo: Image,
    pub wordmark: Image,
}

impl Theme {
    /// `dir` holds logo.png, wordmark.png and theme.env, whose `key=value` lines name the colours.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let read = |name: &str| std::fs::read(dir.join(name)).map_err(|e| format!("{}: {e}", dir.join(name).display()));
        let env = String::from_utf8(read("theme.env")?).map_err(|e| format!("theme.env: {e}"))?;
        let colour = |key: &str| {
            env.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .and_then(|v| parse_colour(v.trim()))
                .ok_or(format!("theme.env: {key} is not a rrggbb colour"))
        };
        Ok(Self {
            fg: colour("fg")?,
            muted: colour("muted")?,
            accent: colour("fill")?,
            error: colour("error")?,
            track: colour("track")?,
            backdrop: colour("backdrop")?,
            logo: Image::decode(&read("logo.png")?)?,
            wordmark: Image::decode(&read("wordmark.png")?)?,
        })
    }
}

/// The bar's cells; the last one fills when the boot is done.
pub const STEPS: u32 = 13;

/// The square the logo, the name and the bar are laid out in, in layout pixels. The greeter
/// repeats the finished frame on a canvas this size.
pub const BLOCK: usize = 400;

pub struct Scene<'a> {
    pub shutdown: bool,
    pub steps: u32,
    pub rows: &'a [Row],
    /// Seconds each row's job has run, by row.
    pub elapsed: &'a [Option<f32>],
    pub slowest: &'a [(String, f32)],
    pub failed: &'a [Failure],
    /// Advances as time passes; drives the running marker.
    pub tick: usize,
    pub opacity: f32,
    /// On top of `opacity`, for the lists alone: they fade out ahead of the finished frame.
    pub lists: f32,
}

/// Layout pixels to device pixels. Whole, so the pixel art is never interpolated.
pub fn scale_for(height: usize) -> usize {
    (height / 500).clamp(1, 4)
}

fn fit(s: &str, chars: usize) -> String {
    if s.chars().count() <= chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn seconds(secs: f32) -> String {
    if secs >= 60.0 { format!("{}m{:02}s", secs as u32 / 60, secs as u32 % 60) } else { format!("{secs:.1}s") }
}

pub fn draw(canvas: &mut Canvas, face: &mut Face, theme: &Theme, scene: &Scene) {
    canvas.clear(theme.backdrop);
    let s = scale_for(canvas.h) as i32;
    let (cx, cy) = (canvas.w as i32 / 2, canvas.h as i32 / 2);
    let o = scene.opacity;

    let logo = &theme.logo;
    canvas.image(logo, cx - logo.w as i32 * s, cy - 66 * s - logo.h as i32 * s, 2 * s as usize, o);
    let wordmark = &theme.wordmark;
    canvas.image(wordmark, cx - wordmark.w as i32 * s / 2, cy + 2 * s, s as usize, o);
    if !scene.shutdown {
        canvas.rect(cx - 81 * s, cy + 43 * s, 162 * s, 12 * s, theme.track, o);
        canvas.rect(cx - 78 * s, cy + 46 * s, scene.steps.min(STEPS) as i32 * 12 * s, 6 * s, theme.accent, o);
    }

    let o = o * scene.lists;
    if o <= 0.0 {
        return;
    }
    let px = 11.0 * s as f32;
    let adv = face.advance(px);
    let line = 15 * s;
    let top = cy + 75 * s;
    let lines = ((canvas.h as i32 - 24 * s - top) / line).max(0) as usize;
    let gap = 32 * s;
    let two = canvas.w as i32 >= 2 * 380 * s + gap + 48 * s;
    let width = if two { 380 * s } else { (canvas.w as i32 - 48 * s).min(380 * s) };
    let left = if two { cx - gap / 2 - width } else { cx - width / 2 };
    let chars = |w: i32| (w as f32 / adv).floor().max(0.0) as usize;
    let baseline = |n: usize| top + n as i32 * line + 11 * s;
    let stroke = (s / 2).max(1);

    // The left column: what failed, then the tree of pending jobs.
    let mut n = 0;
    for failure in scene.failed {
        if n >= lines {
            break;
        }
        let text = fit(&format!("{} failed", crate::jobs::label(&failure.unit)), chars(width));
        canvas.text(face, &text, (left, baseline(n)), px, theme.error, o);
        n += 1;
        for l in &failure.lines {
            if n >= lines {
                break;
            }
            let indent = (2.0 * adv) as i32;
            canvas.text(face, &fit(l, chars(width - indent)), (left + indent, baseline(n)), px, theme.muted, o);
            n += 1;
        }
    }
    let room = lines.saturating_sub(n);
    let shown = if scene.rows.len() > room { room.saturating_sub(1) } else { scene.rows.len() };
    for (row, elapsed) in scene.rows.iter().zip(scene.elapsed).take(shown) {
        let y = top + n as i32 * line;
        let step = (2.0 * adv) as i32;
        for (level, &on) in row.guides.iter().enumerate() {
            let x = left + level as i32 * step + (adv / 2.0) as i32;
            let own = level + 1 == row.depth;
            if on || (own && !row.last) {
                canvas.rect(x, y, stroke, line, theme.track, o);
            } else if own {
                canvas.rect(x, y, stroke, line / 2, theme.track, o);
            }
            if own {
                canvas.rect(x, y + line / 2, step - (adv / 2.0) as i32 - 2 * s, stroke, theme.track, o);
            }
        }
        let mut x = left + row.depth as i32 * step;
        if row.running {
            // A two by two block with one lit cell going round.
            let cell = 3 * s;
            let (bx, by) = (x, y + (line - 2 * cell) / 2);
            for (i, (dx, dy)) in [(0, 0), (1, 0), (1, 1), (0, 1)].into_iter().enumerate() {
                let colour = if scene.tick % 4 == i { theme.accent } else { theme.track };
                canvas.rect(bx + dx * cell, by + dy * cell, cell, cell, colour, o);
            }
            x += 2 * cell + 4 * s;
        }
        let time = elapsed.filter(|_| row.running).map(seconds).unwrap_or_default();
        let time_w = (time.chars().count() as f32 * adv) as i32;
        let room = left + width - x - if time.is_empty() { 0 } else { time_w + adv as i32 };
        let colour = if row.running { theme.fg } else { theme.muted };
        canvas.text(face, &fit(&row.label, chars(room)), (x, baseline(n)), px, colour, o);
        canvas.text(face, &time, (left + width - time_w, baseline(n)), px, theme.muted, o);
        n += 1;
    }
    let cut = scene.rows.len() - shown;
    if cut > 0 && n < lines {
        canvas.text(face, &format!("+{cut} more"), (left, baseline(n)), px, theme.muted, o);
        n += 1;
    }

    // The slowest finished units, each on a bar as long as its share of the slowest one. Beside
    // the tree where two columns fit, else under it.
    let (x, mut n) = if two { (cx + gap / 2, 0) } else { (left, n + 1) };
    let longest = scene.slowest.first().map_or(1.0, |(_, secs)| *secs);
    for (unit, secs) in scene.slowest {
        if n >= lines {
            break;
        }
        let y = top + n as i32 * line;
        let bar = ((width as f32 * secs / longest) as i32).max(stroke);
        canvas.rect(x, y + s, bar, line - 2 * s, theme.track, o);
        let time = seconds(*secs);
        let time_w = (time.chars().count() as f32 * adv) as i32;
        let pad = 4 * s;
        let name = fit(unit, chars(width - time_w - 2 * pad - adv as i32));
        canvas.text(face, &name, (x + pad, baseline(n)), px, theme.muted, o);
        canvas.text(face, &time, (x + width - time_w - pad, baseline(n)), px, theme.muted, o);
        n += 1;
    }
}
