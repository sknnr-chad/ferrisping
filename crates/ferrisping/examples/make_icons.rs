//! Draws the app icon from the SVGs in `assets/` into the files the app embeds:
//! `ferrisping.ico` for the executable, and PNGs for the window and the toolbar.
//!
//! ```text
//! cargo run -p ferrisping --example make_icons [-- --preview <sheet.png>]
//! ```
//!
//! `--preview` also writes every size on light and dark backgrounds, with the
//! small ones magnified, for checking the drawing by eye.

use std::path::Path;

use resvg::tiny_skia::{Color, FilterQuality, Paint, Pixmap, PixmapPaint, Rect, Transform};
use resvg::usvg::{Options, Tree};

/// Sizes Windows asks an .ico for at 100-200% display scaling.
const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// Up to this size the simplified drawing is used; the detailed one turns to mush.
const SMALL_MAX: u32 = 24;
/// PNGs the app loads: the window icon and the in-app logo.
const PNG_SIZES: [u32; 2] = [64, 256];

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let icons = Icons {
        detailed: load(&assets.join("ferrisping.svg")),
        small: load(&assets.join("ferrisping-small.svg")),
    };

    let frames: Vec<(u32, Vec<u8>)> = ICO_SIZES.iter().map(|&size| (size, png(&icons.render(size)))).collect();
    write(&assets.join("ferrisping.ico"), &ico(&frames));
    for size in PNG_SIZES {
        write(
            &assets.join(format!("ferrisping-{size}.png")),
            &png(&icons.render(size)),
        );
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, path] = args.as_slice()
        && flag == "--preview"
    {
        icons.preview(Path::new(path));
    }
}

struct Icons {
    detailed: Tree,
    small: Tree,
}

impl Icons {
    fn render(&self, size: u32) -> Pixmap {
        let tree = if size <= SMALL_MAX { &self.small } else { &self.detailed };
        let mut pixmap = Pixmap::new(size, size).expect("icon size");
        let scale = size as f32 / tree.size().width();
        resvg::render(tree, Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        pixmap
    }

    /// Every size on a light and a dark band, bottom-aligned, with the small sizes
    /// repeated four times larger, pixel for pixel, underneath.
    fn preview(&self, path: &Path) {
        const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
        const ZOOMED: [u32; 4] = [16, 20, 24, 32];
        const ZOOM: u32 = 4;
        const PAD: u32 = 16;
        let band = 256 + 32 * ZOOM + 3 * PAD;
        let width = SIZES.iter().sum::<u32>() + PAD * (SIZES.len() as u32 + 1);
        let mut sheet = Pixmap::new(width, band * 2).expect("sheet size");
        let backgrounds = [
            Color::from_rgba8(243, 243, 241, 255),
            Color::from_rgba8(26, 26, 25, 255),
        ];
        for (row, background) in backgrounds.into_iter().enumerate() {
            let top = row as u32 * band;
            let mut paint = Paint::default();
            paint.set_color(background);
            let rect = Rect::from_xywh(0.0, top as f32, width as f32, band as f32).expect("band");
            sheet.fill_rect(rect, &paint, Transform::identity(), None);

            let mut x = PAD;
            for size in SIZES {
                let y = top + PAD + 256 - size;
                let icon = self.render(size);
                sheet.draw_pixmap(
                    x as i32,
                    y as i32,
                    icon.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                x += size + PAD;
            }
            let nearest = PixmapPaint {
                quality: FilterQuality::Nearest,
                ..PixmapPaint::default()
            };
            let mut x = PAD;
            for size in ZOOMED {
                let y = top + 2 * PAD + 256;
                let at = Transform::from_row(ZOOM as f32, 0.0, 0.0, ZOOM as f32, x as f32, y as f32);
                sheet.draw_pixmap(0, 0, self.render(size).as_ref(), &nearest, at, None);
                x += size * ZOOM + PAD;
            }
        }
        write(path, &png(&sheet));
    }
}

fn load(path: &Path) -> Tree {
    let svg = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    Tree::from_data(&svg, &Options::default()).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn png(pixmap: &Pixmap) -> Vec<u8> {
    pixmap.encode_png().expect("encode PNG")
}

/// An .ico file whose images are stored as PNGs, which Windows reads since Vista.
fn ico(frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len();
    for (size, data) in frames {
        let side = if *size >= 256 { 0 } else { *size as u8 }; // 0 stands for 256
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in frames {
        out.extend_from_slice(data);
    }
    out
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    println!("wrote {} ({} bytes)", path.display(), bytes.len());
}
