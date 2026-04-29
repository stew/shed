use bytes::Bytes;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// A piece of structured data flowing through a filter pipeline.
///
/// `Value` is a nushell-flavored sum type. Most pipelines produce a
/// `List(Vec<Record(...)>)` — a sequence of rows with named columns.
/// Scalars (`Int`, `String`, etc.) appear when a parser produces them
/// (e.g., `from-json` on a top-level number) or when a filter cell holds
/// one. Column order in `Record` is preserved via [`IndexMap`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// Raw bytes — used for binary cell values; not produced by any v0
    /// parser but reserved for future use.
    Bytes(Bytes),
    List(Vec<Value>),
    /// Ordered key-value record. The key order is preserved across
    /// rendering, sort, and projection.
    Record(IndexMap<String, Value>),
}

impl Value {
    /// Stable type tag suitable for error messages and debug output.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Record(_) => "record",
        }
    }
}
