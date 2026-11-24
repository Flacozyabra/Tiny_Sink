//! Dynamic 32-bit ARGB tray icon renderer for headphones/speakers with compact volume digits.

use crate::config::DeviceKind;
use std::ptr::null_mut;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

/// RGBA color with straight alpha in `[0, 255]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Converts to premultiplied BGRA `u32` expected by Win32 32-bit ARGB `HICON` bitmaps.
    pub fn to_premultiplied_bgra(self) -> u32 {
        let a = self.a as u32;
        let r = (self.r as u32 * a + 127) / 255;
        let g = (self.g as u32 * a + 127) / 255;
        let b = (self.b as u32 * a + 127) / 255;
        (a << 24) | (r << 16) | (g << 8) | b
    }
}

/// Color palette adapted for Windows light/dark taskbar and mute state.
#[derive(Debug, Clone, Copy)]
struct Palette {
    icon_fg: Rgba,
    icon_accent: Rgba,
    badge_bg: Rgba,
    badge_border: Rgba,
    digit_fg: Rgba,
}

impl Palette {
    fn new(light_taskbar: bool, muted: bool) -> Self {
        if muted {
            if light_taskbar {
                Self {
                    icon_fg: Rgba::new(72, 78, 88, 220),
                    icon_accent: Rgba::new(208, 48, 56, 255),
                    badge_bg: Rgba::new(155, 28, 36, 245),
                    badge_border: Rgba::new(255, 255, 255, 190),
                    digit_fg: Rgba::new(255, 255, 255, 255),
                }
            } else {
                Self {
                    icon_fg: Rgba::new(175, 182, 194, 215),
                    icon_accent: Rgba::new(255, 92, 92, 255),
                    badge_bg: Rgba::new(158, 26, 36, 245),
                    badge_border: Rgba::new(20, 22, 28, 220),
                    digit_fg: Rgba::new(255, 255, 255, 255),
                }
            }
        } else if light_taskbar {
            Self {
                icon_fg: Rgba::new(28, 32, 38, 255),
                icon_accent: Rgba::new(0, 103, 192, 255),
                badge_bg: Rgba::new(24, 28, 36, 240),
                badge_border: Rgba::new(255, 255, 255, 175),
                digit_fg: Rgba::new(255, 255, 255, 255),
            }
        } else {
            Self {
                icon_fg: Rgba::new(244, 246, 250, 255),
                icon_accent: Rgba::new(96, 205, 255, 255),
                badge_bg: Rgba::new(18, 22, 30, 242),
                badge_border: Rgba::new(90, 104, 126, 210),
                digit_fg: Rgba::new(255, 255, 255, 255),
            }
        }
    }
}

/// 16x16 canonical pixel grid for headphones (top half, leaving room at y=9..15 for digits).
const HEADPHONES_GRID: [&str; 16] = [
    ".....######.....",
    "...##......##...",
    "..#..........#..",
    "..#..........#..",
    ".##..........##.",
    ".###........###.",
    ".###........###.",
    ".###........###.",
    "..##........##..",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

/// 16x16 canonical pixel grid for speaker cone + sound waves (`*` = accent wave).
const SPEAKER_GRID: [&str; 16] = [
    ".......#........",
    "......##....*...",
    ".....###..*..*..",
    "..######..*...*.",
    "..######...*..*.",
    "..######...*..*.",
    "..######..*...*.",
    ".....###..*..*..",
    "......##....*...",
    ".......#........",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

/// 3x5 bitmap glyphs for digits `0`..`9` (each row uses lowest 3 bits, MSB on the left).
const DIGITS_3X5: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b010, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// Narrow 2x5 glyph for `1` used when displaying `100` cleanly in 16x16 icons.
const NARROW_ONE_2X5: [u8; 5] = [0b01, 0b11, 0b01, 0b01, 0b01];

/// Crisp 4x7 bitmap glyphs for medium DPI tray icons (22..=31 px, e.g. 150% DPI = 24x24).
const DIGITS_4X7: [[u8; 7]; 10] = [
    [0b0110, 0b1001, 0b1001, 0b1001, 0b1001, 0b1001, 0b0110], // 0
    [0b0010, 0b0110, 0b0010, 0b0010, 0b0010, 0b0010, 0b0111], // 1
    [0b0110, 0b1001, 0b0001, 0b0010, 0b0100, 0b1000, 0b1111], // 2
    [0b1110, 0b0001, 0b0001, 0b0110, 0b0001, 0b0001, 0b1110], // 3
    [0b1001, 0b1001, 0b1001, 0b1111, 0b0001, 0b0001, 0b0001], // 4
    [0b1111, 0b1000, 0b1110, 0b0001, 0b0001, 0b0001, 0b1110], // 5
    [0b0111, 0b1000, 0b1110, 0b1001, 0b1001, 0b1001, 0b0110], // 6
    [0b1111, 0b0001, 0b0010, 0b0010, 0b0100, 0b0100, 0b0100], // 7
    [0b0110, 0b1001, 0b1001, 0b0110, 0b1001, 0b1001, 0b0110], // 8
    [0b0110, 0b1001, 0b1001, 0b0111, 0b0001, 0b0001, 0b1110], // 9
];

const NARROW_ONE_2X7: [u8; 7] = [0b01, 0b11, 0b01, 0b01, 0b01, 0b01, 0b01];

enum GlyphRows {
    H5([u8; 5]),
    H7([u8; 7]),
}

impl GlyphRows {
    fn height(&self) -> usize {
        match self {
            Self::H5(_) => 5,
            Self::H7(_) => 7,
        }
    }

    fn rows(&self) -> &[u8] {
        match self {
            Self::H5(r) => r,
            Self::H7(r) => r,
        }
    }
}

/// Software RGBA canvas supporting crisp rendering and scaling to any square tray icon size.
pub struct Canvas {
    pub size: usize,
    pub pixels: Vec<Rgba>,
}

impl Canvas {
    pub fn new(size: usize) -> Self {
        Self {
            size,
            pixels: vec![Rgba::new(0, 0, 0, 0); size * size],
        }
    }

    pub fn blend_pixel(&mut self, x: usize, y: usize, color: Rgba) {
        if x >= self.size || y >= self.size || color.a == 0 {
            return;
        }
        let idx = y * self.size + x;
        if color.a == 255 || self.pixels[idx].a == 0 {
            self.pixels[idx] = color;
            return;
        }
        let dst = self.pixels[idx];
        let sa = color.a as u32;
        let da = dst.a as u32;
        let out_a = sa + (da * (255 - sa) + 127) / 255;
        if out_a == 0 {
            self.pixels[idx] = Rgba::new(0, 0, 0, 0);
            return;
        }
        let blend_ch = |sc: u8, dc: u8| -> u8 {
            let num = sc as u32 * sa + (dc as u32 * da * (255 - sa) + 127) / 255;
            ((num + out_a / 2) / out_a).min(255) as u8
        };
        self.pixels[idx] = Rgba::new(
            blend_ch(color.r, dst.r),
            blend_ch(color.g, dst.g),
            blend_ch(color.b, dst.b),
            out_a.min(255) as u8,
        );
    }

    fn fill_rect(&mut self, x0: usize, y0: usize, w: usize, h: usize, color: Rgba) {
        for y in y0..(y0 + h).min(self.size) {
            for x in x0..(x0 + w).min(self.size) {
                self.blend_pixel(x, y, color);
            }
        }
    }

    fn draw_16x16_template(&mut self, template: &[&str; 16], fg: Rgba, accent: Rgba) {
        for (ty, row) in template.iter().enumerate() {
            let y0 = (ty * self.size) / 16;
            let y1 = (((ty + 1) * self.size) / 16).max(y0 + 1);
            for (tx, ch) in row.bytes().enumerate() {
                let color = match ch {
                    b'#' => fg,
                    b'*' => accent,
                    _ => continue,
                };
                let x0 = (tx * self.size) / 16;
                let x1 = (((tx + 1) * self.size) / 16).max(x0 + 1);
                self.fill_rect(x0, y0, x1 - x0, y1 - y0, color);
            }
        }
    }

    fn draw_mute_cross(&mut self, color: Rgba) {
        // Draw a compact X in the upper-right region when muted on speakers, or center-top on headphones.
        let scale = (self.size / 16).max(1);
        let base_x = (self.size * 11) / 16;
        let base_y = (self.size * 2) / 16;
        let span = 3 * scale;
        for i in 0..=span {
            self.fill_rect(base_x + i, base_y + i, scale, scale, color);
            self.fill_rect(base_x + span - i, base_y + i, scale, scale, color);
        }
    }

    fn draw_volume_badge(&mut self, volume: u8, palette: Palette) {
        let clamped = volume.min(100);
        let use_4x7 = (20..32).contains(&self.size);
        let scale = if use_4x7 { 1 } else { (self.size / 16).max(1) };

        let glyphs: Vec<(usize, GlyphRows)> = if use_4x7 {
            if clamped == 100 {
                vec![
                    (2, GlyphRows::H7(NARROW_ONE_2X7)),
                    (4, GlyphRows::H7(DIGITS_4X7[0])),
                    (4, GlyphRows::H7(DIGITS_4X7[0])),
                ]
            } else if clamped >= 10 {
                vec![
                    (4, GlyphRows::H7(DIGITS_4X7[(clamped / 10) as usize])),
                    (4, GlyphRows::H7(DIGITS_4X7[(clamped % 10) as usize])),
                ]
            } else {
                vec![(4, GlyphRows::H7(DIGITS_4X7[clamped as usize]))]
            }
        } else if clamped == 100 {
            vec![
                (2, GlyphRows::H5(NARROW_ONE_2X5)),
                (3, GlyphRows::H5(DIGITS_3X5[0])),
                (3, GlyphRows::H5(DIGITS_3X5[0])),
            ]
        } else if clamped >= 10 {
            vec![
                (3, GlyphRows::H5(DIGITS_3X5[(clamped / 10) as usize])),
                (3, GlyphRows::H5(DIGITS_3X5[(clamped % 10) as usize])),
            ]
        } else {
            vec![(3, GlyphRows::H5(DIGITS_3X5[clamped as usize]))]
        };

        let gap = 1usize;
        let text_units_w: usize =
            glyphs.iter().map(|(w, _)| *w).sum::<usize>() + gap * (glyphs.len() - 1);
        let text_units_h = glyphs[0].1.height();
        let text_w = text_units_w * scale;
        let text_h = text_units_h * scale;

        let pad_x = if use_4x7 && self.size >= 22 { 2 } else { scale };
        let pad_y = scale;
        let badge_w = (text_w + pad_x * 2).min(self.size);
        let badge_h = (text_h + pad_y * 2).min(self.size);

        // Center horizontally so 1-digit, 2-digit, and compact 100 all sit symmetrically at the bottom.
        let badge_x = (self.size.saturating_sub(badge_w)) / 2;
        let badge_y = self.size.saturating_sub(badge_h);

        // Draw pill background with rounded 1px corners.
        for dy in 0..badge_h {
            for dx in 0..badge_w {
                let is_corner = (dy == 0 || dy + 1 == badge_h) && (dx == 0 || dx + 1 == badge_w);
                if is_corner && badge_w >= 6 {
                    continue;
                }
                let is_top_border = dy == 0;
                let color = if is_top_border {
                    palette.badge_border
                } else {
                    palette.badge_bg
                };
                self.blend_pixel(badge_x + dx, badge_y + dy, color);
            }
        }

        // Draw crisp digits inside the pill.
        let mut cursor_x = badge_x + (badge_w.saturating_sub(text_w)) / 2;
        let start_y = badge_y + (badge_h.saturating_sub(text_h)) / 2;
        for (idx, (gw, rows)) in glyphs.iter().enumerate() {
            for (ry, bits) in rows.rows().iter().enumerate() {
                for gx in 0..*gw {
                    let bit_shift = gw - 1 - gx;
                    if (bits >> bit_shift) & 1 == 1 {
                        self.fill_rect(
                            cursor_x + gx * scale,
                            start_y + ry * scale,
                            scale,
                            scale,
                            palette.digit_fg,
                        );
                    }
                }
            }
            cursor_x += gw * scale;
            if idx + 1 < glyphs.len() {
                cursor_x += gap * scale;
            }
        }
    }
}

/// Renders the tray icon pixels into a software RGBA canvas (testable without GDI handles).
pub fn render_icon_canvas(
    size: usize,
    kind: DeviceKind,
    volume: u8,
    muted: bool,
    light_taskbar: bool,
) -> Canvas {
    let size = size.clamp(16, 128);
    let palette = Palette::new(light_taskbar, muted);
    let mut canvas = Canvas::new(size);

    let template = match kind {
        DeviceKind::Headphones => &HEADPHONES_GRID,
        DeviceKind::Speakers => &SPEAKER_GRID,
    };
    let wave_color = if muted {
        Rgba::new(0, 0, 0, 0)
    } else {
        palette.icon_accent
    };
    canvas.draw_16x16_template(template, palette.icon_fg, wave_color);

    if muted {
        canvas.draw_mute_cross(palette.icon_accent);
    }
    canvas.draw_volume_badge(volume, palette);
    canvas
}

/// Creates a Win32 `HICON` for the given audio state and icon size (caller must `DestroyIcon`).
pub fn create_tray_hicon(
    size: i32,
    kind: DeviceKind,
    volume: u8,
    muted: bool,
    light_taskbar: bool,
) -> windows::core::Result<HICON> {
    let size_px = size.clamp(16, 128) as usize;
    let canvas = render_icon_canvas(size_px, kind, volume, muted, light_taskbar);

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size_px as i32,
            biHeight: -(size_px as i32), // Top-down DIB matching canvas row order.
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    unsafe {
        let hdc = CreateCompatibleDC(None);
        let mut bits_ptr: *mut std::ffi::c_void = null_mut();
        let hbm_color = CreateDIBSection(
            Some(hdc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        )?;
        let _ = DeleteDC(hdc);

        if bits_ptr.is_null() {
            let _ = DeleteObject(hbm_color.into());
            return Err(windows::core::Error::from_thread());
        }

        let dst = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, size_px * size_px);
        for (out_px, src_px) in dst.iter_mut().zip(canvas.pixels.iter()) {
            *out_px = src_px.to_premultiplied_bgra();
        }

        let mask_bytes = vec![0u8; ((size_px + 15) / 16) * 2 * size_px];
        let hbm_mask = CreateBitmap(
            size_px as i32,
            size_px as i32,
            1,
            1,
            Some(mask_bytes.as_ptr() as *const _),
        );

        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let icon_res = CreateIconIndirect(&icon_info);

        let _ = DeleteObject(hbm_color.into());
        let _ = DeleteObject(hbm_mask.into());
        icon_res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_edge_volumes_and_sizes_without_panicking() {
        for &size in &[16usize, 20, 24, 32] {
            for &kind in &[DeviceKind::Headphones, DeviceKind::Speakers] {
                for &vol in &[0u8, 5, 42, 99, 100] {
                    for &muted in &[false, true] {
                        for &light in &[false, true] {
                            let canvas = render_icon_canvas(size, kind, vol, muted, light);
                            assert_eq!(canvas.pixels.len(), size * size);
                            let non_transparent =
                                canvas.pixels.iter().filter(|p| p.a > 0).count();
                            assert!(non_transparent > size * 2);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn headphones_and_speakers_produce_distinct_icons() {
        let hp = render_icon_canvas(16, DeviceKind::Headphones, 50, false, false);
        let sp = render_icon_canvas(16, DeviceKind::Speakers, 50, false, false);
        assert_ne!(hp.pixels, sp.pixels);
    }

    #[test]
    fn compact_100_fits_inside_16x16_badge() {
        let canvas = render_icon_canvas(16, DeviceKind::Headphones, 100, false, false);
        // Bottom row digits end at row 14 (0-indexed), check digit pixels exist in rows 10..15.
        let white_bottom_pixels = (10..15)
            .flat_map(|y| (2..14).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = canvas.pixels[y * 16 + x];
                p.r == 255 && p.g == 255 && p.b == 255
            })
            .count();
        assert!(white_bottom_pixels >= 20);
    }

    #[test]
    fn creates_and_destroys_win32_hicon() {
        let icon = create_tray_hicon(16, DeviceKind::Speakers, 100, false, false)
            .expect("Win32 HICON creation");
        assert!(!icon.is_invalid());
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::DestroyIcon(icon).expect("DestroyIcon");
        }
    }
}

