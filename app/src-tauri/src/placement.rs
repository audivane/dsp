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
