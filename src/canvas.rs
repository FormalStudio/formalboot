use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};

/// 0x00RRGGBB, the layout of a little-endian XRGB8888 pixel.
pub type Colour = u32;

pub fn parse_colour(hex: &str) -> Option<Colour> {
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok()).flatten()
}

fn mix(under: Colour, over: Colour, a: u32) -> Colour {
    let ch = |shift: u32| {
        let (u, o) = ((under >> shift) & 0xff, (over >> shift) & 0xff);
        ((u * (255 - a) + o * a + 127) / 255) << shift
    };
    ch(16) | ch(8) | ch(0)
}

pub struct Image {
    pub w: usize,
    pub h: usize,
    /// RGBA, row major.
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; reader.output_buffer_size().ok_or("png too large")?];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let px = &buf[..info.buffer_size()];
        let rgba = match info.color_type {
            png::ColorType::Rgba => px.to_vec(),
            png::ColorType::Rgb => px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::GrayscaleAlpha => {
                px.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
            }
            png::ColorType::Indexed => return Err("indexed png left unexpanded".into()),
        };
        Ok(Self { w: info.width as usize, h: info.height as usize, rgba })
    }
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<Colour>,
}

impl Canvas {
    pub fn new(w: usize, h: usize, fill: Colour) -> Self {
        Self { w, h, px: vec![fill; w * h] }
    }

    pub fn clear(&mut self, colour: Colour) {
        self.px.fill(colour);
    }

    fn blend(&mut self, x: i32, y: i32, colour: Colour, a: u32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a == 0 {
            return;
        }
        let p = &mut self.px[y as usize * self.w + x as usize];
        *p = if a >= 255 { colour } else { mix(*p, colour, a) };
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, colour: Colour, opacity: f32) {
        let a = (opacity.clamp(0.0, 1.0) * 255.0) as u32;
        for yy in y.max(0)..(y + h).min(self.h as i32) {
            for xx in x.max(0)..(x + w).min(self.w as i32) {
                self.blend(xx, yy, colour, a);
            }
        }
    }

    /// Point scaled by a whole factor, so pixel art stays on whole device pixels.
    pub fn image(&mut self, img: &Image, x: i32, y: i32, scale: usize, opacity: f32) {
        let o = opacity.clamp(0.0, 1.0);
        for sy in 0..img.h {
            for sx in 0..img.w {
                let p = &img.rgba[(sy * img.w + sx) * 4..][..4];
                let a = (p[3] as f32 * o) as u32;
                if a == 0 {
                    continue;
                }
                let colour = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
                for dy in 0..scale {
                    for dx in 0..scale {
                        self.blend(x + (sx * scale + dx) as i32, y + (sy * scale + dy) as i32, colour, a);
                    }
                }
            }
        }
    }

    pub fn text(&mut self, face: &mut Face, s: &str, (x, baseline): (i32, i32), px: f32, colour: Colour, opacity: f32) {
        let o = opacity.clamp(0.0, 1.0);
        let mut pen = x as f32;
        for c in s.chars() {
            let (m, cover) = face.glyph(c, px);
            let (gx, gy) = (pen.round() as i32 + m.xmin, baseline - m.ymin - m.height as i32);
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = (cover[row * m.width + col] as f32 * o) as u32;
                    self.blend(gx + col as i32, gy + row as i32, colour, a);
                }
            }
            pen += m.advance_width;
        }
    }
}

pub struct Face {
    font: Font,
    cache: HashMap<(char, u32), (Metrics, Vec<u8>)>,
}

impl Face {
    pub fn new() -> Self {
        let font =
            Font::from_bytes(include_bytes!("../assets/GeistMono-Regular.ttf").as_slice(), FontSettings::default())
                .expect("the embedded font parses");
        Self { font, cache: HashMap::new() }
    }

    fn glyph(&mut self, c: char, px: f32) -> &(Metrics, Vec<u8>) {
        self.cache.entry((c, px.to_bits())).or_insert_with(|| self.font.rasterize(c, px))
    }

    /// The face is monospaced, so one advance measures every string.
    pub fn advance(&self, px: f32) -> f32 {
        self.font.metrics('0', px).advance_width
    }
}
