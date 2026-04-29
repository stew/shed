//! Core data model for shed — an interactive shell with retroactive pipelines.

pub mod filter;
pub mod value;

pub use filter::{Filter, FilterError, FilterSpec, PipelineValue, Predicate};
pub use value::Value;
