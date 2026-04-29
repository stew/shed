//! Core data model for shed — an interactive shell with retroactive pipelines.

pub mod capture;
pub mod filter;
pub mod value;

pub use capture::Capture;
pub use filter::{Filter, FilterError, FilterSpec, PipelineValue, Predicate};
pub use value::Value;
