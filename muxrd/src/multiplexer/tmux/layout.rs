//! Layout snapshot: tmux window and pane rows mapped to the neutral layout.
//!
//! Pure functions over already-parsed rows; this module performs no I/O.
//!
//! Id scheme: a tab id is the tmux window id and a pane id is the tmux pane
//! id, both as the raw number with the `@` / `%` sigil stripped. A tab's
//! `position` is the window index.
//!
//! `panes_to_hide` is always 0: tmux never hides panes to fit a smaller
//! display, so the field carries no information for this backend.
//!
//! Zoom is derived, never read from `pane_zoomed_flag` (absent on tmux 3.5a):
//! in a zoomed window the zoomed pane is always the window's active pane, so
//! `is_fullscreen` is `window zoomed && pane active`.

use crate::multiplexer::types::{LayoutSnapshot, PaneRef, PaneSnapshot, TabSnapshot};

/// One tmux window row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowIn {
    pub id: u32,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub bell: bool,
    pub zoomed: bool,
    pub width: u32,
    pub height: u32,
}

/// One tmux pane row. Before tmux 3.7 `floating` is false and `z` is 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneIn {
    pub id: u32,
    pub window_id: u32,
    pub active: bool,
    pub title: String,
    pub current_command: String,
    pub current_path: String,
    pub dead: bool,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub floating: bool,
    pub z: i32,
}

/// Build the neutral snapshot: tabs by window index, panes tiled first then
/// floating by ascending z (ties by pane id), so input order never matters.
pub fn snapshot(windows: &[WindowIn], panes: &[PaneIn]) -> LayoutSnapshot {
    let mut wins: Vec<&WindowIn> = windows.iter().collect();
    wins.sort_by_key(|w| (w.index, w.id));
    let tabs = wins
        .into_iter()
        .map(|w| {
            let mut ps: Vec<&PaneIn> = panes.iter().filter(|p| p.window_id == w.id).collect();
            ps.sort_by_key(|p| (p.floating, if p.floating { p.z } else { 0 }, p.id));
            TabSnapshot {
                tab_id: u64::from(w.id),
                position: w.index,
                name: w.name.clone(),
                active: w.active,
                has_bell: w.bell,
                panes_to_hide: 0,
                fullscreen_active: w.zoomed,
                floating_panes_visible: ps.iter().any(|p| p.floating),
                panes: ps
                    .into_iter()
                    .map(|p| PaneSnapshot {
                        id: p.id,
                        title: p.title.clone(),
                        is_focused: p.active,
                        is_floating: p.floating,
                        exited: p.dead,
                        command: p.current_command.clone(),
                        cwd: p.current_path.clone(),
                        x: p.left,
                        y: p.top,
                        rows: p.height,
                        cols: p.width,
                        is_plugin: false,
                        is_fullscreen: w.zoomed && p.active,
                        agent: None,
                    })
                    .collect(),
            }
        })
        .collect();
    LayoutSnapshot { tabs }
}

/// `(rows, cols)` of the active window, falling back to the lowest-index one.
pub fn session_size(windows: &[WindowIn]) -> Option<(u16, u16)> {
    let w = windows
        .iter()
        .filter(|w| w.active)
        .min_by_key(|w| (w.index, w.id))
        .or_else(|| windows.iter().min_by_key(|w| (w.index, w.id)))?;
    Some((sat(w.height), sat(w.width)))
}

fn sat(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

/// `(is_floating, floating_visible, focused_floating)` for `pane`.
///
/// Matches the trait contract: a tiled or unknown pane yields
/// `(false, false, None)`. For a floating pane, tmux never hides floating
/// panes, so visibility is true, and the focused floating pane is the active
/// floating pane of the pane's own window (if any).
pub fn floating_visibility(panes: &[PaneIn], pane: u32) -> (bool, bool, Option<PaneRef>) {
    let Some(p) = find_pane(panes, pane) else {
        return (false, false, None);
    };
    if !p.floating {
        return (false, false, None);
    }
    let focused = panes
        .iter()
        .filter(|q| q.window_id == p.window_id && q.floating && q.active)
        .min_by_key(|q| q.id)
        .map(|q| PaneRef::terminal(q.id));
    (true, true, focused)
}

/// The row for pane id `pane`.
pub fn find_pane(panes: &[PaneIn], pane: u32) -> Option<&PaneIn> {
    panes.iter().find(|p| p.id == pane)
}

/// The window id holding pane id `pane`.
pub fn window_of(panes: &[PaneIn], pane: u32) -> Option<u32> {
    find_pane(panes, pane).map(|p| p.window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, index: u32, flags: (bool, bool, bool), size: (u32, u32)) -> WindowIn {
        WindowIn {
            id,
            index,
            name: format!("w{id}"),
            active: flags.0,
            bell: flags.1,
            zoomed: flags.2,
            width: size.0,
            height: size.1,
        }
    }

    fn pane(id: u32, win: u32, active: bool, geo: (u32, u32, u32, u32), fz: (bool, i32)) -> PaneIn {
        PaneIn {
            id,
            window_id: win,
            active,
            title: format!("t{id}"),
            current_command: "bash".into(),
            current_path: "/home/x".into(),
            dead: false,
            left: geo.0,
            top: geo.1,
            width: geo.2,
            height: geo.3,
            floating: fz.0,
            z: fz.1,
        }
    }

    #[test]
    fn floating_window() {
        let ws = [win(0, 0, (true, false, false), (100, 29))];
        let ps = [
            pane(1, 0, false, (4, 2, 50, 7), (true, 0)),
            pane(0, 0, true, (0, 0, 100, 29), (false, 2)),
        ];
        let s = snapshot(&ws, &ps);
        assert_eq!(s.tabs.len(), 1);
        let t = &s.tabs[0];
        assert_eq!(t.tab_id, 0);
        assert!(t.floating_panes_visible);
        assert_eq!(t.panes_to_hide, 0);
        assert_eq!(t.panes.iter().map(|p| p.id).collect::<Vec<_>>(), [0, 1]);
        assert!(!t.panes[0].is_floating && t.panes[1].is_floating);
        let f = &t.panes[1];
        assert_eq!((f.x, f.y, f.cols, f.rows), (4, 2, 50, 7));
        assert_eq!(t.panes[0].cwd, "/home/x");
        assert_eq!(t.panes[0].command, "bash");
        assert!(t.panes[0].agent.is_none() && !t.panes[0].is_plugin);
    }

    #[test]
    fn two_windows_bell() {
        let ws = [
            win(5, 1, (true, true, false), (80, 24)),
            win(3, 0, (false, false, false), (80, 24)),
        ];
        let ps = [
            pane(2, 5, true, (0, 0, 80, 24), (false, 0)),
            pane(1, 3, true, (0, 0, 80, 24), (false, 0)),
        ];
        let s = snapshot(&ws, &ps);
        assert_eq!(s.tabs.iter().map(|t| t.tab_id).collect::<Vec<_>>(), [3, 5]);
        assert_eq!(s.tabs[1].position, 1);
        assert!(s.tabs[1].active && s.tabs[1].has_bell);
        assert!(!s.tabs[0].active && !s.tabs[0].has_bell);
        assert!(!s.tabs[0].floating_panes_visible);
        assert_eq!(session_size(&ws), Some((24, 80)));
    }

    #[test]
    fn zoomed_window() {
        let ws = [win(0, 0, (true, false, true), (100, 30))];
        let ps = [
            pane(0, 0, true, (0, 0, 100, 30), (false, 0)),
            pane(2, 0, false, (0, 0, 50, 7), (false, 0)),
        ];
        let t = &snapshot(&ws, &ps).tabs[0];
        assert!(t.fullscreen_active);
        assert!(t.panes[0].is_fullscreen);
        assert!(!t.panes[1].is_fullscreen);
        assert_eq!((t.panes[1].cols, t.panes[1].rows), (50, 7));
    }

    #[test]
    fn dead_pane() {
        let ws = [win(0, 0, (true, false, false), (80, 24))];
        let mut p = pane(0, 0, true, (0, 0, 80, 24), (false, 0));
        p.dead = true;
        let s = snapshot(&ws, &[p]);
        assert!(s.tabs[0].panes[0].exited);
        assert!(!s.tabs[0].panes[0].is_fullscreen);
    }

    #[test]
    fn pre_37_tiled_only() {
        let ws = [win(0, 0, (true, false, false), (80, 24))];
        let ps = [
            pane(0, 0, true, (0, 0, 40, 24), (false, 0)),
            pane(1, 0, false, (41, 0, 39, 24), (false, 0)),
        ];
        let t = &snapshot(&ws, &ps).tabs[0];
        assert!(!t.floating_panes_visible);
        assert!(t.panes.iter().all(|p| !p.is_floating));
        assert_eq!(t.panes.iter().map(|p| p.id).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn deterministic_any_order() {
        let ws = [
            win(0, 0, (true, false, false), (100, 29)),
            win(1, 1, (false, false, false), (100, 29)),
        ];
        let ps = [
            pane(0, 0, true, (0, 0, 100, 29), (false, 2)),
            pane(1, 0, false, (4, 2, 50, 7), (true, 0)),
            pane(3, 0, false, (6, 3, 20, 5), (true, 1)),
            pane(2, 1, true, (0, 0, 100, 29), (false, 0)),
        ];
        let a = snapshot(&ws, &ps);
        let mut rw = ws.to_vec();
        rw.reverse();
        let mut rp = ps.to_vec();
        rp.reverse();
        assert_eq!(a, snapshot(&rw, &rp));
        assert_eq!(
            a.tabs[0].panes.iter().map(|p| p.id).collect::<Vec<_>>(),
            [0, 1, 3]
        );
    }

    #[test]
    fn size_fallback_and_saturation() {
        assert_eq!(session_size(&[]), None);
        let ws = [win(0, 0, (false, false, false), (70000, 5))];
        assert_eq!(session_size(&ws), Some((5, u16::MAX)));
    }

    #[test]
    fn floating_visibility_cases() {
        let ps = [
            pane(0, 0, false, (0, 0, 100, 29), (false, 2)),
            pane(1, 0, true, (4, 2, 50, 7), (true, 0)),
            pane(2, 1, true, (0, 0, 10, 10), (false, 0)),
        ];
        assert_eq!(floating_visibility(&ps, 0), (false, false, None));
        assert_eq!(
            floating_visibility(&ps, 1),
            (true, true, Some(PaneRef::terminal(1)))
        );
        assert_eq!(floating_visibility(&ps, 99), (false, false, None));
        assert_eq!(window_of(&ps, 2), Some(1));
        assert_eq!(window_of(&ps, 9), None);
        assert_eq!(find_pane(&ps, 1).map(|p| p.id), Some(1));
    }
}
