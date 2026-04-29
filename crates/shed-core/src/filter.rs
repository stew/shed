use bytes::Bytes;
use indexmap::IndexMap;
use regex::Regex;
use thiserror::Error;

use crate::value::Value;

#[derive(Debug, Clone)]
pub enum PipelineValue {
    Bytes(Bytes),
    Structured(Value),
}

#[derive(Debug, Clone)]
pub enum FilterSpec {
    FromLines,
    FromFields,
    FromCsv { delim: char, has_header: bool },
    FromJson,
    FromRegex { pattern: String },
    Where { predicate: Predicate },
    Select { columns: Vec<String> },
    Drop { columns: Vec<String> },
    Take { n: usize },
    Skip { n: usize },
    SortBy { keys: Vec<SortKey> },
    Uniq { by: Option<Vec<String>> },
    Count,
}

#[derive(Debug, Clone)]
pub struct SortKey {
    pub column: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub enum Predicate {
    Matches { column: String, pattern: String },
    Contains { column: String, substring: String },
    Compare { column: String, op: CompareOp, value: Value },
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Error)]
pub enum FilterError {
    #[error("filter expected bytes input, got structured value")]
    ExpectedBytes,
    #[error("filter expected structured input, got bytes")]
    ExpectedStructured,
    #[error("filter expected a list, got {0}")]
    ExpectedList(&'static str),
    #[error("filter expected a record, got {0}")]
    ExpectedRecord(&'static str),
    #[error("unknown column: {0}")]
    UnknownColumn(String),
    #[error("invalid UTF-8 in input")]
    InvalidUtf8,
    #[error("invalid regex `{pattern}`: {error}")]
    BadRegex { pattern: String, error: String },
    #[error("failed to parse {format}: {error}")]
    ParseError { format: &'static str, error: String },
    #[error("type mismatch on column `{column}`: expected {expected}, got {got}")]
    TypeMismatch {
        column: String,
        expected: &'static str,
        got: &'static str,
    },
}

pub trait Filter {
    fn apply(&self, input: PipelineValue) -> Result<PipelineValue, FilterError>;
}

impl Filter for FilterSpec {
    fn apply(&self, input: PipelineValue) -> Result<PipelineValue, FilterError> {
        match self {
            FilterSpec::FromLines => apply_from_lines(input),
            FilterSpec::FromFields => apply_from_fields(input),
            FilterSpec::FromCsv { delim, has_header } => apply_from_csv(input, *delim, *has_header),
            FilterSpec::FromJson => apply_from_json(input),
            FilterSpec::FromRegex { pattern } => apply_from_regex(input, pattern),
            FilterSpec::Where { predicate } => apply_where(input, predicate),
            FilterSpec::Select { columns } => apply_select(input, columns),
            FilterSpec::Drop { columns } => apply_drop(input, columns),
            FilterSpec::Take { n } => apply_take(input, *n),
            FilterSpec::Skip { n } => apply_skip(input, *n),
            FilterSpec::SortBy { keys } => apply_sort_by(input, keys),
            FilterSpec::Uniq { by } => apply_uniq(input, by.as_deref()),
            FilterSpec::Count => apply_count(input),
        }
    }
}

fn require_bytes(input: PipelineValue) -> Result<Bytes, FilterError> {
    match input {
        PipelineValue::Bytes(b) => Ok(b),
        PipelineValue::Structured(_) => Err(FilterError::ExpectedBytes),
    }
}

fn require_list(input: PipelineValue) -> Result<Vec<Value>, FilterError> {
    let value = match input {
        PipelineValue::Structured(v) => v,
        PipelineValue::Bytes(_) => return Err(FilterError::ExpectedStructured),
    };
    match value {
        Value::List(items) => Ok(items),
        other => Err(FilterError::ExpectedList(other.type_name())),
    }
}

fn apply_from_lines(input: PipelineValue) -> Result<PipelineValue, FilterError> {
    let bytes = require_bytes(input)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| FilterError::InvalidUtf8)?;
    let records: Vec<Value> = text
        .lines()
        .map(|line| {
            let mut rec = IndexMap::with_capacity(1);
            rec.insert("line".to_string(), Value::String(line.to_string()));
            Value::Record(rec)
        })
        .collect();
    Ok(PipelineValue::Structured(Value::List(records)))
}

fn apply_from_fields(input: PipelineValue) -> Result<PipelineValue, FilterError> {
    let bytes = require_bytes(input)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| FilterError::InvalidUtf8)?;

    let lines_fields: Vec<Vec<&str>> = text
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    let max_fields = lines_fields.iter().map(|fs| fs.len()).max().unwrap_or(0);
    let columns: Vec<String> = (1..=max_fields).map(|i| format!("_{i}")).collect();

    let records: Vec<Value> = lines_fields
        .into_iter()
        .map(|fields| {
            let mut rec = IndexMap::with_capacity(max_fields);
            for (i, col) in columns.iter().enumerate() {
                let value = fields
                    .get(i)
                    .map(|s| Value::String((*s).to_string()))
                    .unwrap_or(Value::Null);
                rec.insert(col.clone(), value);
            }
            Value::Record(rec)
        })
        .collect();

    Ok(PipelineValue::Structured(Value::List(records)))
}

fn apply_where(input: PipelineValue, predicate: &Predicate) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    let kept = items
        .into_iter()
        .map(|item| predicate.evaluate(&item).map(|keep| (keep, item)))
        .filter_map(|res| match res {
            Ok((true, item)) => Some(Ok(item)),
            Ok((false, _)) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PipelineValue::Structured(Value::List(kept)))
}

fn apply_select(input: PipelineValue, columns: &[String]) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    let kept: Vec<Value> = items
        .into_iter()
        .map(|item| match item {
            Value::Record(r) => {
                let mut new_rec = IndexMap::with_capacity(columns.len());
                for col in columns {
                    let v = r.get(col).cloned().unwrap_or(Value::Null);
                    new_rec.insert(col.clone(), v);
                }
                Value::Record(new_rec)
            }
            other => other,
        })
        .collect();
    Ok(PipelineValue::Structured(Value::List(kept)))
}

fn apply_drop(input: PipelineValue, columns: &[String]) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    let drop_set: std::collections::HashSet<&str> =
        columns.iter().map(|s| s.as_str()).collect();
    let kept: Vec<Value> = items
        .into_iter()
        .map(|item| match item {
            Value::Record(r) => {
                let new_rec: IndexMap<String, Value> = r
                    .into_iter()
                    .filter(|(k, _)| !drop_set.contains(k.as_str()))
                    .collect();
                Value::Record(new_rec)
            }
            other => other,
        })
        .collect();
    Ok(PipelineValue::Structured(Value::List(kept)))
}

fn apply_take(input: PipelineValue, n: usize) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    Ok(PipelineValue::Structured(Value::List(
        items.into_iter().take(n).collect(),
    )))
}

fn apply_skip(input: PipelineValue, n: usize) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    Ok(PipelineValue::Structured(Value::List(
        items.into_iter().skip(n).collect(),
    )))
}

fn apply_from_csv(
    input: PipelineValue,
    delim: char,
    has_header: bool,
) -> Result<PipelineValue, FilterError> {
    let bytes = require_bytes(input)?;
    let mut builder = csv::ReaderBuilder::new();
    builder.has_headers(has_header);
    builder.flexible(true);
    if delim.is_ascii() {
        builder.delimiter(delim as u8);
    }
    let mut reader = builder.from_reader(bytes.as_ref());

    let headers: Vec<String> = if has_header {
        reader
            .headers()
            .map_err(|e| FilterError::ParseError {
                format: "csv",
                error: e.to_string(),
            })?
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };

    let mut records = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|e| FilterError::ParseError {
            format: "csv",
            error: e.to_string(),
        })?;
        let mut rec = IndexMap::with_capacity(record.len());
        for (i, field) in record.iter().enumerate() {
            let col = if has_header {
                headers
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("_{}", i + 1))
            } else {
                format!("_{}", i + 1)
            };
            rec.insert(col, Value::String(field.to_string()));
        }
        records.push(Value::Record(rec));
    }
    Ok(PipelineValue::Structured(Value::List(records)))
}

fn apply_from_json(input: PipelineValue) -> Result<PipelineValue, FilterError> {
    let bytes = require_bytes(input)?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| FilterError::ParseError {
            format: "json",
            error: e.to_string(),
        })?;
    let value = json_to_value(json);
    let list = match value {
        Value::List(items) => items,
        record @ Value::Record(_) => vec![record],
        scalar => {
            let mut rec = IndexMap::with_capacity(1);
            rec.insert("value".to_string(), scalar);
            vec![Value::Record(rec)]
        }
    };
    Ok(PipelineValue::Structured(Value::List(list)))
}

fn json_to_value(j: serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(arr) => Value::List(arr.into_iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => Value::Record(
            obj.into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
    }
}

fn apply_sort_by(input: PipelineValue, keys: &[SortKey]) -> Result<PipelineValue, FilterError> {
    let mut items = require_list(input)?;
    items.sort_by(|a, b| {
        use std::cmp::Ordering;
        for key in keys {
            let av = match a {
                Value::Record(r) => r.get(&key.column),
                _ => None,
            };
            let bv = match b {
                Value::Record(r) => r.get(&key.column),
                _ => None,
            };
            let ord = match (av, bv) {
                (Some(av), Some(bv)) => compare_values(av, bv).unwrap_or(Ordering::Equal),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            let ord = match key.direction {
                SortDirection::Asc => ord,
                SortDirection::Desc => ord.reverse(),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    Ok(PipelineValue::Structured(Value::List(items)))
}

fn apply_uniq(input: PipelineValue, by: Option<&[String]>) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let key = uniq_key(&item, by);
        if seen.insert(key) {
            kept.push(item);
        }
    }
    Ok(PipelineValue::Structured(Value::List(kept)))
}

fn uniq_key(record: &Value, by: Option<&[String]>) -> String {
    match record {
        Value::Record(r) => {
            let cols: Vec<&str> = match by {
                Some(c) => c.iter().map(String::as_str).collect(),
                None => r.keys().map(String::as_str).collect(),
            };
            cols.iter()
                .map(|col| format!("{col}={}", value_key_string(r.get(*col))))
                .collect::<Vec<_>>()
                .join("\0")
        }
        other => format!("scalar:{other:?}"),
    }
}

fn value_key_string(v: Option<&Value>) -> String {
    match v {
        None => "<missing>".into(),
        Some(Value::Null) => "<null>".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::Float(f)) => f.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => format!("{other:?}"),
    }
}

fn apply_count(input: PipelineValue) -> Result<PipelineValue, FilterError> {
    let items = require_list(input)?;
    let mut rec = IndexMap::with_capacity(1);
    rec.insert("count".to_string(), Value::Int(items.len() as i64));
    Ok(PipelineValue::Structured(Value::List(vec![Value::Record(rec)])))
}

fn apply_from_regex(input: PipelineValue, pattern: &str) -> Result<PipelineValue, FilterError> {
    let bytes = require_bytes(input)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| FilterError::InvalidUtf8)?;
    let regex = Regex::new(pattern).map_err(|e| FilterError::BadRegex {
        pattern: pattern.to_string(),
        error: e.to_string(),
    })?;

    let column_names: Vec<String> = regex
        .capture_names()
        .enumerate()
        .skip(1)
        .map(|(i, name)| match name {
            Some(n) => n.to_string(),
            None => format!("_{i}"),
        })
        .collect();

    let mut records = Vec::new();
    for line in text.lines() {
        let Some(captures) = regex.captures(line) else {
            continue;
        };
        let mut rec = IndexMap::with_capacity(column_names.len());
        for (i, name) in column_names.iter().enumerate() {
            let value = captures
                .get(i + 1)
                .map(|m| Value::String(m.as_str().to_string()))
                .unwrap_or(Value::Null);
            rec.insert(name.clone(), value);
        }
        records.push(Value::Record(rec));
    }
    Ok(PipelineValue::Structured(Value::List(records)))
}

impl Predicate {
    pub fn evaluate(&self, value: &Value) -> Result<bool, FilterError> {
        match self {
            Predicate::Matches { column, pattern } => {
                let field = lookup_column(value, column)?;
                let text = match field {
                    Value::String(s) => s.as_str(),
                    other => {
                        return Err(FilterError::TypeMismatch {
                            column: column.clone(),
                            expected: "string",
                            got: other.type_name(),
                        });
                    }
                };
                let regex = Regex::new(pattern).map_err(|e| FilterError::BadRegex {
                    pattern: pattern.clone(),
                    error: e.to_string(),
                })?;
                Ok(regex.is_match(text))
            }
            Predicate::Contains { column, substring } => {
                let field = lookup_column(value, column)?;
                let text = match field {
                    Value::String(s) => s.as_str(),
                    other => {
                        return Err(FilterError::TypeMismatch {
                            column: column.clone(),
                            expected: "string",
                            got: other.type_name(),
                        });
                    }
                };
                Ok(text.contains(substring))
            }
            Predicate::Compare { column, op, value: target } => {
                let field = lookup_column(value, column)?;
                let Some(ord) = compare_values(field, target) else {
                    return Err(FilterError::TypeMismatch {
                        column: column.clone(),
                        expected: target.type_name(),
                        got: field.type_name(),
                    });
                };
                use std::cmp::Ordering::*;
                Ok(match op {
                    CompareOp::Eq => ord == Equal,
                    CompareOp::Ne => ord != Equal,
                    CompareOp::Lt => ord == Less,
                    CompareOp::Le => matches!(ord, Less | Equal),
                    CompareOp::Gt => ord == Greater,
                    CompareOp::Ge => matches!(ord, Greater | Equal),
                })
            }
            Predicate::And(a, b) => Ok(a.evaluate(value)? && b.evaluate(value)?),
            Predicate::Or(a, b) => Ok(a.evaluate(value)? || b.evaluate(value)?),
            Predicate::Not(p) => Ok(!p.evaluate(value)?),
        }
    }
}

// Best-effort total ordering across Value types. Numeric variants compare
// numerically (with int↔float widening). Strings vs numerics try parsing
// the string side as a number — so `where _5 > 1000` works when from-fields
// has produced string columns. Mismatched types we can't reconcile return None.
fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use Value::*;
    use std::cmp::Ordering;
    match (a, b) {
        (Null, Null) => Some(Ordering::Equal),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)),
        (String(x), String(y)) => {
            if let (Ok(xn), Ok(yn)) = (x.parse::<f64>(), y.parse::<f64>()) {
                Some(xn.partial_cmp(&yn).unwrap_or(std::cmp::Ordering::Equal))
            } else {
                Some(x.cmp(y))
            }
        }
        (String(x), Int(y)) => x.parse::<i64>().ok().and_then(|n| Some(n.cmp(y))),
        (String(x), Float(y)) => x.parse::<f64>().ok().and_then(|n| n.partial_cmp(y)),
        (Int(x), String(y)) => y.parse::<i64>().ok().and_then(|n| Some(x.cmp(&n))),
        (Float(x), String(y)) => y.parse::<f64>().ok().and_then(|n| x.partial_cmp(&n)),
        _ => None,
    }
}

fn lookup_column<'a>(value: &'a Value, column: &str) -> Result<&'a Value, FilterError> {
    match value {
        Value::Record(map) => map
            .get(column)
            .ok_or_else(|| FilterError::UnknownColumn(column.to_string())),
        other => Err(FilterError::ExpectedRecord(other.type_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pipeline: &[FilterSpec], input: &[u8]) -> PipelineValue {
        let mut value = PipelineValue::Bytes(Bytes::copy_from_slice(input));
        for filter in pipeline {
            value = filter.apply(value).unwrap();
        }
        value
    }

    fn lines_of(value: PipelineValue) -> Vec<String> {
        match value {
            PipelineValue::Structured(Value::List(items)) => items
                .into_iter()
                .map(|item| match item {
                    Value::Record(r) => match r.get("line").unwrap() {
                        Value::String(s) => s.clone(),
                        _ => panic!("expected line:string"),
                    },
                    _ => panic!("expected record"),
                })
                .collect(),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn from_lines_then_where_matches() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::Matches {
                    column: "line".into(),
                    pattern: "^b".into(),
                },
            },
        ];
        let result = run(&pipeline, b"apple\nbanana\ncherry\nblueberry\n");
        assert_eq!(lines_of(result), vec!["banana", "blueberry"]);
    }

    #[test]
    fn where_with_and_or_not() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::And(
                    Box::new(Predicate::Matches {
                        column: "line".into(),
                        pattern: "berry".into(),
                    }),
                    Box::new(Predicate::Not(Box::new(Predicate::Matches {
                        column: "line".into(),
                        pattern: "^straw".into(),
                    }))),
                ),
            },
        ];
        let result = run(
            &pipeline,
            b"strawberry\nblueberry\nblackberry\nbanana\n",
        );
        assert_eq!(lines_of(result), vec!["blueberry", "blackberry"]);
    }

    #[test]
    fn unknown_column_errors() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::Matches {
                    column: "nope".into(),
                    pattern: ".".into(),
                },
            },
        ];
        let mut value = PipelineValue::Bytes(Bytes::from_static(b"a\nb\n"));
        for filter in &pipeline {
            match filter.apply(value) {
                Ok(v) => value = v,
                Err(FilterError::UnknownColumn(col)) => {
                    assert_eq!(col, "nope");
                    return;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        panic!("expected UnknownColumn error");
    }

    #[test]
    fn from_lines_rejects_structured_input() {
        let err = FilterSpec::FromLines
            .apply(PipelineValue::Structured(Value::Null))
            .unwrap_err();
        assert!(matches!(err, FilterError::ExpectedBytes));
    }

    #[test]
    fn where_rejects_bytes_input() {
        let err = FilterSpec::Where {
            predicate: Predicate::Matches {
                column: "line".into(),
                pattern: ".".into(),
            },
        }
        .apply(PipelineValue::Bytes(Bytes::from_static(b"x")))
        .unwrap_err();
        assert!(matches!(err, FilterError::ExpectedStructured));
    }

    #[test]
    fn from_fields_splits_whitespace_with_auto_names() {
        let result = FilterSpec::FromFields
            .apply(PipelineValue::Bytes(Bytes::from(
                "a b c\nd e f\ng h\n".to_string(),
            )))
            .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!("expected list"),
        };
        assert_eq!(items.len(), 3);
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(first.get("_1"), Some(&Value::String("a".into())));
        assert_eq!(first.get("_2"), Some(&Value::String("b".into())));
        assert_eq!(first.get("_3"), Some(&Value::String("c".into())));
        let third = match &items[2] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(third.get("_3"), Some(&Value::Null));
    }

    #[test]
    fn take_keeps_first_n() {
        let pipeline = vec![FilterSpec::FromLines, FilterSpec::Take { n: 2 }];
        let result = run(&pipeline, b"a\nb\nc\nd\n");
        assert_eq!(lines_of(result), vec!["a", "b"]);
    }

    #[test]
    fn skip_drops_first_n() {
        let pipeline = vec![FilterSpec::FromLines, FilterSpec::Skip { n: 2 }];
        let result = run(&pipeline, b"a\nb\nc\nd\n");
        assert_eq!(lines_of(result), vec!["c", "d"]);
    }

    #[test]
    fn select_keeps_columns_in_specified_order() {
        let pipeline = vec![
            FilterSpec::FromFields,
            FilterSpec::Select {
                columns: vec!["_3".into(), "_1".into()],
            },
        ];
        let result = run(&pipeline, b"a b c\n");
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        let keys: Vec<&str> = first.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["_3", "_1"]);
        assert_eq!(first.get("_3"), Some(&Value::String("c".into())));
        assert_eq!(first.get("_1"), Some(&Value::String("a".into())));
    }

    #[test]
    fn drop_removes_columns() {
        let pipeline = vec![
            FilterSpec::FromFields,
            FilterSpec::Drop {
                columns: vec!["_2".into()],
            },
        ];
        let result = run(&pipeline, b"a b c\n");
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        let keys: Vec<&str> = first.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["_1", "_3"]);
    }

    #[test]
    fn contains_matches_substring_case_sensitive() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::Contains {
                    column: "line".into(),
                    substring: "ell".into(),
                },
            },
        ];
        let result = run(&pipeline, b"hello\nworld\nyellow\nHELL\n");
        assert_eq!(lines_of(result), vec!["hello", "yellow"]);
    }

    #[test]
    fn compare_equality_on_strings() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::Compare {
                    column: "line".into(),
                    op: CompareOp::Eq,
                    value: Value::String("hello".into()),
                },
            },
        ];
        let result = run(&pipeline, b"hello\nworld\nhello\n");
        assert_eq!(lines_of(result), vec!["hello", "hello"]);
    }

    #[test]
    fn compare_gt_with_numeric_coercion_on_string_column() {
        // from-lines yields string-typed `line`; comparing with Int(10) coerces.
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::Compare {
                    column: "line".into(),
                    op: CompareOp::Gt,
                    value: Value::Int(10),
                },
            },
        ];
        let result = run(&pipeline, b"5\n10\n15\n20\n");
        assert_eq!(lines_of(result), vec!["15", "20"]);
    }

    #[test]
    fn compare_lex_on_strings() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Where {
                predicate: Predicate::Compare {
                    column: "line".into(),
                    op: CompareOp::Lt,
                    value: Value::String("m".into()),
                },
            },
        ];
        let result = run(&pipeline, b"alpha\nbravo\nzulu\n");
        assert_eq!(lines_of(result), vec!["alpha", "bravo"]);
    }

    #[test]
    fn from_csv_with_header() {
        let result = FilterSpec::FromCsv {
            delim: ',',
            has_header: true,
        }
        .apply(PipelineValue::Bytes(Bytes::from(
            "name,age\nalice,30\nbob,25\n".to_string(),
        )))
        .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 2);
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        let keys: Vec<&str> = first.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["name", "age"]);
        assert_eq!(first.get("name"), Some(&Value::String("alice".into())));
        assert_eq!(first.get("age"), Some(&Value::String("30".into())));
    }

    #[test]
    fn from_csv_without_header_uses_underscore_names() {
        let result = FilterSpec::FromCsv {
            delim: ',',
            has_header: false,
        }
        .apply(PipelineValue::Bytes(Bytes::from(
            "alice,30\nbob,25\n".to_string(),
        )))
        .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(first.get("_1"), Some(&Value::String("alice".into())));
        assert_eq!(first.get("_2"), Some(&Value::String("30".into())));
    }

    #[test]
    fn from_csv_is_flexible_about_field_count() {
        // Some rows have extra/missing fields. flexible(true) lets the parse
        // succeed; downstream filters can deal with the variance.
        let result = FilterSpec::FromCsv {
            delim: ',',
            has_header: true,
        }
        .apply(PipelineValue::Bytes(Bytes::from(
            "a,b\n1,2\n3\n4,5,6\n".to_string(),
        )))
        .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn from_csv_tab_delim() {
        let result = FilterSpec::FromCsv {
            delim: '\t',
            has_header: true,
        }
        .apply(PipelineValue::Bytes(Bytes::from(
            "x\ty\n1\t2\n".to_string(),
        )))
        .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(first.get("x"), Some(&Value::String("1".into())));
        assert_eq!(first.get("y"), Some(&Value::String("2".into())));
    }

    #[test]
    fn from_json_array_of_objects() {
        let result = FilterSpec::FromJson
            .apply(PipelineValue::Bytes(Bytes::from(
                r#"[{"a": 1}, {"a": 2, "b": "x"}]"#.to_string(),
            )))
            .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 2);
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(first.get("a"), Some(&Value::Int(1)));
        let second = match &items[1] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(second.get("a"), Some(&Value::Int(2)));
        assert_eq!(second.get("b"), Some(&Value::String("x".into())));
    }

    #[test]
    fn from_json_top_level_object_becomes_single_record() {
        let result = FilterSpec::FromJson
            .apply(PipelineValue::Bytes(Bytes::from(
                r#"{"name": "alice", "age": 30}"#.to_string(),
            )))
            .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn from_json_array_of_scalars() {
        let result = FilterSpec::FromJson
            .apply(PipelineValue::Bytes(Bytes::from(
                "[1, 2, 3]".to_string(),
            )))
            .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 3);
        // Scalars stay as-is in the list — they'll render but downstream filters may struggle.
        assert_eq!(items[0], Value::Int(1));
    }

    #[test]
    fn from_json_invalid_input_errors() {
        let err = FilterSpec::FromJson
            .apply(PipelineValue::Bytes(Bytes::from("not json".to_string())))
            .unwrap_err();
        assert!(matches!(err, FilterError::ParseError { format: "json", .. }));
    }

    #[test]
    fn from_regex_named_captures() {
        let result = FilterSpec::FromRegex {
            pattern: r"(?<key>\w+)=(?<val>\d+)".into(),
        }
        .apply(PipelineValue::Bytes(Bytes::from(
            "alpha=1\nbeta=22\nignored\ngamma=333\n".to_string(),
        )))
        .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 3);
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        let keys: Vec<&str> = first.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["key", "val"]);
        assert_eq!(first.get("key"), Some(&Value::String("alpha".into())));
        assert_eq!(first.get("val"), Some(&Value::String("1".into())));
    }

    #[test]
    fn from_regex_unnamed_groups_get_underscores() {
        let result = FilterSpec::FromRegex {
            pattern: r"(\w+) (\w+)".into(),
        }
        .apply(PipelineValue::Bytes(Bytes::from(
            "hello world\nfoo bar\n".to_string(),
        )))
        .unwrap();
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        let keys: Vec<&str> = first.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["_1", "_2"]);
    }

    #[test]
    fn count_returns_single_row_with_total() {
        let pipeline = vec![FilterSpec::FromLines, FilterSpec::Count];
        let result = run(&pipeline, b"a\nb\nc\n");
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 1);
        let r = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(r.get("count"), Some(&Value::Int(3)));
    }

    #[test]
    fn uniq_full_row_dedup() {
        let pipeline = vec![FilterSpec::FromLines, FilterSpec::Uniq { by: None }];
        let result = run(&pipeline, b"a\nb\na\nc\nb\n");
        assert_eq!(lines_of(result), vec!["a", "b", "c"]);
    }

    #[test]
    fn uniq_by_specific_column_keeps_first_occurrence() {
        let pipeline = vec![
            FilterSpec::FromFields,
            FilterSpec::Uniq {
                by: Some(vec!["_1".into()]),
            },
        ];
        let result = run(&pipeline, b"a 1\nb 2\na 3\nc 4\n");
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        assert_eq!(items.len(), 3);
        let firsts: Vec<String> = items
            .iter()
            .map(|v| match v {
                Value::Record(r) => match r.get("_1") {
                    Some(Value::String(s)) => s.clone(),
                    _ => panic!(),
                },
                _ => panic!(),
            })
            .collect();
        assert_eq!(firsts, vec!["a", "b", "c"]);
        // Confirm we kept the FIRST a (with _2="1"), not the later one (_2="3")
        match &items[0] {
            Value::Record(r) => {
                assert_eq!(r.get("_2"), Some(&Value::String("1".into())));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn sort_by_ascending() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::SortBy {
                keys: vec![SortKey {
                    column: "line".into(),
                    direction: SortDirection::Asc,
                }],
            },
        ];
        let result = run(&pipeline, b"banana\napple\ncherry\n");
        assert_eq!(lines_of(result), vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn sort_by_descending() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::SortBy {
                keys: vec![SortKey {
                    column: "line".into(),
                    direction: SortDirection::Desc,
                }],
            },
        ];
        let result = run(&pipeline, b"banana\napple\ncherry\n");
        assert_eq!(lines_of(result), vec!["cherry", "banana", "apple"]);
    }

    #[test]
    fn sort_by_numeric_via_string_coercion() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::SortBy {
                keys: vec![SortKey {
                    column: "line".into(),
                    direction: SortDirection::Asc,
                }],
            },
        ];
        // String-typed numbers — sort_by uses compare_values which coerces.
        let result = run(&pipeline, b"10\n2\n1\n100\n");
        assert_eq!(lines_of(result), vec!["1", "2", "10", "100"]);
    }

    #[test]
    fn select_inserts_null_for_missing_column() {
        let pipeline = vec![
            FilterSpec::FromLines,
            FilterSpec::Select {
                columns: vec!["line".into(), "missing".into()],
            },
        ];
        let result = run(&pipeline, b"a\n");
        let items = match result {
            PipelineValue::Structured(Value::List(items)) => items,
            _ => panic!(),
        };
        let first = match &items[0] {
            Value::Record(r) => r,
            _ => panic!(),
        };
        assert_eq!(first.get("missing"), Some(&Value::Null));
    }
}
