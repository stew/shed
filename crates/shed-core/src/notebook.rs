//! Notebook persistence: save and load a session's sheds as JSON.
//!
//! A notebook is the durable form of a [`Session`]: an ordered list of
//! commands plus the retroactive pipeline the user built around each one.
//! Captures, exit codes, and timestamps are *not* persisted — re-opening
//! a notebook gives you idle sheds ready to be re-run, not a frozen view
//! of past output.
//!
//! Round-trip is asymmetric on purpose:
//!
//! - [`Notebook::from_session`] snapshots argv + name + pipeline for every
//!   shed currently in the session.
//! - [`Notebook::apply_to_session`] inserts each entry as a fresh shed in
//!   [`ShedState::Idle`]. The user runs them via the TUI's run-in-place
//!   action.
//!
//! On-disk format is pretty-printed JSON with a `version` field so the
//! shape can evolve. There is exactly one variant on [`NotebookEntry`]
//! today (`Command`); the enum exists so notes can be added in a future
//! pass without breaking existing files.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use indexmap::IndexMap;

use crate::filter::FilterSpec;
use crate::session::Session;
use crate::shed::{OutputSpec, ShedState};

/// On-disk schema version. Bumped whenever the file format changes in a
/// non-backwards-compatible way.
pub const NOTEBOOK_VERSION: u32 = 1;

/// A serializable snapshot of a session's command structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notebook {
    pub version: u32,
    pub entries: Vec<NotebookEntry>,
}

/// A single entry in a notebook. Today this is always `Command`; future
/// versions may add a standalone `Note` variant. For now, free-form text
/// hangs off a `Command` entry as `pre_text`/`post_text`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotebookEntry {
    Command {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pipeline: Vec<FilterSpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_text: Option<String>,
        /// Named outputs this shed declares — see [`OutputSpec`].
        /// Persisted only when the shed actually declares outputs;
        /// older notebooks load with an empty map.
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        outputs: IndexMap<String, OutputSpec>,
    },
}

/// Errors that can occur saving or loading a notebook.
#[derive(Debug, thiserror::Error)]
pub enum NotebookError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported notebook version: {0} (this build supports up to {max})", max = NOTEBOOK_VERSION)]
    UnsupportedVersion(u32),
}

impl Notebook {
    /// Snapshot every shed in the session. Shed order in the notebook
    /// matches shed id order (oldest first).
    pub fn from_session(session: &Session) -> Self {
        let entries = session
            .sheds()
            .map(|b| NotebookEntry::Command {
                argv: b.argv.clone(),
                name: b.name.clone(),
                pipeline: b.pipeline.clone(),
                pre_text: b.pre_text.clone(),
                post_text: b.post_text.clone(),
                outputs: b.outputs.clone(),
            })
            .collect();
        Self {
            version: NOTEBOOK_VERSION,
            entries,
        }
    }

    /// Append every entry as a fresh shed in [`ShedState::Idle`]. The
    /// session is not cleared first; callers wanting a clean load should
    /// pass a fresh [`Session`].
    pub fn apply_to_session(&self, session: &mut Session) {
        for entry in &self.entries {
            let NotebookEntry::Command {
                argv,
                name,
                pipeline,
                pre_text,
                post_text,
                outputs,
            } = entry;
            let id = session.add_shed(argv.clone());
            session.set_state(id, ShedState::Idle);
            if let Some(shed) = session.shed_mut(id) {
                shed.pipeline = pipeline.clone();
                shed.pre_text = pre_text.clone();
                shed.post_text = post_text.clone();
                shed.outputs = outputs.clone();
            }
            if let Some(n) = name {
                session.pin(id, n.clone());
            }
        }
    }

    /// Write pretty-printed JSON to `path`. Overwrites any existing file.
    pub fn save(&self, path: &Path) -> Result<(), NotebookError> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    /// Read and parse a notebook file. Errors if the version is newer than
    /// this build understands.
    pub fn load(path: &Path) -> Result<Self, NotebookError> {
        let text = fs::read_to_string(path)?;
        let nb: Notebook = serde_json::from_str(&text)?;
        if nb.version > NOTEBOOK_VERSION {
            return Err(NotebookError::UnsupportedVersion(nb.version));
        }
        Ok(nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{CompareOp, Predicate, SortDirection, SortKey};
    use crate::value::Value;

    #[test]
    fn round_trips_argv_pipeline_and_name() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["ls".into(), "-l".into()]);
        s.shed_mut(a).unwrap().pipeline = vec![
            FilterSpec::FromFields,
            FilterSpec::Where {
                predicate: Predicate::Compare {
                    column: "_5".into(),
                    op: CompareOp::Gt,
                    value: Value::Int(100),
                },
            },
            FilterSpec::SortBy {
                keys: vec![SortKey {
                    column: "_5".into(),
                    direction: SortDirection::Desc,
                }],
            },
        ];
        s.pin(a, "biggies".into());
        let _ = s.add_shed(vec!["uptime".into()]);

        let nb = Notebook::from_session(&s);
        let json = serde_json::to_string(&nb).unwrap();
        let nb2: Notebook = serde_json::from_str(&json).unwrap();

        let mut s2 = Session::new();
        nb2.apply_to_session(&mut s2);

        let sheds: Vec<_> = s2.sheds().collect();
        assert_eq!(sheds.len(), 2);
        assert_eq!(sheds[0].argv, vec!["ls", "-l"]);
        assert_eq!(sheds[0].name.as_deref(), Some("biggies"));
        assert_eq!(sheds[0].pipeline.len(), 3);
        assert!(matches!(sheds[0].state, ShedState::Idle));
        assert_eq!(sheds[1].argv, vec!["uptime"]);
        assert!(sheds[1].name.is_none());
        assert!(sheds[1].pipeline.is_empty());
    }

    #[test]
    fn round_trips_pre_and_post_text() {
        let mut s = Session::new();
        let id = s.add_shed(vec!["ls".into()]);
        if let Some(b) = s.shed_mut(id) {
            b.pre_text = Some("## Section\nthis lists things".into());
            b.post_text = Some("output looked normal".into());
        }
        let nb = Notebook::from_session(&s);
        let json = serde_json::to_string(&nb).unwrap();
        assert!(json.contains("Section"));

        let nb2: Notebook = serde_json::from_str(&json).unwrap();
        let mut s2 = Session::new();
        nb2.apply_to_session(&mut s2);

        let sheds: Vec<_> = s2.sheds().collect();
        assert_eq!(
            sheds[0].pre_text.as_deref(),
            Some("## Section\nthis lists things")
        );
        assert_eq!(sheds[0].post_text.as_deref(), Some("output looked normal"));
    }

    #[test]
    fn save_load_uses_filesystem() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shed-test-notebook-{}.json", std::process::id()));
        let mut s = Session::new();
        s.add_shed(vec!["echo".into(), "hi".into()]);
        let nb = Notebook::from_session(&s);
        nb.save(&path).unwrap();

        let loaded = Notebook::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_future_version() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shed-test-future-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"version": 999, "entries": []}"#).unwrap();
        let err = Notebook::load(&path).unwrap_err();
        assert!(matches!(err, NotebookError::UnsupportedVersion(999)));
        let _ = std::fs::remove_file(&path);
    }
}
