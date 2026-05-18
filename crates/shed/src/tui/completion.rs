//! Tab-completion machinery for the prompt and the in-place command
//! editor.
//!
//! The flow:
//! 1. [`cycle_completion`] is called on Tab / Shift-Tab.
//! 2. On the first press it captures `(text_before_cursor, text_after_cursor)`,
//!    splits the text at the last whitespace to find the token under the
//!    cursor, classifies the token via [`classify_completion`], and asks
//!    the right candidate source ([`env_completions`], [`pinned_completions`],
//!    [`id_completions`], [`slash_completions`], [`argv0_completions`],
//!    or [`path_or_carapace_completions`]).
//! 3. The match list lives in [`CompletionState`] on `App`; subsequent
//!    Tabs cycle through it by mutating `idx`.
//! 4. Any non-Tab keystroke in a completion context clears
//!    `app.completion`, so the next Tab rebuilds from scratch.

use std::collections::HashSet;
use std::path::PathBuf;

use shed_core::{AliasFile, Session};

use super::{App, Focus};

const COMPLETION_BUILTINS: &[&str] = &["cd", "exit", "quit", "export", "unset"];
const COMPLETION_SLASH: &[&str] = &["/aliases"];

#[derive(Debug, Clone)]
pub(super) struct CompletionState {
    /// The unchanged prefix of the input (before the token being
    /// completed). Each cycle re-renders the input as
    /// `base_text + matches[idx] + suffix`.
    pub(super) base_text: String,
    /// The unchanged suffix of the input (everything after the cursor
    /// at the moment Tab was first pressed).
    pub(super) suffix: String,
    pub(super) matches: Vec<String>,
    pub(super) idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionContext {
    EnvVar,
    Pinned,
    ShedId,
    Slash,
    Argv0,
    Path,
}

/// Split `text` into (everything before the final token, the final
/// token). The final token starts after the last whitespace char.
fn split_last_token(text: &str) -> (&str, &str) {
    match text.rfind(char::is_whitespace) {
        Some(idx) => {
            let after = idx + text[idx..].chars().next().unwrap().len_utf8();
            (&text[..after], &text[after..])
        }
        None => ("", text),
    }
}

fn classify_completion(focus: Focus, base: &str, token: &str) -> CompletionContext {
    if token.starts_with('$') {
        return CompletionContext::EnvVar;
    }
    if token.starts_with('@') {
        return CompletionContext::Pinned;
    }
    if token.starts_with('%') {
        return CompletionContext::ShedId;
    }
    let argv0 = base.trim().is_empty();
    if argv0 {
        if focus == Focus::Prompt && token.starts_with('/') {
            return CompletionContext::Slash;
        }
        if token.starts_with('/')
            || token.starts_with("./")
            || token.starts_with("../")
            || token.starts_with("~/")
        {
            return CompletionContext::Path;
        }
        return CompletionContext::Argv0;
    }
    CompletionContext::Path
}

fn env_completions(token: &str) -> Vec<String> {
    let prefix = &token[1..]; // strip $
    let mut names: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with(prefix))
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().map(|n| format!("${n}")).collect()
}

fn id_completions(session: &Session, token: &str) -> Vec<String> {
    let prefix = &token[1..]; // strip %
    let mut matches: Vec<String> = session
        .sheds()
        .map(|b| format!("%{}", b.id.0))
        .filter(|s| s.starts_with(&format!("%{prefix}")))
        .collect();
    matches.sort();
    matches.dedup();
    matches
}

fn pinned_completions(session: &Session, token: &str) -> Vec<String> {
    let prefix = &token[1..]; // strip @
    let mut names: Vec<String> = session
        .sheds()
        .filter_map(|b| b.name.clone())
        .filter(|n| n.starts_with(prefix))
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().map(|n| format!("@{n}")).collect()
}

fn slash_completions(token: &str) -> Vec<String> {
    COMPLETION_SLASH
        .iter()
        .filter(|cmd| cmd.starts_with(token))
        .map(|s| (*s).to_string())
        .collect()
}

fn argv0_completions(aliases: &AliasFile, token: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut all: BTreeSet<String> = BTreeSet::new();
    for b in COMPLETION_BUILTINS {
        if b.starts_with(token) {
            all.insert((*b).to_string());
        }
    }
    for alias in &aliases.aliases {
        if alias.name.starts_with(token) {
            all.insert(alias.name.clone());
        }
    }
    for cmd in path_executables(token) {
        all.insert(cmd);
    }
    all.into_iter().collect()
}

/// Shell out to `carapace <argv0> export <argv0> <tokens...>` to get
/// rich completions for the current argument position. Returns `None`
/// when carapace isn't installed, when the spawn fails, or when its
/// output isn't valid export JSON — in all of those cases the caller
/// falls back to filesystem path completion.
///
/// Carapace's `export` format is a JSON object whose top-level
/// `values` array holds `{value, display, description, style, tag}`
/// records; we surface just `value` strings here. The carapace docs
/// claim sub-10ms response on warm cache, which is well within
/// interactive Tab-press latency.
fn run_carapace_export(argv0: &str, tokens: &[String]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("carapace")
        .arg(argv0)
        .arg("export")
        .arg(argv0)
        .args(tokens)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Parse a carapace `export`-format JSON document into a sorted, deduped
/// list of completion values that start with `token`. Carapace's own
/// completers usually return only matching values for the partial, but
/// we filter defensively so we behave the same regardless.
fn carapace_completions_from_export(json: &[u8], token: &str) -> Vec<String> {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(values) = root.get("values").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = values
        .iter()
        .filter_map(|item| item.get("value").and_then(|v| v.as_str()).map(String::from))
        .filter(|v| v.starts_with(token))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn carapace_completions(argv0: &str, tokens: &[String], token: &str) -> Vec<String> {
    let Some(json) = run_carapace_export(argv0, tokens) else {
        return Vec::new();
    };
    carapace_completions_from_export(&json, token)
}

fn path_executables(prefix: &str) -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            if !is_executable_entry(&entry) {
                continue;
            }
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(unix)]
fn is_executable_entry(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::PermissionsExt;
    entry
        .metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_entry(_entry: &std::fs::DirEntry) -> bool {
    true
}

fn path_completions(token: &str) -> Vec<String> {
    // dir_str is what we keep in the displayed match (so `~/` stays
    // `~/`), dir_lookup is the actual filesystem path.
    let (dir_str, prefix) = match token.rfind('/') {
        Some(idx) => (&token[..=idx], &token[idx + 1..]),
        None => ("", token),
    };
    let dir_lookup: PathBuf = if dir_str.is_empty() {
        PathBuf::from(".")
    } else if let Some(rest) = dir_str.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => {
                let mut p = PathBuf::from(home);
                p.push(rest);
                p
            }
            None => PathBuf::from(dir_str),
        }
    } else {
        PathBuf::from(dir_str)
    };

    let Ok(entries) = std::fs::read_dir(&dir_lookup) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let mut rows: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(prefix) {
                return None;
            }
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some((name, is_dir))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .map(|(name, is_dir)| {
            let suffix = if is_dir { "/" } else { "" };
            format!("{dir_str}{name}{suffix}")
        })
        .collect()
}

/// Compute the (base_text, matches) pair for completing `text` at the
/// final token.
fn compute_completions(
    session: &Session,
    aliases: &AliasFile,
    focus: Focus,
    text: &str,
) -> (String, Vec<String>) {
    let (base, token) = split_last_token(text);
    let ctx = classify_completion(focus, base, token);
    let matches = match ctx {
        CompletionContext::EnvVar => env_completions(token),
        CompletionContext::Pinned => pinned_completions(session, token),
        CompletionContext::ShedId => id_completions(session, token),
        CompletionContext::Slash => slash_completions(token),
        CompletionContext::Argv0 => argv0_completions(aliases, token),
        CompletionContext::Path => path_or_carapace_completions(base, token),
    };
    (base.to_string(), matches)
}

/// For an argv1+ position, ask carapace first (if installed) for
/// command-aware completions like git branch names, kubectl flags,
/// docker image tags, etc. Fall back to filesystem path completion if
/// carapace isn't available or returns no matches.
fn path_or_carapace_completions(base: &str, token: &str) -> Vec<String> {
    let prior: Vec<String> = base.split_whitespace().map(String::from).collect();
    if let Some(argv0) = prior.first() {
        let mut spans = prior.clone();
        spans.push(token.to_string());
        let from_carapace = carapace_completions(argv0, &spans, token);
        if !from_carapace.is_empty() {
            return from_carapace;
        }
    }
    path_completions(token)
}

fn current_input_state(app: &App) -> Option<(String, usize)> {
    match app.focus {
        Focus::Prompt => Some((app.prompt.clone(), app.prompt_cursor)),
        Focus::EditShed if app.cmd_edit_input_mode => {
            Some((app.cmd_edit_input.clone(), app.cmd_edit_cursor))
        }
        _ => None,
    }
}

fn set_current_input(app: &mut App, text: String, cursor: usize) {
    match app.focus {
        Focus::Prompt => {
            app.prompt = text;
            app.prompt_cursor = cursor;
        }
        Focus::EditShed if app.cmd_edit_input_mode => {
            app.cmd_edit_input = text;
            app.cmd_edit_cursor = cursor;
        }
        _ => {}
    }
}

/// Handle a Tab (dir = +1) or Shift-Tab (dir = -1) press in a
/// completion context. On the first press, builds a fresh match list
/// and applies the first match. Subsequent presses cycle through
/// `app.completion.matches`.
pub(super) fn cycle_completion(app: &mut App, dir: i32) {
    if app.completion.is_none() {
        let Some((text, cursor)) = current_input_state(app) else {
            return;
        };
        let prefix = &text[..cursor];
        let suffix = text[cursor..].to_string();
        let focus = app.focus;
        let (base, matches) = compute_completions(&app.session, &app.aliases, focus, prefix);
        if matches.is_empty() {
            app.flash = Some("no completions".into());
            return;
        }
        let new_cursor = base.len() + matches[0].len();
        set_current_input(app, format!("{base}{}{suffix}", matches[0]), new_cursor);
        app.completion = Some(CompletionState {
            base_text: base,
            suffix,
            matches,
            idx: 0,
        });
        return;
    }
    let state = app.completion.as_mut().unwrap();
    let n = state.matches.len() as isize;
    state.idx = ((state.idx as isize + dir as isize).rem_euclid(n)) as usize;
    let new_cursor = state.base_text.len() + state.matches[state.idx].len();
    let new_text = format!(
        "{}{}{}",
        state.base_text, state.matches[state.idx], state.suffix
    );
    set_current_input(app, new_text, new_cursor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use shed_core::Alias;

    #[test]
    fn split_last_token_handles_empty_and_whitespace() {
        assert_eq!(split_last_token(""), ("", ""));
        assert_eq!(split_last_token("hi"), ("", "hi"));
        assert_eq!(split_last_token("ls "), ("ls ", ""));
        assert_eq!(split_last_token("ls fo"), ("ls ", "fo"));
        assert_eq!(split_last_token("ls foo bar"), ("ls foo ", "bar"));
    }

    #[test]
    fn classify_completion_picks_each_context() {
        // Env var.
        assert_eq!(
            classify_completion(Focus::Prompt, "echo ", "$HO"),
            CompletionContext::EnvVar,
        );
        // Pinned name.
        assert_eq!(
            classify_completion(Focus::Prompt, "", "@lo"),
            CompletionContext::Pinned,
        );
        // Slash command at start of prompt.
        assert_eq!(
            classify_completion(Focus::Prompt, "", "/al"),
            CompletionContext::Slash,
        );
        // Slash command NOT first token → path.
        assert_eq!(
            classify_completion(Focus::Prompt, "echo ", "/al"),
            CompletionContext::Path,
        );
        // Argv0 (first token, not a path or special prefix).
        assert_eq!(
            classify_completion(Focus::Prompt, "", "ls"),
            CompletionContext::Argv0,
        );
        // Argv1+ position.
        assert_eq!(
            classify_completion(Focus::Prompt, "ls ", "/etc"),
            CompletionContext::Path,
        );
    }

    #[test]
    fn classify_completion_recognises_percent_prefix() {
        assert_eq!(
            classify_completion(Focus::Prompt, "echo ", "%1"),
            CompletionContext::ShedId,
        );
        assert_eq!(
            classify_completion(Focus::Prompt, "", "%5"),
            CompletionContext::ShedId,
        );
    }

    #[test]
    fn slash_completions_returns_known_commands() {
        let got = slash_completions("/al");
        assert_eq!(got, vec!["/aliases".to_string()]);
        assert!(slash_completions("/nope").is_empty());
    }

    #[test]
    fn argv0_completions_includes_builtins_and_aliases() {
        let aliases = AliasFile {
            version: 1,
            aliases: vec![Alias {
                name: "list".into(),
                argv: vec!["ls".into()],
                pipeline: Vec::new(),
            }],
        };
        let got = argv0_completions(&aliases, "li");
        assert!(got.contains(&"list".to_string()));
        let got = argv0_completions(&aliases, "ex");
        assert!(got.contains(&"exit".to_string()) || got.contains(&"export".to_string()));
    }

    #[test]
    fn pinned_completions_filters_by_prefix() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["a".into()]);
        let b = s.add_shed(vec!["b".into()]);
        s.pin(a, "logs".into());
        s.pin(b, "long".into());
        let got = pinned_completions(&s, "@lo");
        assert_eq!(got, vec!["@logs".to_string(), "@long".to_string()]);
        let got = pinned_completions(&s, "@logs");
        assert_eq!(got, vec!["@logs".to_string()]);
        let got = pinned_completions(&s, "@x");
        assert!(got.is_empty());
    }

    #[test]
    fn id_completions_returns_all_shed_ids_matching_prefix() {
        let mut s = Session::new();
        for _ in 0..15 {
            let _ = s.add_shed(vec!["x".into()]);
        }
        // `%1` matches "%1", "%10", "%11", ... "%15" (string prefix).
        let got = id_completions(&s, "%1");
        for expected in &["%1", "%10", "%11", "%12", "%13", "%14", "%15"] {
            assert!(got.contains(&expected.to_string()), "missing {expected}");
        }
        // `%9` matches only "%9".
        let got = id_completions(&s, "%9");
        assert_eq!(got, vec!["%9".to_string()]);
    }

    #[test]
    fn env_completions_pulls_from_environment() {
        // SAFETY: setting an env var with a fixed test-only name; cleanup
        // after to avoid leaking into other tests.
        // SAFETY: required for std::env::set_var / remove_var in 2024 edition.
        unsafe {
            std::env::set_var("SHED_TEST_COMPLETION_XYZ", "1");
        }
        let got = env_completions("$SHED_TEST_COMPLETION_X");
        assert!(got.contains(&"$SHED_TEST_COMPLETION_XYZ".to_string()));
        unsafe {
            std::env::remove_var("SHED_TEST_COMPLETION_XYZ");
        }
    }

    #[test]
    fn carapace_export_parser_extracts_values_matching_prefix() {
        let json = br#"{
            "version": "unknown",
            "messages": [],
            "values": [
                {"value": "main", "display": "main", "description": "branch"},
                {"value": "master", "display": "master"},
                {"value": "develop", "display": "develop"}
            ]
        }"#;
        let got = carapace_completions_from_export(json, "ma");
        assert_eq!(got, vec!["main".to_string(), "master".to_string()]);
    }

    #[test]
    fn carapace_export_parser_empty_token_returns_all_values_sorted() {
        let json = br#"{
            "values": [
                {"value": "zeta"},
                {"value": "alpha"},
                {"value": "alpha"}
            ]
        }"#;
        let got = carapace_completions_from_export(json, "");
        assert_eq!(got, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn carapace_export_parser_missing_values_returns_empty() {
        let got = carapace_completions_from_export(b"{\"version\":\"x\"}", "");
        assert!(got.is_empty());
    }

    #[test]
    fn carapace_export_parser_handles_invalid_json() {
        let got = carapace_completions_from_export(b"not json at all", "");
        assert!(got.is_empty());
    }

    #[test]
    fn carapace_export_parser_ignores_records_without_value_field() {
        let json = br#"{"values": [{"display": "x"}, {"value": "real"}]}"#;
        let got = carapace_completions_from_export(json, "");
        assert_eq!(got, vec!["real".to_string()]);
    }
}
