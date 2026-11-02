//! Where the first window opens. On a laptop the usual size can be taller than the
//! screen less the taskbar, so there it shrinks to fit. Later launches reopen the
//! window as the user left it.

use eframe::egui::{Pos2, Rect, Vec2, vec2};

/// The window's usual content size.
pub const SIZE: Vec2 = vec2(1180.0, 820.0);
/// The smallest the window's content can be resized to.
pub const MIN_SIZE: Vec2 = vec2(720.0, 480.0);
/// Room for the window's frame: its borders, and the title bar above the content.
const FRAME: Vec2 = vec2(16.0, 40.0);

/// The first window's content size, and where its frame's corner goes, given the
/// screen's work area (the screen less the taskbar) in points.
pub fn first_window(work_area: Option<Rect>) -> (Vec2, Option<Pos2>) {
    let Some(area) = work_area else {
        return (SIZE, None);
    };
    let size = SIZE.min(area.size() - FRAME).max(MIN_SIZE);
    let corner = area.center() - (size + FRAME) / 2.0;
    (size, Some(corner.max(area.min)))
}

/// The main screen's work area, in points.
#[cfg(windows)]
pub fn work_area() -> Option<Rect> {
    use eframe::egui::pos2;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};

    let mut area = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0) } == 0 {
        return None;
    }
    // Pixels once the process is DPI-aware, and points before that, while the window
    // is being set up. Windows reports the matching scale either way.
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let rect = Rect::from_min_max(
        pos2(area.left as f32 / scale, area.top as f32 / scale),
        pos2(area.right as f32 / scale, area.bottom as f32 / scale),
    );
    (scale > 0.0 && rect.is_positive()).then_some(rect)
}

#[cfg(not(windows))]
pub fn work_area() -> Option<Rect> {
    None
}

#[cfg(test)]
mod tests {
    use eframe::egui::pos2;

    use super::*;

    #[test]
    fn a_big_screen_gets_the_usual_size_in_the_middle() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(1920.0, 1032.0));
        assert_eq!(first_window(Some(area)), (SIZE, Some(pos2(362.0, 86.0))));
    }

    #[test]
    fn a_laptop_screen_gets_a_window_clear_of_the_taskbar() {
        // 1920 × 1200 at 150% is 1280 × 800 points, less a 48-point taskbar.
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 752.0));
        let (size, corner) = first_window(Some(area));
        assert_eq!(size, vec2(1180.0, 712.0));
        let frame = Rect::from_min_size(corner.expect("a corner"), size + FRAME);
        assert!(area.contains_rect(frame), "{frame:?} fits in {area:?}");
    }

    #[test]
    fn without_a_work_area_the_usual_size() {
        assert_eq!(first_window(None), (SIZE, None));
    }

    #[cfg(windows)]
    #[test]
    fn finds_the_work_area() {
        let area = work_area().expect("a work area");
        assert!(area.width() >= 320.0 && area.height() >= 240.0, "{area:?}");
    }
}
