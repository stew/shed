use std::time::Instant;

use crate::capture::Capture;
use crate::filter::FilterSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

#[derive(Debug, Clone)]
pub enum BlockState {
    Running,
    Snapshotted,
    Done(i32),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub capture: Option<Capture>,
    pub pipeline: Vec<FilterSpec>,
    pub state: BlockState,
    pub last_touched: Instant,
}

impl Block {
    pub fn capture_size_bytes(&self) -> usize {
        self.capture
            .as_ref()
            .map_or(0, |c| c.stdout.len() + c.stderr.len())
    }
}
