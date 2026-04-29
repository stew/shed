//! Global, cross-session command aliases.
//!
//! An alias is a `(name, argv, pipeline)` triple. Type the name at the
//! shed prompt and a new block materialises with the saved argv and
//! pipeline already filled in — useful for "I always want `ls -lat |
//! from-fields | sort-by` formatted as a table" scenarios.
//!
//! Aliases live outside any single notebook: the file at
//! `$XDG_CONFIG_HOME/shed/aliases.json` (fallback `~/.config/shed/`)
//! is loaded once on startup and rewritten on every change. Pure data
//! here in `shed-core`; the binary handles the storage path and the
//! TUI plumbing.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::filter::FilterSpec;

/// On-disk schema version. Bump on any non-backwards-compatible change.
pub const ALIASES_VERSION: u32 = 1;

/// One named, reusable command + pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alias {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline: Vec<FilterSpec>,
}

/// The whole aliases file: a versioned list. Order is insertion order;
/// callers that want sorted display sort at render time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasFile {
    pub version: u32,
    #[serde(default)]
    pub aliases: Vec<Alias>,
}

impl Default for AliasFile {
    fn default() -> Self {
        Self {
            version: ALIASES_VERSION,
            aliases: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AliasError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported aliases version: {0} (this build supports up to {max})", max = ALIASES_VERSION)]
    UnsupportedVersion(u32),
}

impl AliasFile {
    /// Read the file at `path`. Errors on a future version (so we don't
    /// silently corrupt newer-format aliases by re-saving in this
    /// build's format).
    pub fn load(path: &Path) -> Result<Self, AliasError> {
        let text = fs::read_to_string(path)?;
        let f: AliasFile = serde_json::from_str(&text)?;
        if f.version > ALIASES_VERSION {
            return Err(AliasError::UnsupportedVersion(f.version));
        }
        Ok(f)
    }

    /// Write pretty-printed JSON to `path`, creating parent dirs as
    /// needed. Overwrites any existing file.
    pub fn save(&self, path: &Path) -> Result<(), AliasError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn lookup(&self, name: &str) -> Option<&Alias> {
        self.aliases.iter().find(|a| a.name == name)
    }

    /// Insert or replace by name. Returns `true` if an existing entry
    /// was overwritten, `false` if a new one was appended.
    pub fn upsert(&mut self, alias: Alias) -> bool {
        if let Some(slot) = self.aliases.iter_mut().find(|a| a.name == alias.name) {
            *slot = alias;
            true
        } else {
            self.aliases.push(alias);
            false
        }
    }

    /// Remove the entry with `name`. Returns `true` if anything was
    /// removed.
    pub fn delete(&mut self, name: &str) -> bool {
        let len_before = self.aliases.len();
        self.aliases.retain(|a| a.name != name);
        self.aliases.len() < len_before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::FilterSpec;

    #[test]
    fn upsert_inserts_then_replaces_by_name() {
        let mut f = AliasFile::default();
        let replaced = f.upsert(Alias {
            name: "list".into(),
            argv: vec!["ls".into(), "-l".into()],
            pipeline: Vec::new(),
        });
        assert!(!replaced);
        assert_eq!(f.aliases.len(), 1);

        let replaced = f.upsert(Alias {
            name: "list".into(),
            argv: vec!["ls".into(), "-lat".into()],
            pipeline: vec![FilterSpec::FromFields],
        });
        assert!(replaced);
        assert_eq!(f.aliases.len(), 1);
        assert_eq!(f.lookup("list").unwrap().argv, vec!["ls", "-lat"]);
    }

    #[test]
    fn delete_returns_true_on_hit_false_on_miss() {
        let mut f = AliasFile::default();
        f.upsert(Alias {
            name: "list".into(),
            argv: vec!["ls".into()],
            pipeline: Vec::new(),
        });
        assert!(f.delete("list"));
        assert!(!f.delete("list"));
        assert!(f.aliases.is_empty());
    }

    #[test]
    fn save_load_uses_filesystem() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shed-aliases-test-{}.json", std::process::id()));

        let mut f = AliasFile::default();
        f.upsert(Alias {
            name: "list".into(),
            argv: vec!["ls".into(), "-lat".into()],
            pipeline: vec![FilterSpec::FromFields],
        });
        f.save(&path).unwrap();

        let loaded = AliasFile::load(&path).unwrap();
        assert_eq!(loaded.aliases.len(), 1);
        assert_eq!(loaded.lookup("list").unwrap().argv, vec!["ls", "-lat"]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_future_version() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shed-aliases-future-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"version": 99, "aliases": []}"#).unwrap();
        let err = AliasFile::load(&path).unwrap_err();
        assert!(matches!(err, AliasError::UnsupportedVersion(99)));
        let _ = std::fs::remove_file(&path);
    }
}
