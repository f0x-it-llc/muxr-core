//! Per-pane agent state shared by the tmux watcher and the backend.
//!
//! Rows are keyed by the neutral `u32` pane id. Every change is stamped with a
//! monotonic sequence number so readers can order updates and tell a fresh
//! completion from one already seen.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::multiplexer::types::{AgentMark, AgentStatus};

/// One pane's agent record.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub status: AgentStatus,
    pub kind: String,
    pub display_kind: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub tab_id: u64,
    pub tab_name: String,
    /// Sequence of the last change; stamped by [`AgentState::set_pane`].
    pub change_seq: u64,
    /// Sequence at which the status last became `Done`.
    pub completion_seq: Option<u64>,
    pub last_output: Option<Instant>,
    pub session: String,
}

#[derive(Debug, Default)]
struct Inner {
    rows: HashMap<u32, AgentRow>,
    seq: u64,
}

/// Thread-safe table of agent rows plus the sequence counter.
#[derive(Debug, Default)]
pub struct AgentState {
    inner: Mutex<Inner>,
}

impl AgentState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Store `row` for `pane`, stamping the next `change_seq` (returned).
    ///
    /// `completion_seq` is set to the new sequence when the status becomes
    /// `Done` from anything else, kept while it stays `Done`, and cleared
    /// otherwise. `last_output` carries over from the previous row when the
    /// new one has none.
    pub fn set_pane(&self, pane: u32, mut row: AgentRow) -> u64 {
        let mut inner = self.lock();
        inner.seq += 1;
        let seq = inner.seq;
        row.change_seq = seq;
        let previous = inner.rows.get(&pane);
        row.completion_seq = if row.status == AgentStatus::Done {
            match previous {
                Some(prev) if prev.status == AgentStatus::Done => prev.completion_seq.or(Some(seq)),
                _ => Some(seq),
            }
        } else {
            None
        };
        if row.last_output.is_none() {
            row.last_output = previous.and_then(|p| p.last_output);
        }
        inner.rows.insert(pane, row);
        seq
    }

    /// Record output activity for `pane`; ignored when the pane has no row.
    pub fn touch_output(&self, pane: u32, at: Instant) {
        if let Some(row) = self.lock().rows.get_mut(&pane) {
            row.last_output = Some(at);
        }
    }

    /// Drop `pane`; true when it had a row.
    pub fn remove_pane(&self, pane: u32) -> bool {
        self.lock().rows.remove(&pane).is_some()
    }

    /// Drop every row of `session`, returning the removed pane ids (sorted).
    pub fn remove_session(&self, session: &str) -> Vec<u32> {
        let mut inner = self.lock();
        let mut removed: Vec<u32> = inner
            .rows
            .iter()
            .filter(|(_, row)| row.session == session)
            .map(|(pane, _)| *pane)
            .collect();
        removed.sort_unstable();
        for pane in &removed {
            inner.rows.remove(pane);
        }
        removed
    }

    /// Every row of `session`, sorted by pane id.
    pub fn rows_for_session(&self, session: &str) -> Vec<(u32, AgentRow)> {
        let mut rows: Vec<(u32, AgentRow)> = self
            .lock()
            .rows
            .iter()
            .filter(|(_, row)| row.session == session)
            .map(|(pane, row)| (*pane, row.clone()))
            .collect();
        rows.sort_by_key(|(pane, _)| *pane);
        rows
    }

    /// The agent mark for `pane`, if it has a row.
    pub fn mark(&self, pane: u32) -> Option<AgentMark> {
        self.lock().rows.get(&pane).map(|row| AgentMark {
            kind: row.kind.clone(),
            name: row.display_kind.clone(),
            status: row.status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, status: AgentStatus) -> AgentRow {
        AgentRow {
            status,
            kind: "claude".into(),
            display_kind: Some("Claude".into()),
            title: None,
            summary: None,
            tab_id: 1,
            tab_name: "main".into(),
            change_seq: 0,
            completion_seq: None,
            last_output: None,
            session: session.into(),
        }
    }

    #[test]
    fn set_pane_stamps_increasing_sequence() {
        let state = AgentState::new();
        let a = state.set_pane(1, row("s", AgentStatus::Working));
        let b = state.set_pane(2, row("s", AgentStatus::Working));
        let c = state.set_pane(1, row("s", AgentStatus::Blocked));
        assert!(a < b && b < c);
        let rows = state.rows_for_session("s");
        assert_eq!(rows[0].1.change_seq, c);
        assert_eq!(rows[1].1.change_seq, b);
    }

    #[test]
    fn completion_seq_follows_done_transitions() {
        let state = AgentState::new();
        state.set_pane(1, row("s", AgentStatus::Working));
        assert_eq!(state.rows_for_session("s")[0].1.completion_seq, None);
        let done = state.set_pane(1, row("s", AgentStatus::Done));
        assert_eq!(state.rows_for_session("s")[0].1.completion_seq, Some(done));
        let again = state.set_pane(1, row("s", AgentStatus::Done));
        assert!(again > done);
        assert_eq!(state.rows_for_session("s")[0].1.completion_seq, Some(done));
        state.set_pane(1, row("s", AgentStatus::Working));
        assert_eq!(state.rows_for_session("s")[0].1.completion_seq, None);
        let redone = state.set_pane(1, row("s", AgentStatus::Done));
        assert_eq!(
            state.rows_for_session("s")[0].1.completion_seq,
            Some(redone)
        );
    }

    #[test]
    fn touch_output_sets_and_survives_updates() {
        let state = AgentState::new();
        let at = Instant::now();
        state.touch_output(9, at);
        assert!(state.rows_for_session("s").is_empty());
        state.set_pane(9, row("s", AgentStatus::Working));
        assert_eq!(state.rows_for_session("s")[0].1.last_output, None);
        state.touch_output(9, at);
        assert_eq!(state.rows_for_session("s")[0].1.last_output, Some(at));
        state.set_pane(9, row("s", AgentStatus::Idle));
        assert_eq!(state.rows_for_session("s")[0].1.last_output, Some(at));
    }

    #[test]
    fn remove_pane_reports_presence() {
        let state = AgentState::new();
        state.set_pane(1, row("s", AgentStatus::Idle));
        assert!(state.remove_pane(1));
        assert!(!state.remove_pane(1));
        assert!(state.mark(1).is_none());
    }

    #[test]
    fn remove_session_returns_its_panes_only() {
        let state = AgentState::new();
        state.set_pane(3, row("a", AgentStatus::Idle));
        state.set_pane(1, row("a", AgentStatus::Idle));
        state.set_pane(2, row("b", AgentStatus::Idle));
        assert_eq!(state.remove_session("a"), vec![1, 3]);
        assert!(state.rows_for_session("a").is_empty());
        assert_eq!(state.rows_for_session("b").len(), 1);
        assert!(state.remove_session("a").is_empty());
    }

    #[test]
    fn rows_for_session_filters_and_sorts() {
        let state = AgentState::new();
        state.set_pane(5, row("a", AgentStatus::Idle));
        state.set_pane(2, row("a", AgentStatus::Idle));
        state.set_pane(4, row("b", AgentStatus::Idle));
        let panes: Vec<u32> = state
            .rows_for_session("a")
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(panes, vec![2, 5]);
    }

    #[test]
    fn mark_maps_kind_name_and_status() {
        let state = AgentState::new();
        assert!(state.mark(1).is_none());
        state.set_pane(1, row("s", AgentStatus::Blocked));
        let mark = state.mark(1).unwrap();
        assert_eq!(mark.kind, "claude");
        assert_eq!(mark.name.as_deref(), Some("Claude"));
        assert_eq!(mark.status, AgentStatus::Blocked);
    }
}
