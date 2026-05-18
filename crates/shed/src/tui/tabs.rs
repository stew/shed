//! Multi-tab support: per-tab state stashing, tab-bar rendering, and
//! the keybindings + drive loop that hold the system together.
//!
//! Each tab owns its own [`Session`], `prompt`, `focus`, undo/redo
//! stacks, `running` map, notebook path, and pending work. The active
//! tab's data lives directly on App fields (`app.session`,
//! `app.prompt`, …) so the rest of the codebase doesn't need to know
//! about tabs. Other tabs hold their state inside [`TabSlot::stashed`];
//! [`App::switch_to_tab`] stashes the previously-active tab and pulls
//! the destination's stash onto App.
//!
//! Background streaming: [`drive_all_tabs`] runs the equivalent of one
//! event-loop tick (drain streams + reap completed children) for every
//! tab each frame, including inactive ones, so a long-running command
//! in a background tab keeps producing output while the user works in
//! another tab.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use shed_core::{Session, ShedId};

use super::input::handle_text_input;
use super::{
    App, ClickAction, ClickRegion, Focus, HandoverRequest, RerunRequest, RunningCommand,
    collapse_home_in_path, drain_streams, pinned_entries_json, reap_completed,
};

/// The persistent per-tab state extracted from App. Active tab's data
/// lives directly on App fields (`app.session`, `app.prompt`, etc.) for
/// minimal disruption to existing code; stashed tabs hold a snapshot of
/// these fields. `switch_to_tab` swaps the active tab's data into the
/// stash and pulls the target tab's stash into App.
pub(super) struct StashedTab {
    pub(super) session: Session,
    pub(super) running: HashMap<ShedId, RunningCommand>,
    pub(super) prompt: String,
    pub(super) prompt_cursor: usize,
    pub(super) focus: Focus,
    pub(super) notebook_path: Option<PathBuf>,
    pub(super) dirty: bool,
    pub(super) undo_stack: Vec<Session>,
    pub(super) redo_stack: Vec<Session>,
    pub(super) pending_run_chain: VecDeque<ShedId>,
    pub(super) chain_in_flight: Option<ShedId>,
    pub(super) pending_handover: Option<HandoverRequest>,
    pub(super) pending_rerun: Option<RerunRequest>,
    /// JSON-serialised snapshot of the *pinned* sheds at the last save
    /// or load. Compared against the current pinned-shed JSON to decide
    /// whether the exit prompt should fire — unpinned-shed edits are
    /// treated as scratch and don't trigger the "save before quitting"
    /// dialog.
    pub(super) saved_pinned_json: String,
}

/// Per-tab metadata that lives on App regardless of which tab is
/// active. The active tab's `stashed` is `None` (its data lives on App
/// fields); inactive tabs hold their data in `stashed`.
pub(super) struct TabSlot {
    pub(super) title: Option<String>,
    /// Monotonically increasing counter, bumped on any session mutation
    /// (output streams in, command completes, structural edits). The
    /// tab is rendered as "has unread activity" when
    /// `activity_seq > last_viewed_seq`.
    pub(super) activity_seq: u64,
    pub(super) last_viewed_seq: u64,
    pub(super) stashed: Option<StashedTab>,
}

impl TabSlot {
    pub(super) fn new_active(title: Option<String>) -> Self {
        Self {
            title,
            activity_seq: 0,
            last_viewed_seq: 0,
            stashed: None,
        }
    }

    pub(super) fn display_title(&self, index: usize) -> String {
        if let Some(t) = &self.title {
            if !t.is_empty() {
                return t.clone();
            }
        }
        // Fall back to the notebook basename if one is bound.
        if let Some(stashed) = &self.stashed {
            if let Some(p) = &stashed.notebook_path {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    return name.to_string();
                }
            }
        }
        format!("tab {}", index + 1)
    }

    pub(super) fn has_unread(&self) -> bool {
        self.activity_seq > self.last_viewed_seq
    }
}

impl App {
    pub(super) fn stash_active(&mut self) -> StashedTab {
        StashedTab {
            session: std::mem::take(&mut self.session),
            running: std::mem::take(&mut self.running),
            prompt: std::mem::take(&mut self.prompt),
            prompt_cursor: std::mem::take(&mut self.prompt_cursor),
            focus: std::mem::replace(&mut self.focus, Focus::Prompt),
            notebook_path: self.notebook_path.take(),
            dirty: std::mem::take(&mut self.dirty),
            undo_stack: std::mem::take(&mut self.undo_stack),
            redo_stack: std::mem::take(&mut self.redo_stack),
            pending_run_chain: std::mem::take(&mut self.pending_run_chain),
            chain_in_flight: self.chain_in_flight.take(),
            pending_handover: self.pending_handover.take(),
            pending_rerun: self.pending_rerun.take(),
            saved_pinned_json: std::mem::take(&mut self.saved_pinned_json),
        }
    }

    /// Restore a [`StashedTab`] back onto the App's per-tab fields.
    pub(super) fn restore_stashed(&mut self, t: StashedTab) {
        self.session = t.session;
        self.running = t.running;
        self.prompt = t.prompt;
        self.prompt_cursor = t.prompt_cursor;
        self.focus = t.focus;
        self.notebook_path = t.notebook_path;
        self.dirty = t.dirty;
        self.undo_stack = t.undo_stack;
        self.redo_stack = t.redo_stack;
        self.pending_run_chain = t.pending_run_chain;
        self.chain_in_flight = t.chain_in_flight;
        self.pending_handover = t.pending_handover;
        self.pending_rerun = t.pending_rerun;
        self.saved_pinned_json = t.saved_pinned_json;
    }

    /// Build a fresh [`StashedTab`] for a brand-new tab — empty session,
    /// prompt focus, no pending work.
    fn fresh_stashed_tab() -> StashedTab {
        let empty = Session::new();
        let baseline = pinned_entries_json(&empty);
        StashedTab {
            session: empty,
            running: HashMap::new(),
            prompt: String::new(),
            prompt_cursor: 0,
            focus: Focus::Prompt,
            notebook_path: None,
            dirty: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending_run_chain: VecDeque::new(),
            chain_in_flight: None,
            pending_handover: None,
            pending_rerun: None,
            saved_pinned_json: baseline,
        }
    }

    /// Reset all modal/input state so a tab switch (or new-tab creation)
    /// lands cleanly. Doesn't touch per-tab persistent state — that's
    /// handled by [`App::stash_active`] / [`App::restore_stashed`].
    pub(super) fn close_transient_state(&mut self) {
        self.completion = None;
        self.write_input_mode = false;
        self.write_input.clear();
        self.write_cursor = 0;
        self.pin_input_mode = false;
        self.pin_input.clear();
        self.pin_cursor = 0;
        self.rerun_input_mode = false;
        self.rerun_input.clear();
        self.rerun_cursor = 0;
        self.rerun_source_id = None;
        self.command_focused = false;
        self.cmd_edit_input_mode = false;
        self.cmd_edit_input.clear();
        self.cmd_edit_cursor = 0;
        self.env_edit = None;
        self.note_edit = None;
        self.palette_state = None;
        self.palette_prev_focus = None;
        self.filter_edit = None;
        self.pipeline_cursor = 0;
        self.expand_scroll = 0;
        self.search_query.clear();
        self.search_input.clear();
        self.search_cursor = 0;
        self.search_input_mode = false;
        self.search_anchor_scroll = 0;
        self.search_input_backward = false;
        self.alias_name_input_mode = false;
        self.alias_name_input.clear();
        self.alias_name_cursor = 0;
        self.alias_overwrite = None;
        self.alias_manage = None;
        self.save_input_mode = false;
        self.save_input.clear();
        self.save_cursor = 0;
        self.open_input_mode = false;
        self.open_input.clear();
        self.open_cursor = 0;
        self.exit_prompt = None;
        self.context_menu = None;
        self.rename_tab_input_mode = false;
        self.rename_tab_input.clear();
        self.rename_tab_cursor = 0;
    }

    /// Switch the active tab to `idx`. No-op if `idx` is already active
    /// or out of bounds. Closes transient/modal state on switch; the
    /// destination tab's persistent state is restored from its stash.
    pub(super) fn switch_to_tab(&mut self, idx: usize) {
        if idx == self.active_tab || idx >= self.tabs.len() {
            return;
        }
        self.close_transient_state();
        let old = self.active_tab;
        let stashed = self.stash_active();
        self.tabs[old].stashed = Some(stashed);
        let taken = self.tabs[idx]
            .stashed
            .take()
            .expect("inactive tab missing its stash");
        self.restore_stashed(taken);
        self.active_tab = idx;
        // Mark as viewed: clear unread badge.
        let slot = &mut self.tabs[idx];
        slot.last_viewed_seq = slot.activity_seq;
    }

    /// Create a new tab and switch to it. Returns the new tab's index.
    pub(super) fn new_tab(&mut self) -> usize {
        self.close_transient_state();
        let old = self.active_tab;
        let stashed = self.stash_active();
        self.tabs[old].stashed = Some(stashed);
        // Push a placeholder slot for the new tab, then restore a fresh
        // StashedTab onto App fields.
        self.tabs.push(TabSlot::new_active(None));
        let new_idx = self.tabs.len() - 1;
        self.restore_stashed(Self::fresh_stashed_tab());
        self.active_tab = new_idx;
        new_idx
    }

    /// Close the active tab. No-op when there is only one tab (otherwise
    /// shed would have nothing to render). Children still running in the
    /// tab are killed first.
    pub(super) fn close_active_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.flash = Some("can't close the last tab".into());
            return;
        }
        // Kill children in the active (about-to-be-closed) tab.
        for (_, mut cmd) in self.running.drain() {
            let _ = cmd.killer.kill();
            cmd.handle.abort();
        }
        let old = self.active_tab;
        // Pick which tab to switch to (prefer the left neighbour so the
        // index after removal still points at a real tab).
        let target = if old == 0 { 1 } else { old - 1 };
        // Move the target's stash onto App.
        self.close_transient_state();
        // Drop App's per-tab data (we just killed running; let the rest go).
        let _ = self.stash_active();
        let taken = self.tabs[target]
            .stashed
            .take()
            .expect("inactive tab missing its stash");
        self.restore_stashed(taken);
        // Remove the closed slot. After removal the target's new index is
        // either unchanged (if it was left of old) or decremented.
        self.tabs.remove(old);
        let new_active = if target > old { target - 1 } else { target };
        self.active_tab = new_active;
        // Clear unread for the now-active tab.
        let slot = &mut self.tabs[new_active];
        slot.last_viewed_seq = slot.activity_seq;
    }

    /// Cycle the active tab by `delta` (positive = next, negative = prev).
    /// Wraps around.
    pub(super) fn cycle_tab(&mut self, delta: i32) {
        let n = self.tabs.len() as i32;
        if n <= 1 {
            return;
        }
        let cur = self.active_tab as i32;
        let next = ((cur + delta).rem_euclid(n)) as usize;
        self.switch_to_tab(next);
    }

    /// Rename the active tab. Empty input clears the user-set name and
    /// falls back to the default (notebook basename or "tab N").
    pub(super) fn rename_active_tab(&mut self, new_name: String) {
        let idx = self.active_tab;
        if let Some(slot) = self.tabs.get_mut(idx) {
            slot.title = if new_name.trim().is_empty() {
                None
            } else {
                Some(new_name)
            };
        }
    }
}

/// Handle global tab-management key combos. Returns `true` if the key
/// was consumed. Fires from any focus so the user can switch / create /
/// rename tabs without first dismissing modals (modals close as a
/// side-effect of the switch — see `App::close_transient_state`).
pub(super) fn try_handle_tab_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Ctrl-T: new tab.
    if ctrl && matches!(key.code, KeyCode::Char('t')) {
        app.new_tab();
        return true;
    }
    // Ctrl-Q: close active tab (refused when only one).
    if ctrl && matches!(key.code, KeyCode::Char('q')) {
        app.close_active_tab();
        return true;
    }
    // Ctrl-Tab / Ctrl-Shift-Tab: cycle next/prev. Some terminals encode
    // Shift-Tab as KeyCode::BackTab so we accept both shapes.
    if ctrl && matches!(key.code, KeyCode::Tab) {
        app.cycle_tab(if shift { -1 } else { 1 });
        return true;
    }
    if ctrl && matches!(key.code, KeyCode::BackTab) {
        app.cycle_tab(-1);
        return true;
    }
    // Alt-1..Alt-9: jump to tab N (1-indexed). Out-of-range is a no-op.
    if alt {
        if let KeyCode::Char(c) = key.code {
            if let Some(d) = c.to_digit(10) {
                if (1..=9).contains(&d) {
                    let idx = (d as usize) - 1;
                    app.switch_to_tab(idx);
                    return true;
                }
            }
        }
    }
    // F2: rename active tab.
    if matches!(key.code, KeyCode::F(2)) {
        begin_rename_tab(app);
        return true;
    }
    false
}

/// Open the rename-tab input bar, pre-filling with the active tab's
/// current title (or "" when the tab uses its default-derived label).
pub(super) fn begin_rename_tab(app: &mut App) {
    let initial = app
        .tabs
        .get(app.active_tab)
        .and_then(|s| s.title.clone())
        .unwrap_or_default();
    app.rename_tab_cursor = initial.len();
    app.rename_tab_input = initial;
    app.rename_tab_input_mode = true;
}

/// Dispatch a key while the rename-tab input bar is open. Enter commits;
/// Esc cancels; readline edits handled via `apply_readline_edit`.
pub(super) fn handle_rename_tab_input_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let new_name = std::mem::take(&mut app.rename_tab_input);
            app.rename_tab_input_mode = false;
            app.rename_tab_cursor = 0;
            app.rename_active_tab(new_name);
        }
        KeyCode::Esc => {
            app.rename_tab_input_mode = false;
            app.rename_tab_input.clear();
            app.rename_tab_cursor = 0;
        }
        _ => {
            let _ = handle_text_input(&mut app.rename_tab_input, &mut app.rename_tab_cursor, &key);
        }
    }
}

/// Drain streams + reap completed for every tab — background tabs need
/// to keep collecting output and reaping their finished processes even
/// when not active. Bumps each tab's activity counter when work
/// happened; for the active tab also pushes `last_viewed_seq` forward
/// so the badge doesn't appear for the tab the user is looking at.
/// Background tabs that produced a `pending_handover` keep it in their
/// stash and act on it next time the user switches to that tab.
pub(super) async fn drive_all_tabs(app: &mut App) {
    let saved_active = app.active_tab;
    // Active tab first.
    let active_a = drain_streams(app);
    let active_b = reap_completed(app).await;
    if active_a || active_b {
        if let Some(slot) = app.tabs.get_mut(saved_active) {
            slot.activity_seq = slot.activity_seq.saturating_add(1);
            slot.last_viewed_seq = slot.activity_seq;
        }
    }
    // Each background tab, temporarily swapped into the App slots.
    for i in 0..app.tabs.len() {
        if i == saved_active {
            continue;
        }
        let prev = app.stash_active();
        let taken = match app.tabs[i].stashed.take() {
            Some(s) => s,
            None => {
                // Restore prev and skip — invariant violated but
                // defensive: don't lose the active tab's state.
                app.restore_stashed(prev);
                continue;
            }
        };
        app.restore_stashed(taken);
        let bg_a = drain_streams(app);
        let bg_b = reap_completed(app).await;
        let new_stash = app.stash_active();
        app.tabs[i].stashed = Some(new_stash);
        app.restore_stashed(prev);
        if bg_a || bg_b {
            if let Some(slot) = app.tabs.get_mut(i) {
                slot.activity_seq = slot.activity_seq.saturating_add(1);
            }
        }
    }
}

/// Render the tab bar across `area` and register click regions for
/// per-tab switching plus the `+` new-tab affordance. The current
/// working directory is rendered right-justified on the same row; tabs
/// truncate from the right before the cwd does.
pub(super) fn draw_tab_bar(f: &mut Frame, area: Rect, app: &App, regions: &mut Vec<ClickRegion>) {
    let active_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let unread_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::DarkGray);
    let plus_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let cwd_style = Style::default().fg(Color::DarkGray);

    // Build the cwd suffix string first so we can reserve its width
    // before laying out tabs. One leading space for visual breathing room.
    let cwd_text = std::env::current_dir()
        .ok()
        .map(|p| collapse_home_in_path(&p))
        .unwrap_or_else(|| "?".into());
    let cwd_label = format!(" {cwd_text} ");
    let cwd_width = cwd_label.chars().count() as u16;

    let area_end = area.x.saturating_add(area.width);
    // Reserve cwd_width on the right (if it fits); tabs lay out up to that.
    let tab_limit = if cwd_width < area.width {
        area_end.saturating_sub(cwd_width)
    } else {
        area_end
    };

    let buf = f.buffer_mut();
    let mut x = area.x;
    for (i, slot) in app.tabs.iter().enumerate() {
        if x >= tab_limit {
            break;
        }
        let label = format!(" {} {} ", i + 1, slot.display_title(i));
        let style = if i == app.active_tab {
            active_style
        } else if slot.has_unread() {
            unread_style
        } else {
            normal_style
        };
        let label_width = label.chars().count() as u16;
        let drawn_width = label_width.min(tab_limit.saturating_sub(x));
        if drawn_width == 0 {
            break;
        }
        buf.set_string(x, area.y, &label, style);
        regions.push(ClickRegion {
            rect: Rect {
                x,
                y: area.y,
                width: drawn_width,
                height: 1,
            },
            action: ClickAction::SwitchTab(i),
        });
        x = x.saturating_add(drawn_width);
        if x < tab_limit {
            buf.set_string(x, area.y, "│", normal_style);
            x = x.saturating_add(1);
        }
    }
    // `+` new-tab affordance.
    if x.saturating_add(3) <= tab_limit {
        buf.set_string(x, area.y, " + ", plus_style);
        regions.push(ClickRegion {
            rect: Rect {
                x,
                y: area.y,
                width: 3,
                height: 1,
            },
            action: ClickAction::NewTab,
        });
    }
    // cwd, right-justified.
    if cwd_width <= area.width {
        let cwd_x = area_end.saturating_sub(cwd_width);
        buf.set_string(cwd_x, area.y, &cwd_label, cwd_style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_starts_with_one_tab() {
        let app = App::new();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, 0);
        assert!(app.tabs[0].stashed.is_none());
    }

    #[test]
    fn new_tab_pushes_and_switches() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["tab0".into()]);
        let idx = app.new_tab();
        assert_eq!(idx, 1);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, 1);
        assert!(app.session.sheds().next().is_none());
        let stash = app.tabs[0].stashed.as_ref().expect("tab 0 stashed");
        assert_eq!(stash.session.sheds().count(), 1);
    }

    #[test]
    fn switch_to_tab_swaps_state_round_trip() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        let _ = app.new_tab();
        let _ = app.session.add_shed(vec!["b".into()]);
        app.switch_to_tab(0);
        let argvs: Vec<String> = app.session.sheds().map(|s| s.argv.join(" ")).collect();
        assert_eq!(argvs, vec!["a".to_string()]);
        app.switch_to_tab(1);
        let argvs: Vec<String> = app.session.sheds().map(|s| s.argv.join(" ")).collect();
        assert_eq!(argvs, vec!["b".to_string()]);
    }

    #[test]
    fn cycle_tab_wraps_in_both_directions() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.new_tab();
        let _ = app.new_tab();
        assert_eq!(app.active_tab, 2);
        app.cycle_tab(1);
        assert_eq!(app.active_tab, 0);
        app.cycle_tab(-1);
        assert_eq!(app.active_tab, 2);
        app.cycle_tab(-1);
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn close_last_tab_is_refused_with_flash() {
        let mut app = App::new();
        app.history.clear();
        app.close_active_tab();
        assert_eq!(app.tabs.len(), 1);
        assert!(app.flash.is_some());
    }

    #[test]
    fn close_active_tab_drops_slot_and_picks_neighbour() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        let _ = app.new_tab();
        let _ = app.session.add_shed(vec!["b".into()]);
        let _ = app.new_tab();
        let _ = app.session.add_shed(vec!["c".into()]);
        app.close_active_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, 1);
        let argvs: Vec<String> = app.session.sheds().map(|s| s.argv.join(" ")).collect();
        assert_eq!(argvs, vec!["b".to_string()]);
    }

    #[test]
    fn switch_to_tab_clears_unread_for_destination() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.new_tab();
        app.tabs[0].activity_seq = 5;
        app.tabs[0].last_viewed_seq = 0;
        assert!(app.tabs[0].has_unread());
        app.switch_to_tab(0);
        assert!(!app.tabs[0].has_unread(), "switching clears the badge");
    }

    #[test]
    fn rename_active_tab_sets_and_clears_title() {
        let mut app = App::new();
        app.history.clear();
        app.rename_active_tab("logs".into());
        assert_eq!(app.tabs[0].title.as_deref(), Some("logs"));
        app.rename_active_tab("   ".into());
        assert!(app.tabs[0].title.is_none());
    }

    #[test]
    fn display_title_falls_back_to_default_when_unset() {
        let mut app = App::new();
        app.history.clear();
        assert_eq!(app.tabs[0].display_title(0), "tab 1");
        app.rename_active_tab("greppy".into());
        assert_eq!(app.tabs[0].display_title(0), "greppy");
    }

    #[test]
    fn alt_digit_jumps_to_tab() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.new_tab();
        let _ = app.new_tab();
        // Alt-1 → tab 0
        let key = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT);
        assert!(try_handle_tab_key(&mut app, key));
        assert_eq!(app.active_tab, 0);
        // Alt-2 → tab 1
        let key = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT);
        assert!(try_handle_tab_key(&mut app, key));
        assert_eq!(app.active_tab, 1);
        // Alt-9 with only 3 tabs is a no-op (switch_to_tab guards bounds).
        let key = KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT);
        assert!(try_handle_tab_key(&mut app, key));
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn ctrl_tab_cycles() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.new_tab();
        let _ = app.new_tab();
        app.switch_to_tab(0);
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL);
        try_handle_tab_key(&mut app, key);
        assert_eq!(app.active_tab, 1);
        try_handle_tab_key(&mut app, key);
        assert_eq!(app.active_tab, 2);
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        try_handle_tab_key(&mut app, key);
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn ctrl_t_creates_new_tab() {
        let mut app = App::new();
        app.history.clear();
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(try_handle_tab_key(&mut app, key));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn f2_opens_rename_input_bar() {
        let mut app = App::new();
        app.history.clear();
        app.tabs[0].title = Some("oldname".into());
        let key = KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);
        assert!(try_handle_tab_key(&mut app, key));
        assert!(app.rename_tab_input_mode);
        assert_eq!(app.rename_tab_input, "oldname");
    }
}
