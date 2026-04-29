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
    Where { predicate: Predicate },
    Select { columns: Vec<String> },
    Drop { columns: Vec<String> },
    Take { n: usize },
    Skip { n: usize },
}

#[derive(Debug, Clone)]
pub enum Predicate {
    Matches { column: String, pattern: String },
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
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
            FilterSpec::Where { predicate } => apply_where(input, predicate),
            FilterSpec::Select { columns } => apply_select(input, columns),
            FilterSpec::Drop { columns } => apply_drop(input, columns),
            FilterSpec::Take { n } => apply_take(input, *n),
            FilterSpec::Skip { n } => apply_skip(input, *n),
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
            Predicate::And(a, b) => Ok(a.evaluate(value)? && b.evaluate(value)?),
            Predicate::Or(a, b) => Ok(a.evaluate(value)? || b.evaluate(value)?),
            Predicate::Not(p) => Ok(!p.evaluate(value)?),
        }
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
