use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use crate::capture::Capture;
use crate::shed::{Shed, ShedId, ShedState};

/// Default total capture-byte budget for an entire [`Session`], in bytes
/// (256 MiB). When the sum of unpinned captures exceeds this, the
/// oldest-touched ones are evicted by [`Session::evict_to_fit`]. Pinned
/// captures count toward the budget but are never evicted.
pub const DEFAULT_CAPTURE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// All session-wide state: the ordered set of [`Shed`]s, a name registry,
/// the current cursor, and the capture-byte budget.
///
/// `Session` is pure data — it does not own running tasks, the TUI, or
/// the terminal. The binary crate maintains its own `App` struct that
/// wraps a `Session` plus I/O state.
///
/// `Clone` is derived so the binary crate can take cheap structural
/// snapshots for undo / redo. `Capture` clones share underlying bytes
/// via `bytes::Bytes` refcounting, so a snapshot of a session with
/// large captures is small in additional memory.
#[derive(Clone)]
pub struct Session {
    sheds: BTreeMap<ShedId, Shed>,
    next_id: u64,
    names: HashMap<String, ShedId>,
    cursor: Option<ShedId>,
    /// Total byte budget for unpinned captures across the session.
    /// Initialized to [`DEFAULT_CAPTURE_BUDGET_BYTES`].
    pub capture_budget_bytes: usize,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            sheds: BTreeMap::new(),
            next_id: 1,
            names: HashMap::new(),
            cursor: None,
            capture_budget_bytes: DEFAULT_CAPTURE_BUDGET_BYTES,
        }
    }

    /// Append a new shed in `Running` state and return its id. The id is
    /// monotonic — `add_shed` always returns a strictly greater id than
    /// any previous call on this session.
    pub fn add_shed(&mut self, argv: Vec<String>) -> ShedId {
        let id = ShedId(self.next_id);
        self.next_id += 1;
        let shed = Shed {
            id,
            name: None,
            argv,
            capture: None,
            pipeline: Vec::new(),
            state: ShedState::Running,
            last_touched: Instant::now(),
            pre_text: None,
            post_text: None,
            outputs: indexmap::IndexMap::new(),
            output_values: std::collections::HashMap::new(),
            pipe_cache: std::cell::RefCell::new(std::collections::HashMap::new()),
        };
        self.sheds.insert(id, shed);
        id
    }

    pub fn shed(&self, id: ShedId) -> Option<&Shed> {
        self.sheds.get(&id)
    }

    pub fn shed_mut(&mut self, id: ShedId) -> Option<&mut Shed> {
        self.sheds.get_mut(&id)
    }

    /// Iterate sheds in monotonic id order (oldest first).
    pub fn sheds(&self) -> impl Iterator<Item = &Shed> {
        self.sheds.values()
    }

    /// Attach a captured stdout/stderr to a shed and bump its
    /// `last_touched` timestamp. Returns `false` if the id is unknown.
    pub fn set_capture(&mut self, id: ShedId, capture: Capture) -> bool {
        match self.sheds.get_mut(&id) {
            Some(b) => {
                b.capture = Some(capture);
                b.last_touched = Instant::now();
                true
            }
            None => false,
        }
    }

    pub fn set_state(&mut self, id: ShedId, state: ShedState) -> bool {
        match self.sheds.get_mut(&id) {
            Some(b) => {
                b.state = state;
                b.last_touched = Instant::now();
                true
            }
            None => false,
        }
    }

    /// Bump a shed's `last_touched` timestamp without mutating anything
    /// else. Used when the user opens or pipes through a shed — it
    /// signals "still relevant" to the LRU evictor.
    pub fn touch(&mut self, id: ShedId) {
        if let Some(b) = self.sheds.get_mut(&id) {
            b.last_touched = Instant::now();
        }
    }

    /// Pin a shed under `name`. If `name` already maps to another shed,
    /// that mapping is transferred (the previous owner's `name` becomes
    /// `None`). Pinned sheds count toward the capture budget but never
    /// evict. Returns `false` if `id` is unknown.
    pub fn pin(&mut self, id: ShedId, name: String) -> bool {
        if !self.sheds.contains_key(&id) {
            return false;
        }
        if let Some(prev_owner) = self.names.get(&name).copied() {
            if prev_owner == id {
                return true;
            }
            if let Some(prev) = self.sheds.get_mut(&prev_owner) {
                prev.name = None;
            }
        }
        if let Some(shed) = self.sheds.get_mut(&id) {
            if let Some(old_name) = shed.name.take() {
                self.names.remove(&old_name);
            }
            shed.name = Some(name.clone());
        }
        self.names.insert(name, id);
        true
    }

    pub fn unpin(&mut self, id: ShedId) {
        if let Some(b) = self.sheds.get_mut(&id)
            && let Some(name) = b.name.take()
        {
            self.names.remove(&name);
        }
    }

    /// Remove a shed from the session entirely. Drops the names entry if
    /// the shed was pinned, clears the cursor if it pointed at this id.
    /// Returns the removed shed, or `None` if the id was unknown.
    pub fn remove_shed(&mut self, id: ShedId) -> Option<Shed> {
        let shed = self.sheds.remove(&id)?;
        if let Some(name) = &shed.name {
            self.names.remove(name);
        }
        if self.cursor == Some(id) {
            self.cursor = None;
        }
        Some(shed)
    }

    pub fn lookup_by_name(&self, name: &str) -> Option<ShedId> {
        self.names.get(name).copied()
    }

    pub fn cursor(&self) -> Option<ShedId> {
        self.cursor
    }

    pub fn set_cursor(&mut self, id: Option<ShedId>) {
        self.cursor = id;
    }

    pub fn total_capture_bytes(&self) -> usize {
        self.sheds.values().map(|b| b.capture_size_bytes()).sum()
    }

    /// Evict captures from unpinned sheds (oldest `last_touched` first)
    /// until total bytes is under [`Session::capture_budget_bytes`].
    ///
    /// Pinned sheds count toward the budget but are never evicted; if
    /// the pinned set alone exceeds the budget, the budget is violated
    /// rather than enforced. Eviction sets the shed's
    /// [`Shed::capture`] to `None`; the shed itself stays in the
    /// session (its id and pipeline are preserved). The user sees a cold
    /// `○` glyph in the UI.
    pub fn evict_to_fit(&mut self) {
        if self.total_capture_bytes() <= self.capture_budget_bytes {
            return;
        }
        let mut candidates: Vec<(ShedId, Instant)> = self
            .sheds
            .values()
            .filter(|b| b.name.is_none() && b.capture.is_some())
            .map(|b| (b.id, b.last_touched))
            .collect();
        candidates.sort_by_key(|(_, t)| *t);

        for (id, _) in candidates {
            if self.total_capture_bytes() <= self.capture_budget_bytes {
                break;
            }
            if let Some(b) = self.sheds.get_mut(&id) {
                b.capture = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::thread;
    use std::time::Duration;

    fn cap_of_size(n: usize) -> Capture {
        Capture {
            stdout: Bytes::from(vec![0u8; n]),
            stderr: Bytes::new(),
            exit_code: Some(0),
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            finished_wall: Some(jiff::Timestamp::now()),
            truncated: false,
            snapshotted: false,
            structured: None,
        }
    }

    #[test]
    fn ids_are_monotonic() {
        let mut s = Session::new();
        assert_eq!(s.add_shed(vec!["a".into()]), ShedId(1));
        assert_eq!(s.add_shed(vec!["b".into()]), ShedId(2));
        assert_eq!(s.add_shed(vec!["c".into()]), ShedId(3));
    }

    #[test]
    fn pin_and_lookup() {
        let mut s = Session::new();
        let id = s.add_shed(vec!["a".into()]);
        assert!(s.pin(id, "logs".into()));
        assert_eq!(s.lookup_by_name("logs"), Some(id));
        assert_eq!(s.shed(id).unwrap().name.as_deref(), Some("logs"));
    }

    #[test]
    fn pin_overwrites_previous_owner() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["a".into()]);
        let b = s.add_shed(vec!["b".into()]);
        assert!(s.pin(a, "x".into()));
        assert!(s.pin(b, "x".into()));
        assert_eq!(s.lookup_by_name("x"), Some(b));
        assert!(s.shed(a).unwrap().name.is_none());
    }

    #[test]
    fn unpin_removes_name() {
        let mut s = Session::new();
        let id = s.add_shed(vec!["a".into()]);
        s.pin(id, "name".into());
        s.unpin(id);
        assert_eq!(s.lookup_by_name("name"), None);
        assert!(s.shed(id).unwrap().name.is_none());
    }

    #[test]
    fn pin_unknown_shed_fails() {
        let mut s = Session::new();
        assert!(!s.pin(ShedId(99), "x".into()));
    }

    #[test]
    fn evict_drops_oldest_unpinned_first() {
        let mut s = Session::new();
        s.capture_budget_bytes = 100;
        let a = s.add_shed(vec!["a".into()]);
        s.set_capture(a, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let b = s.add_shed(vec!["b".into()]);
        s.set_capture(b, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let c = s.add_shed(vec!["c".into()]);
        s.set_capture(c, cap_of_size(50));

        s.evict_to_fit();
        assert!(s.shed(a).unwrap().capture.is_none());
        assert!(s.shed(b).unwrap().capture.is_some());
        assert!(s.shed(c).unwrap().capture.is_some());
    }

    #[test]
    fn evict_skips_pinned_sheds() {
        let mut s = Session::new();
        s.capture_budget_bytes = 100;
        let a = s.add_shed(vec!["a".into()]);
        s.set_capture(a, cap_of_size(50));
        s.pin(a, "saved".into());
        thread::sleep(Duration::from_millis(2));
        let b = s.add_shed(vec!["b".into()]);
        s.set_capture(b, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let c = s.add_shed(vec!["c".into()]);
        s.set_capture(c, cap_of_size(50));

        s.evict_to_fit();
        assert!(s.shed(a).unwrap().capture.is_some(), "pinned never evicts");
        assert!(
            s.shed(b).unwrap().capture.is_none(),
            "oldest unpinned evicts"
        );
        assert!(s.shed(c).unwrap().capture.is_some());
    }

    #[test]
    fn remove_shed_drops_capture_and_name() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["a".into()]);
        s.pin(a, "saved".into());
        let removed = s.remove_shed(a).expect("removed");
        assert_eq!(removed.id, a);
        assert!(s.shed(a).is_none());
        assert!(s.lookup_by_name("saved").is_none());
    }

    #[test]
    fn remove_shed_clears_cursor_when_pointing_at_it() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["a".into()]);
        s.set_cursor(Some(a));
        s.remove_shed(a);
        assert!(s.cursor().is_none());
    }

    #[test]
    fn remove_unknown_shed_returns_none() {
        let mut s = Session::new();
        assert!(s.remove_shed(ShedId(99)).is_none());
    }

    #[test]
    fn touch_updates_lru_order() {
        let mut s = Session::new();
        s.capture_budget_bytes = 100;
        let a = s.add_shed(vec!["a".into()]);
        s.set_capture(a, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let b = s.add_shed(vec!["b".into()]);
        s.set_capture(b, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        // Touch `a` so it becomes more recent than `b`.
        s.touch(a);
        thread::sleep(Duration::from_millis(2));
        let c = s.add_shed(vec!["c".into()]);
        s.set_capture(c, cap_of_size(50));

        s.evict_to_fit();
        assert!(s.shed(a).unwrap().capture.is_some(), "touched stays alive");
        assert!(
            s.shed(b).unwrap().capture.is_none(),
            "untouched oldest evicts"
        );
        assert!(s.shed(c).unwrap().capture.is_some());
    }
}
