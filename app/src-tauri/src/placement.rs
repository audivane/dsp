//! Where the tray popup and the on-screen display go, in physical pixels
//! of one monitor's work area. Only X11 lets an app place its windows; on
//! Wayland the compositor decides and these positions are ignored.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Gap between the popup and the tray icon or the screen edge.
pub const MARGIN: i32 = 8;

/// The popup next to the click: centred on it, above it when the click is
/// in the lower half (a bottom panel), below it otherwise, always inside
/// the work area. Without a click position (0, 0 or none), the corner the
/// tray usually sits in: bottom right.
pub fn popup(click: Option<(i32, i32)>, area: Area, size: (i32, i32)) -> (i32, i32) {
    let (w, h) = size;
    let max_x = area.x + area.w - w - MARGIN;
    let max_y = area.y + area.h - h - MARGIN;
    let clamp = |v: i32, lo: i32, hi: i32| if hi < lo { lo } else { v.clamp(lo, hi) };
    match click.filter(|&(x, y)| (x, y) != (0, 0)) {
        None => (clamp(max_x, area.x, max_x), clamp(max_y, area.y, max_y)),
        Some((cx, cy)) => {
            let x = clamp(cx - w / 2, area.x + MARGIN, max_x);
            let below = cy < area.y + area.h / 2;
            let y = if below { cy + MARGIN } else { cy - h - MARGIN };
            (x, clamp(y, area.y + MARGIN, max_y))
        }
    }
}

/// The OSD: centred, in the lower part of the work area (clear of a
/// bottom panel, which the work area already leaves out).
pub fn osd(area: Area, size: (i32, i32)) -> (i32, i32) {
    let (w, h) = size;
    let x = area.x + (area.w - w) / 2;
    let y = area.y + area.h - h - area.h / 8;
    (x, y.max(area.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Area = Area {
        x: 0,
        y: 0,
        w: 1920,
        h: 1040,
    };

    #[test]
    fn bottom_panel_click_opens_above_the_icon() {
        // The click is on the panel, below the work area: the popup sits on
        // the work area's bottom edge, and its right edge stays on screen.
        assert_eq!(
            popup(Some((1800, 1060)), A, (320, 180)),
            (1920 - 320 - MARGIN, 1040 - 180 - MARGIN)
        );
        // A click higher up (a tall panel or a floating tray) opens above it.
        assert_eq!(
            popup(Some((1000, 800)), A, (320, 180)),
            (1000 - 160, 800 - 180 - MARGIN)
        );
    }

    #[test]
    fn top_panel_click_opens_below() {
        let (x, y) = popup(
            Some((900, 12)),
            Area {
                y: 30,
                h: 1050,
                ..A
            },
            (320, 180),
        );
        assert_eq!(x, 900 - 160);
        assert_eq!(y, 30 + MARGIN);
    }

    #[test]
    fn unknown_click_uses_the_bottom_right_corner() {
        assert_eq!(
            popup(None, A, (320, 180)),
            (1920 - 320 - MARGIN, 1040 - 180 - MARGIN)
        );
        assert_eq!(
            popup(Some((0, 0)), A, (320, 180)),
            (1920 - 320 - MARGIN, 1040 - 180 - MARGIN)
        );
    }

    #[test]
    fn second_monitor_offsets_are_kept() {
        let right = Area { x: 1920, ..A };
        let (x, _) = popup(Some((1925, 1000)), right, (320, 180));
        assert_eq!(x, 1920 + MARGIN);
    }

    #[test]
    fn osd_sits_low_and_centred() {
        assert_eq!(osd(A, (280, 80)), ((1920 - 280) / 2, 1040 - 80 - 130));
    }
}
