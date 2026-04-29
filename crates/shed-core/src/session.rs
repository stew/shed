use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use crate::block::{Block, BlockId, BlockState};
use crate::capture::Capture;

pub const DEFAULT_CAPTURE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

pub struct Session {
    blocks: BTreeMap<BlockId, Block>,
    next_id: u64,
    names: HashMap<String, BlockId>,
    cursor: Option<BlockId>,
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
            blocks: BTreeMap::new(),
            next_id: 1,
            names: HashMap::new(),
            cursor: None,
            capture_budget_bytes: DEFAULT_CAPTURE_BUDGET_BYTES,
        }
    }

    pub fn add_block(&mut self, argv: Vec<String>) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        let block = Block {
            id,
            name: None,
            argv,
            capture: None,
            pipeline: Vec::new(),
            state: BlockState::Running,
            last_touched: Instant::now(),
        };
        self.blocks.insert(id, block);
        id
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(&id)
    }

    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.blocks.get_mut(&id)
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.blocks.values()
    }

    pub fn set_capture(&mut self, id: BlockId, capture: Capture) -> bool {
        match self.blocks.get_mut(&id) {
            Some(b) => {
                b.capture = Some(capture);
                b.last_touched = Instant::now();
                true
            }
            None => false,
        }
    }

    pub fn set_state(&mut self, id: BlockId, state: BlockState) -> bool {
        match self.blocks.get_mut(&id) {
            Some(b) => {
                b.state = state;
                b.last_touched = Instant::now();
                true
            }
            None => false,
        }
    }

    pub fn touch(&mut self, id: BlockId) {
        if let Some(b) = self.blocks.get_mut(&id) {
            b.last_touched = Instant::now();
        }
    }

    pub fn pin(&mut self, id: BlockId, name: String) -> bool {
        if !self.blocks.contains_key(&id) {
            return false;
        }
        if let Some(prev_owner) = self.names.get(&name).copied() {
            if prev_owner == id {
                return true;
            }
            if let Some(prev) = self.blocks.get_mut(&prev_owner) {
                prev.name = None;
            }
        }
        if let Some(block) = self.blocks.get_mut(&id) {
            if let Some(old_name) = block.name.take() {
                self.names.remove(&old_name);
            }
            block.name = Some(name.clone());
        }
        self.names.insert(name, id);
        true
    }

    pub fn unpin(&mut self, id: BlockId) {
        if let Some(b) = self.blocks.get_mut(&id) {
            if let Some(name) = b.name.take() {
                self.names.remove(&name);
            }
        }
    }

    pub fn lookup_by_name(&self, name: &str) -> Option<BlockId> {
        self.names.get(name).copied()
    }

    pub fn cursor(&self) -> Option<BlockId> {
        self.cursor
    }

    pub fn set_cursor(&mut self, id: Option<BlockId>) {
        self.cursor = id;
    }

    pub fn total_capture_bytes(&self) -> usize {
        self.blocks.values().map(|b| b.capture_size_bytes()).sum()
    }

    /// Evict captures from unpinned blocks (oldest `last_touched` first) until
    /// total bytes is under budget. Pinned blocks count toward the budget but
    /// are never evicted; if pinned alone exceed the budget, the budget is
    /// violated rather than enforced.
    pub fn evict_to_fit(&mut self) {
        if self.total_capture_bytes() <= self.capture_budget_bytes {
            return;
        }
        let mut candidates: Vec<(BlockId, Instant)> = self
            .blocks
            .values()
            .filter(|b| b.name.is_none() && b.capture.is_some())
            .map(|b| (b.id, b.last_touched))
            .collect();
        candidates.sort_by_key(|(_, t)| *t);

        for (id, _) in candidates {
            if self.total_capture_bytes() <= self.capture_budget_bytes {
                break;
            }
            if let Some(b) = self.blocks.get_mut(&id) {
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
            truncated: false,
            snapshotted: false,
        }
    }

    #[test]
    fn ids_are_monotonic() {
        let mut s = Session::new();
        assert_eq!(s.add_block(vec!["a".into()]), BlockId(1));
        assert_eq!(s.add_block(vec!["b".into()]), BlockId(2));
        assert_eq!(s.add_block(vec!["c".into()]), BlockId(3));
    }

    #[test]
    fn pin_and_lookup() {
        let mut s = Session::new();
        let id = s.add_block(vec!["a".into()]);
        assert!(s.pin(id, "logs".into()));
        assert_eq!(s.lookup_by_name("logs"), Some(id));
        assert_eq!(s.block(id).unwrap().name.as_deref(), Some("logs"));
    }

    #[test]
    fn pin_overwrites_previous_owner() {
        let mut s = Session::new();
        let a = s.add_block(vec!["a".into()]);
        let b = s.add_block(vec!["b".into()]);
        assert!(s.pin(a, "x".into()));
        assert!(s.pin(b, "x".into()));
        assert_eq!(s.lookup_by_name("x"), Some(b));
        assert!(s.block(a).unwrap().name.is_none());
    }

    #[test]
    fn unpin_removes_name() {
        let mut s = Session::new();
        let id = s.add_block(vec!["a".into()]);
        s.pin(id, "name".into());
        s.unpin(id);
        assert_eq!(s.lookup_by_name("name"), None);
        assert!(s.block(id).unwrap().name.is_none());
    }

    #[test]
    fn pin_unknown_block_fails() {
        let mut s = Session::new();
        assert!(!s.pin(BlockId(99), "x".into()));
    }

    #[test]
    fn evict_drops_oldest_unpinned_first() {
        let mut s = Session::new();
        s.capture_budget_bytes = 100;
        let a = s.add_block(vec!["a".into()]);
        s.set_capture(a, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let b = s.add_block(vec!["b".into()]);
        s.set_capture(b, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let c = s.add_block(vec!["c".into()]);
        s.set_capture(c, cap_of_size(50));

        s.evict_to_fit();
        assert!(s.block(a).unwrap().capture.is_none());
        assert!(s.block(b).unwrap().capture.is_some());
        assert!(s.block(c).unwrap().capture.is_some());
    }

    #[test]
    fn evict_skips_pinned_blocks() {
        let mut s = Session::new();
        s.capture_budget_bytes = 100;
        let a = s.add_block(vec!["a".into()]);
        s.set_capture(a, cap_of_size(50));
        s.pin(a, "saved".into());
        thread::sleep(Duration::from_millis(2));
        let b = s.add_block(vec!["b".into()]);
        s.set_capture(b, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let c = s.add_block(vec!["c".into()]);
        s.set_capture(c, cap_of_size(50));

        s.evict_to_fit();
        assert!(s.block(a).unwrap().capture.is_some(), "pinned never evicts");
        assert!(s.block(b).unwrap().capture.is_none(), "oldest unpinned evicts");
        assert!(s.block(c).unwrap().capture.is_some());
    }

    #[test]
    fn touch_updates_lru_order() {
        let mut s = Session::new();
        s.capture_budget_bytes = 100;
        let a = s.add_block(vec!["a".into()]);
        s.set_capture(a, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        let b = s.add_block(vec!["b".into()]);
        s.set_capture(b, cap_of_size(50));
        thread::sleep(Duration::from_millis(2));
        // Touch `a` so it becomes more recent than `b`.
        s.touch(a);
        thread::sleep(Duration::from_millis(2));
        let c = s.add_block(vec!["c".into()]);
        s.set_capture(c, cap_of_size(50));

        s.evict_to_fit();
        assert!(s.block(a).unwrap().capture.is_some(), "touched stays alive");
        assert!(s.block(b).unwrap().capture.is_none(), "untouched oldest evicts");
        assert!(s.block(c).unwrap().capture.is_some());
    }
}
