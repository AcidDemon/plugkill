//! Where the dashboard opens, from the position the panel reported for a click.

/// A monitor in the logical coordinates panels report clicks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

/// Layer-shell anchoring for the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Index into the monitors passed to `place`.
    pub monitor: usize,
    /// Anchor to the top edge (true) or the bottom edge (false).
    pub top: bool,
    /// Margin from the monitor's right edge.
    pub right_margin: i32,
}

/// Distance kept from every monitor edge.
pub const INSET: i32 = 8;

/// Open on the monitor that contains the click, at the edge of whichever half
/// the click was in, with the window's right edge at the click and the whole
/// window on screen. A click reported as (0, 0), or outside every monitor,
/// opens in the top right corner of the first monitor. None without monitors.
pub fn place(click_x: i32, click_y: i32, monitors: &[Rect], width: i32) -> Option<Placement> {
    if monitors.is_empty() {
        return None;
    }
    let found = if (click_x, click_y) == (0, 0) {
        None
    } else {
        monitors.iter().position(|m| m.contains(click_x, click_y))
    };
    let Some(index) = found else {
        return Some(Placement {
            monitor: 0,
            top: true,
            right_margin: INSET,
        });
    };
    let m = monitors[index];
    let top = click_y - m.y < m.height / 2;
    let from_right = m.x + m.width - click_x;
    let max_right = (m.width - width - INSET).max(INSET);
    Some(Placement {
        monitor: index,
        top,
        right_margin: from_right.clamp(INSET, max_right),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: Rect = Rect {
        x: 0,
        y: 0,
        width: 2560,
        height: 1440,
    };
    const RIGHT: Rect = Rect {
        x: 2560,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn test_click_in_a_top_bar_opens_under_it_on_that_monitor() {
        let p = place(4200, 20, &[LEFT, RIGHT], 352).unwrap();
        assert_eq!(
            p,
            Placement {
                monitor: 1,
                top: true,
                right_margin: 280
            }
        );
    }

    #[test]
    fn test_click_in_a_bottom_bar_anchors_to_the_bottom() {
        let p = place(2400, 1420, &[LEFT, RIGHT], 352).unwrap();
        assert_eq!(p.monitor, 0);
        assert!(!p.top);
        assert_eq!(p.right_margin, 160);
    }

    #[test]
    fn test_window_stays_on_screen_near_either_edge() {
        let near_right = place(2555, 10, &[LEFT], 352).unwrap();
        assert_eq!(near_right.right_margin, INSET);
        let near_left = place(40, 10, &[LEFT], 352).unwrap();
        assert_eq!(near_left.right_margin, 2560 - 352 - INSET);
    }

    #[test]
    fn test_unknown_position_opens_top_right_of_the_first_monitor() {
        let expected = Placement {
            monitor: 0,
            top: true,
            right_margin: INSET,
        };
        assert_eq!(place(0, 0, &[LEFT, RIGHT], 352), Some(expected));
        assert_eq!(place(-50, 9000, &[LEFT, RIGHT], 352), Some(expected));
    }

    #[test]
    fn test_no_monitors_no_placement() {
        assert_eq!(place(10, 10, &[], 352), None);
    }
}
