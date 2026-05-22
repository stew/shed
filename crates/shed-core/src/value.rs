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
    /// An absolute instant in time. Produced by the `parse-time` filter,
    /// which combines and parses date/time columns. Sorts chronologically
    /// (it's a real `Ord` instant); the TUI renders it as relative time
    /// ("3 minutes ago") while keeping a canonical RFC 3339 form for
    /// copying, export, and dedup keys.
    DateTime(jiff::Timestamp),
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
            Value::DateTime(_) => "datetime",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Record(_) => "record",
        }
    }

    /// Convert into a `serde_json::Value` for export / for piping into
    /// downstream JSON-consuming tools. Lossy for [`Value::DateTime`]
    /// (serialized as RFC 3339 string) and [`Value::Bytes`] (lossy UTF-8
    /// → string).
    pub fn to_json(self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(b),
            Value::Int(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::Float(f) => serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::String(s) => serde_json::Value::String(s),
            Value::DateTime(ts) => serde_json::Value::String(ts.to_string()),
            Value::Bytes(b) => serde_json::Value::String(String::from_utf8_lossy(&b).to_string()),
            Value::List(items) => {
                serde_json::Value::Array(items.into_iter().map(Value::to_json).collect())
            }
            Value::Record(r) => {
                let mut map = serde_json::Map::with_capacity(r.len());
                for (k, v) in r {
                    map.insert(k, v.to_json());
                }
                serde_json::Value::Object(map)
            }
        }
    }
}
