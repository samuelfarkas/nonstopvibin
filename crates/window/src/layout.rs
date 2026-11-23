//! Window geometry, in logical pixels.

use nsv_desktop::pipe::Rect;

pub(crate) const MAIN_SIZE: (u32, u32) = (1240, 880);
pub(crate) const MAIN_MIN_SIZE: (u32, u32) = (740, 580);
/// The quota panel's CSS fills whatever width the window has.
const QUOTA_WIDTH: u32 = 386;

/// Where the quota popup goes, as `openTray` in `desktop/main.ts` placed it: centred under the
/// tray icon and kept 8 px inside the work area, 7 px below the menu bar icon on macOS
/// (`below_anchor`), 8 px above the bottom of the work area elsewhere.
pub(crate) fn quota_bounds(anchor: Rect, work_area: Rect, below_anchor: bool) -> Rect {
    let height = work_area.height.saturating_sub(24).min(640);
    let (area_x, area_width) = (f64::from(work_area.x), f64::from(work_area.width));
    let width = f64::from(QUOTA_WIDTH);
    let centred = f64::from(anchor.x) + f64::from(anchor.width) / 2.0 - width / 2.0;
    let x = centred
        .min(area_x + area_width - width - 8.0)
        .max(area_x + 8.0);
    let y = if below_anchor {
        i64::from(anchor.y) + i64::from(anchor.height) + 7
    } else {
        i64::from(work_area.y) + i64::from(work_area.height) - i64::from(height) - 8
    };
    Rect {
        x: math_round(x),
        y: i32::try_from(y).unwrap_or(work_area.y),
        width: QUOTA_WIDTH,
        height,
    }
}

/// JavaScript's `Math.round`: halves round up, toward positive infinity.
fn math_round(value: f64) -> i32 {
    let rounded = (value + 0.5).floor();
    if rounded >= f64::from(i32::MAX) {
        i32::MAX
    } else if rounded <= f64::from(i32::MIN) {
        i32::MIN
    } else {
        #[expect(clippy::cast_possible_truncation, reason = "clamped to i32 above")]
        let whole = rounded as i32;
        whole
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn places_the_popup_as_main_ts_did() {
        // Linux: a 27 px top panel, tray icon near the right edge; the popup hugs the bottom.
        let linux = quota_bounds(rect(1800, 0, 24, 27), rect(0, 27, 1920, 1053), false);
        assert_eq!(linux, rect(1526, 432, 386, 640));
        // macOS: under the menu bar icon, clamped 8 px inside the right edge.
        let mac = quota_bounds(rect(1500, 0, 30, 24), rect(0, 25, 1512, 957), true);
        assert_eq!(mac, rect(1118, 31, 386, 640));
        // Centred under an icon in the middle; 819.5 rounds up as Math.round does.
        let centred = quota_bounds(rect(1000, 0, 25, 24), rect(0, 25, 1920, 1055), true);
        assert_eq!(centred, rect(820, 31, 386, 640));
        // A short work area: 24 px less than its height. Left edge: 8 px inside.
        let short = quota_bounds(rect(-5, 0, 20, 20), rect(-100, 0, 1280, 500), false);
        assert_eq!(short, rect(-92, 16, 386, 476));
    }
}
