//! Where a new document window opens: on the monitor of the window focused last, measured on the
//! monitors themselves (in physical pixels), not guessed from a monitor's size.
//!
//! egui reports the size of a window's monitor but not where that monitor is, so the cascade's
//! bounds used to be guessed: monitors of that size were taken to tile the desktop from its
//! origin. An external monitor of another size, or one beside or above the laptop's own screen,
//! isn't on that grid, and a window on a monitor scaled differently from the one a new window is
//! made on has its position converted with the wrong scale. Either way a second document opened
//! from a full-screen window on an external monitor was listed in the taskbar and never seen.
//! The desktop app now lists the monitors ([`crate::WordApp::set_monitors`]); the new window's
//! place is worked out on them, and a window that still opens off that monitor is moved there.

use egui::{Pos2, Rect, Vec2};

/// The monitor holding `window`'s centre, else the one it overlaps most: `None` when it is on
/// none of `monitors`.
pub fn monitor_holding(window: Rect, monitors: &[Rect]) -> Option<Rect> {
    let usable = |m: &&Rect| m.is_finite() && m.is_positive();
    if let Some(m) = monitors.iter().filter(usable).find(|m| m.contains(window.center())) {
        return Some(*m);
    }
    monitors
        .iter()
        .filter(usable)
        .map(|m| (m.intersect(window), m))
        .filter(|(on, _)| on.is_positive())
        .max_by(|(a, _), (b, _)| a.area().total_cmp(&b.area()))
        .map(|(_, m)| *m)
}

/// Where a window of outer `size` opens on `monitor`, one `step` right of and below `previous`,
/// the window focused last (each axis starting over at the monitor's edge when it would run past
/// it). All in physical pixels.
pub fn cascade_on(previous: Rect, size: Vec2, monitor: Rect, step: f32) -> Pos2 {
    let placed = craft_window::cascade(previous.into(), [size.x, size.y], monitor.into(), step);
    egui::pos2(placed.x, placed.y)
}

/// Whether `window` (physical pixels) must be moved to `monitor`: its centre is off it, or too
/// little of its top edge is on it to drag it by.
pub fn misplaced(window: Rect, monitor: Rect) -> bool {
    !window.is_finite() || !monitor.contains(window.center()) || !crate::window_geometry::reachable(window, &[monitor])
}

/// A rectangle in points, `pixels_per_point` (the window's own) giving physical pixels.
pub fn to_physical(rect: Rect, pixels_per_point: f32) -> Option<Rect> {
    let r = Rect::from_min_max((rect.min.to_vec2() * pixels_per_point).to_pos2(), (rect.max.to_vec2() * pixels_per_point).to_pos2());
    (pixels_per_point.is_finite() && pixels_per_point > 0.0 && r.is_finite()).then_some(r)
}

/// A new window being checked until it shows on the monitor it was meant for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pending {
    /// The monitor of the window focused last when it opened (physical pixels).
    pub monitor: Rect,
    /// Its place there (physical pixels, outer top-left corner).
    pub at: Pos2,
    /// Frames since it opened.
    pub frames: u32,
    /// Moves asked for so far.
    pub moves: u32,
    /// The frame of the last move: the system moves the window a frame or two later.
    pub moved_at: u32,
}

/// Frames a new window has to report where it is before the check gives up on it.
const WAIT_FRAMES: u32 = 120;
/// Moves asked for before the check gives up: the system may keep a window where it put it.
const MAX_MOVES: u32 = 3;
/// Frames between moves, for the last one to arrive.
const SETTLE_FRAMES: u32 = 10;

/// What to do about a new window this frame, given where it is (physical pixels and its pixels
/// per point), if it reports that yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Check {
    /// Keep checking on later frames.
    Wait,
    /// Move the window to this outer position, in its own points.
    Move(Pos2),
    /// It is where it should be, or the check gave up: stop checking.
    Done,
}

impl Pending {
    pub fn new(monitor: Rect, at: Pos2) -> Self {
        Self { monitor, at, frames: 0, moves: 0, moved_at: 0 }
    }

    /// The check for this frame. `window` is the window's outer rectangle in physical pixels and
    /// its pixels per point, once it reports them.
    pub fn check(&mut self, window: Option<(Rect, f32)>) -> Check {
        self.frames = self.frames.saturating_add(1);
        let Some((rect, ppp)) = window.filter(|(r, p)| r.is_finite() && p.is_finite() && *p > 0.0) else {
            return if self.frames > WAIT_FRAMES { Check::Done } else { Check::Wait };
        };
        if !misplaced(rect, self.monitor) {
            return Check::Done;
        }
        if self.moves >= MAX_MOVES {
            return if self.frames.saturating_sub(self.moved_at) > SETTLE_FRAMES { Check::Done } else { Check::Wait };
        }
        if self.moves > 0 && self.frames.saturating_sub(self.moved_at) <= SETTLE_FRAMES {
            return Check::Wait;
        }
        self.moves += 1;
        self.moved_at = self.frames;
        Check::Move((self.at.to_vec2() / ppp).to_pos2())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A laptop screen of 1920×1200 pixels at 150 % at the origin, and an external monitor of
    /// 2560×1440 at 100 % to its right: the arrangement the old tiling guess got wrong.
    fn desk() -> [Rect; 2] {
        [
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1200.0)),
            Rect::from_min_size(egui::pos2(1920.0, 0.0), egui::vec2(2560.0, 1440.0)),
        ]
    }

    #[test]
    fn a_maximized_window_on_the_external_monitor_is_on_that_monitor() {
        let [laptop, external] = desk();
        // Maximized on Windows: a few pixels past each edge.
        let maximized = Rect::from_min_size(egui::pos2(1912.0, -8.0), egui::vec2(2576.0, 1416.0));
        assert_eq!(monitor_holding(maximized, &desk()), Some(external));
        // The old guess from the monitor's size alone: the tile from 2560, past the monitor's
        // middle, which would cascade the window to the right of what is on screen.
        let guessed = crate::windows::monitor_of(maximized.into(), external.size());
        assert_ne!((guessed.x, guessed.y), (external.min.x, external.min.y));
        let restored = Rect::from_min_size(egui::pos2(100.0, 80.0), egui::vec2(1200.0, 900.0));
        assert_eq!(monitor_holding(restored, &desk()), Some(laptop));
        assert_eq!(monitor_holding(Rect::from_min_size(egui::pos2(9000.0, 0.0), egui::vec2(10.0, 10.0)), &desk()), None);
        // Centre off every monitor, but partly on one: that one.
        let straddling = Rect::from_min_size(egui::pos2(-500.0, 100.0), egui::vec2(800.0, 600.0));
        assert_eq!(monitor_holding(straddling, &desk()), Some(laptop));
    }

    #[test]
    fn the_new_window_cascades_on_the_monitor_of_the_last_one() {
        let [_, external] = desk();
        let maximized = Rect::from_min_size(egui::pos2(1912.0, -8.0), egui::vec2(2576.0, 1416.0));
        let at = cascade_on(maximized, egui::vec2(1456.0, 959.0), external, 28.0);
        assert_eq!(at, egui::pos2(1940.0, 20.0));
        let placed = Rect::from_min_size(at, egui::vec2(1456.0, 959.0));
        assert!(!misplaced(placed, external));
    }

    #[test]
    fn a_window_opened_off_its_monitor_is_moved_there_in_its_own_points() {
        let [laptop, external] = desk();
        let mut p = Pending::new(external, egui::pos2(1940.0, 20.0));
        assert_eq!(p.check(None), Check::Wait, "not shown yet");
        // Placed with the laptop's 150 % scale: 1.5 times too far right, off every screen.
        let off = Rect::from_min_size(egui::pos2(2910.0 * 1.5, 30.0), egui::vec2(2184.0, 1438.0));
        assert!(misplaced(off, external));
        assert_eq!(p.check(Some((off, 1.5))), Check::Move(egui::pos2(1940.0 / 1.5, 20.0 / 1.5)));
        // The move takes a frame or two: no second move meanwhile.
        assert_eq!(p.check(Some((off, 1.5))), Check::Wait);
        let there = Rect::from_min_size(egui::pos2(1940.0, 20.0), egui::vec2(1456.0, 959.0));
        assert_eq!(p.check(Some((there, 1.0))), Check::Done);
        // A window already where it should be is left alone.
        let mut q = Pending::new(laptop, egui::pos2(128.0, 108.0));
        assert_eq!(q.check(Some((Rect::from_min_size(egui::pos2(128.0, 108.0), egui::vec2(900.0, 700.0)), 1.5))), Check::Done);
    }

    #[test]
    fn the_check_gives_up_on_a_window_that_never_reports_or_never_moves() {
        let [_, external] = desk();
        let mut p = Pending::new(external, egui::pos2(1940.0, 20.0));
        let mut last = Check::Wait;
        for _ in 0..=WAIT_FRAMES {
            last = p.check(None);
        }
        assert_eq!(last, Check::Done);
        let mut q = Pending::new(external, egui::pos2(1940.0, 20.0));
        let off = Rect::from_min_size(egui::pos2(9000.0, 0.0), egui::vec2(1000.0, 800.0));
        let moves = (0..200).map(|_| q.check(Some((off, 1.0)))).take_while(|c| *c != Check::Done).filter(|c| matches!(c, Check::Move(_))).count();
        assert_eq!(moves, MAX_MOVES as usize);
        // Nonsense from the platform never moves a window.
        let mut r = Pending::new(external, egui::pos2(1940.0, 20.0));
        assert_eq!(r.check(Some((off, f32::NAN))), Check::Wait);
        assert_eq!(r.check(Some((off, 0.0))), Check::Wait);
    }

    #[test]
    fn physical_pixels_from_points() {
        let r = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 50.0));
        assert_eq!(to_physical(r, 1.5), Some(Rect::from_min_size(egui::pos2(15.0, 30.0), egui::vec2(150.0, 75.0))));
        assert_eq!(to_physical(r, 0.0), None);
        assert_eq!(to_physical(r, f32::INFINITY), None);
    }
}
