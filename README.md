# shed

> An interactive shell where the pipeline comes after the command.

shed is a TUI shell (Linux/macOS) that captures every command's output as a
structured **shed** and lets you build pipelines **retroactively** — adding,
editing, removing, and reordering filters with live preview. The killer
feature is that you don't have to know what filter you want before you run
the command. Run it; look at the output; sculpt the pipeline.

## Status

Pre-alpha. Single developer, ~80 commits in. All v0 filter primitives and
the core focus model are in place. Not packaged or distributed; build from
source.

## Demo

```
  shed v0.0.0  ·

  %1 ● ls -la /etc
       from-fields │ where _5 > 1000  ⓘ-1
       _1          _2  _3    _4    _5      _6   _7  _8     _9
       drwxr-xr-x  3   root  root  4096    Apr  29  10:42  systemd
       -rw-r--r--  1   root  root  1287    Apr  28  17:39  passwd
       -rw-r--r--  1   root  root  9012    Apr  28  17:39  services
       … 47 more rows

  %2 ⏵ cargo build --release
       Compiling foo v0.1.0
       Compiling bar v0.4.2

  ─────────────────────────────────────────────────────────────────
  ▶ |
   Enter run · !cmd fullscreen · Esc focus shed · Ctrl-D quit
```

The yellow `ⓘ-1` next to `where _5 > 1000` says: *one row was dropped
because it had no `_5` value to compare* (the `total 4567` summary line at
the top of `ls -la`'s output).

## Quick start

```bash
git clone <repo> shed
cd shed
cargo run                          # fresh empty session
cargo run -- ./demo.json           # open a notebook (created if absent)
```

A walkthrough:

1. Type a command — `ls -la /etc` — and press **Enter**. Shed `%1` appears
   with colored output (shed runs commands in a PTY so terminal-aware
   programs emit color).
2. Press **Esc** to focus the newest shed. The prompt at the bottom
   changes to `(shed %1 selected)`. The shed highlights in cyan.
3. Press **f** to open the filter form. You're now in *FilterEdit* focus:
   the screen splits into preview / pipeline stack / form.
4. The form's first field is the filter type. Use **←→** to cycle:
   `from-lines`, `from-fields`, `from-csv`, `from-json`, `from-regex`,
   `where`, `select`, `drop`, `take`, `skip`, `sort-by`, `uniq`, `count`,
   `rename`, `split`, `join`, `parse-time`, `pipe`, `to-json`,
   `combine`. Pick `from-fields` and press **Enter**.
5. The shed now shows columns `_1` through `_9` and the inline pipeline
   reads `from-fields`.
6. Press **f** again to add another filter. The form defaults to `where`
   because the schema is non-empty. **Tab** through fields:
   `column` → cycle to `_5`; `op` → cycle to `>`; `value` → type `1000`.
   Watch the preview shrink as you type.
7. Press **Enter** to apply; **Esc** to return to the prompt.

To quit: `Ctrl-D`.

## Concepts

### Tabs

shed supports multiple tabs, each with its own session, prompt, focus,
undo/redo stack, and notebook binding. Commands running in a background
tab keep streaming output; the tab bar shows a yellow indicator on any
tab with activity since you last viewed it.

| Key                 | Action |
|---------------------|--------|
| Ctrl-T              | new tab |
| Ctrl-Q              | close active tab (refused when only one) |
| Ctrl-Tab            | next tab (wraps) |
| Ctrl-Shift-Tab      | previous tab (wraps) |
| Alt-1 … Alt-9       | jump to tab N (clamps to last tab) |
| F2                  | rename the active tab (Esc to cancel; empty input resets to default) |
| click on a tab      | switch to it |
| click `+` in the bar | new tab |

Tab title defaults to the notebook basename when one is loaded,
otherwise `tab N`. A user-set title (via F2 or the palette's "Rename
tab") wins over both.

With more than one tab open, **Ctrl-D**, `exit`, and `quit` close the
active tab rather than quitting the program — tabs are independent
sessions, so the shell-style "close this view" behaviour applies. The
last tab does quit (or prompts to save unsaved pinned changes). To
preserve a tab's pinned-shed work before closing it, Ctrl-S first;
closing an earlier tab is otherwise a deliberate discard.

Background semantics: drain + reap run for every tab each tick, so
output streams in and finished children get reaped no matter which tab
is active. Pending fullscreen handover and new chain dispatch only
fire for the active tab — they pick up next time you switch to a tab
that has them queued. Switching tabs closes any open modal/input bar
in the source tab (palette, FilterEdit, rerun bar, etc.) — the
destination tab lands clean on its persistent state.

### Sheds

Each command spawns a *shed* with:

- captured stdout (PTY merges stderr into stdout; cap defaults to 16 MB)
- exit code
- a retroactive pipeline of filters
- a monotonic id (`%1`, `%2`, …) and an optional pinned name (`◉ name`)

Sheds live in a session-wide store. Unnamed sheds evict on memory
pressure (LRU); pinned sheds count toward the budget but never evict.
Pin a shed with `p` from ShedCursor — it renders with `◉ <name>`
next to its command. `u` unpins.

Each shed's top rule carries a clickable `[×]` button at its right end
— left click on it to delete that shed. Same effect as pressing `x` with the
shed selected, and equally undoable via `Ctrl-Z`. Deleting a shed
whose command is still running asks for confirmation first (`y` to
delete and kill the child, `n` to keep it). Mouse capture is enabled
automatically and toggled off during fullscreen handover so the child
program owns the terminal.

### Pipelines

A pipeline is an ordered list of filters applied to a shed's captured
stdout. Filters fall into four classes:

- **Parsers** turn raw bytes into structured rows: `from-lines`,
  `from-fields`, `from-csv`, `from-json`, `from-regex`.
- **Row transforms** keep/reshape rows: `where`, `take`, `skip`, `uniq`,
  `sort-by`.
- **Column transforms** reshape the schema: `select`, `drop`, `rename`.
- **Aggregations** collapse to a summary: `count`.

A pipeline runs on every redraw, so previews update live as you tune a
filter. Captures are bounded (16 MB) so re-running the pipeline is cheap.

### Focus model

Each shed is headed by a single top rule that carries the identifier
(`%5` or `@name` for pinned sheds) plus a state glyph; the body runs
edge-to-edge below it with no side borders. A single "scratch" box at
the bottom is the place to type a new command — it's always present and
takes the next id.

- **ShedCursor** — navigate sheds (`↑↓`); the scratch sits one slot
  past the last real shed. Compact view: id/name + glyph + argv in
  the top border, output in the body. To reveal each filter on its own
  navigable line, press `e` to enter EditShed. Press `v` to open the
  fullscreen pager. Press `/` to jump to the prompt and start typing.
- **EditShed** — pipeline-edit mode for the cursor shed. The argv is
  shown on the first line and each filter on its own indented line;
  `←→` navigates between the command and the filters; `f`/Enter opens
  the form editor for the active slot; `i` inserts, `d` drops, `<>`
  reorder. `Esc` returns to ShedCursor.
- **Prompt** — typing in the scratch box. `Enter` runs (and the new
  shed appears in place of the scratch; a fresh scratch is drawn for
  the next command). `Esc` returns to ShedCursor on the last shed.
  `↑↓` walk persistent history.
- **FilterEdit** — schema-aware form for the active filter, with live
  preview at the top.

### Concurrency

Commands run in tokio tasks; the TUI never freezes. Multiple commands can
run side-by-side. Each shed carries a status glyph:

| glyph | meaning |
|------:|---------|
| `⏵`   | running |
| `●`   | done, replayable |
| `⚠`   | failed (or non-zero exit) |
| `✂`   | output truncated at the cap |
| `❄`   | snapshotted from a stream (planned) |
| `◉`   | pinned with a name (planned UI) |

Below the output, each shed that has run carries a status footer: `✓`
plus the elapsed time on success, `✗ exit N · <time>` on a non-zero
exit, `✗ <reason>` when the command couldn't be spawned, and `⏵
running <time>` (ticking live) while it's still in flight. Once a
command finishes, the wall-clock time it finished (`HH:MM:SS`) is shown
right-aligned at the end of that line.

### PTY-based capture

shed spawns commands attached to a pseudo-terminal so terminal-aware
programs (`ls --color`, `cargo build`, `git status`, …) emit colored
output. The captured bytes include ANSI escape sequences; the renderer
parses SGR (color/style) sequences via `vte` and emits ratatui-styled
spans. Cursor-positioning sequences are dropped, so non-curses programs
render cleanly. Programs that try to take over the screen get
**fullscreen handover** instead.

Output streams in: each chunk the reader pulls off the PTY is sent
through an mpsc channel that the event loop drains every tick and
mirrors onto the shed's `capture` while the command is still running.
The pipeline re-applies on every render, so `from-lines | where …`
shows partial rows accumulate in real time. The final `Capture` (with
exit code + finished timestamp) replaces the partial one when the
reader task completes.

### Shed previews and inline scrolling

An *unselected* shed shows a compact preview: the last few lines of its
output (with `… N more` pinned to the top), so the most recent activity
is always what you see. Selecting a shed (ShedCursor focus) **expands**
it — the box grows to show as much of the full output as fits, pushing
neighbouring sheds aside. When the output is taller than the expanded
box, the body becomes scrollable:

- `j` / `k` scroll one line, PgDn / PgUp scroll a page.
- The mouse wheel scrolls the shed under the pointer (and selects it).
- The top rule shows the visible line range, e.g. ` 12–31/86 `.

For heavier scrolling — search, page jumps, whole-output inspection —
`v` still opens the fullscreen pager.

### Builtins

A few commands aren't external programs and are handled inside shed
itself:

- **`cd [path]`** — change shed's current working directory. Subsequent
  commands inherit the new cwd. Without an argument, `cd` goes to
  `$HOME`. `cd -` swaps with the previous cwd. `~` and `~/path` are
  expanded. The cwd shows in the header bar (`shed  ·  ~/devel/shed`).
- **`exit`** / **`quit`** — close the active tab; quits shed when only
  one tab is open. Same semantics as Ctrl-D.
- **`export KEY=VALUE [KEY=VALUE ...]`** — set environment variables in
  shed's process; subsequent spawned commands inherit them. `export`
  with no args, or a bare key without `=`, is rejected.
- **`unset NAME [NAME ...]`** — remove environment variables.

Most other "shell-isms" (`|`, `>`, `&&`, `$(…)`) are deliberately not
supported — see the design constraints below. Use `bash -c '…'` if you
need real shell semantics.

### Pinned references (`@name`)

A pinned shed (`p` to pin) gets a name. Type `@name` (or `%N` for any
shed by id, pinned or not) at the prompt and shed creates a new shed
whose capture is a *snapshot* of the source's current pipeline output:

- structured output (rows from a parser) carries over as typed rows —
  the downstream shed's pipeline sees the columns immediately, in the
  source's original order, with no explicit `from-json` needed
- raw bytes are passed through unchanged

The snapshot is taken at create time. It does *not* re-evaluate when the
source changes — to refresh, focus the snapshot shed and press `Space`
(re-run in place re-snapshots from the source's current state). If the
source is gone or has no capture, the snapshot shed lands in `Failed`
state with a message saying so.

**Dependency auto-run.** If you run a snapshot shed (`Space` or typing
`@name`) and the source itself hasn't run yet, shed walks the chain and
runs the prereqs first, in dep order. A flash message tells you what's
about to happen (`running 2 deps first, then %5`). If a prereq fails, the
rest of the chain is dropped rather than running against a stale source.
Cycles (`@a` pinned as `a`) terminate via cycle-detection.

`@name` sheds save and load through notebooks like any other shed (the
argv is just `@name`); on re-open they're `Idle` and re-snapshot when
you run them.

### Outputs and argv interpolation

A shed can declare named **outputs** that downstream sheds consume via
`${...}` interpolation in their argv. The classic motivating case is
the tofu plan / apply workflow: `tofu plan` writes a plan file that
`tofu apply` then reads, and you want the apply step to run only after
plan succeeds, against the same file.

Two output flavours in v0:

| Spec | Behaviour |
|------|-----------|
| `Literal(string)` | A fixed value, always resolvable, even before the shed runs. Use for stable paths or constants. |
| `TempPath` | Each spawn generates a fresh path under `$TMPDIR` (e.g. `/tmp/shed-1-plan-1763458920123`) and substitutes it into argv. The file is **not** created — the command is expected to write to it. |

Interpolation grammar inside argv strings:

- `${name}` — the shed's *own* declared output.
- `${@source}` / `${%N}` — the source's trimmed stdout (like shell `$(...)`).
- `${@source.name}` / `${%N.name}` — the source's named output.

Interpolations are resolved at spawn time. The source must have ended
in `Done(0)` or `Snapshotted` for the lookup to succeed — a still-
running, idle, failed, or non-zero-exit source returns an error and
the dependent shed lands in `Failed` with a clear message. That's the
safety property: shed 3 (`tofu apply ${@tofuplan.plan}`) cannot run
on a stale or failed plan.

The run chain machinery walks interpolation references the same way it
walks `@name` snapshot refs — so `Space` on the apply shed auto-runs
the plan shed first, then apply, in order. Re-running a source
regenerates its `TempPath` outputs; downstream sheds running after
will pick up the new values.

Notebooks persist output **declarations** (the `outputs:` map); the
**values** are runtime state, recomputed each spawn.

In **EditShed** (`e` on a shed), an `outputs:` section appears below the
filters with one row per declared output plus a `+ add output` slot.
Use `↓` to walk down from the filters into the outputs section, then:

| Key | Action |
|-----|--------|
| `i` | open the input bar to add a new output |
| `f` / `Enter` | edit the output under the cursor (or `+ add` for a new one) |
| `d` / `x` | drop the output under the cursor |

The input bar takes `name=TempPath` for a generated temp file path, or
`name=value` for a literal string. The "Add output" command-palette
entry jumps straight here from anywhere.

### Shed notes (pre / post text)

Each shed can carry two free-form text notes — `pre_text` shown above
the shed, `post_text` shown below. Use them for section headers,
running commentary, observations about the output, or anything else you
want to preserve alongside the command.

| Key | Action |
|-----|--------|
| `n` | edit the pre-note (above the shed) |
| `N` | edit the post-note (below the shed) |

In the editor: type to insert at the cursor, `Enter` for newline,
`Backspace` / `Delete`, `←→ ↑↓ Home End` to navigate, `Ctrl-S` to save,
`Esc` (or `Ctrl-C`) to cancel. An empty buffer clears the note.

Notes render dimmed and italicized with a `▎` left edge so they're
visually distinct from command output. They round-trip through
notebooks alongside argv, name, and pipeline.

### Aliases

A *global* alias is a saved `(argv, pipeline)` pair addressed by name.
Type the name at the prompt and shed materialises a fresh shed with
that argv and pipeline pre-filled, then drops you into in-place command
edit so you can append args before running:

```
list           ↵    →   shed %N appears with argv `ls -lat`,
                         cmd-edit bar shows  `ls -lat ` (cursor at end)
list /etc      ✗         (alias lookup is single-token only)
```

Aliases shadow real binaries — an alias named `ls` wins over `/usr/bin/ls`.
Use `bash -c 'ls'` if you need to bypass.

| How                      | Action |
|--------------------------|--------|
| `A` (in ShedCursor)     | save the cursor shed as an alias (input bar `alias name:`). If the name already exists, a `[y]es / [n]o` prompt appears before overwriting. |
| type `<name>` at prompt  | invoke alias — creates an Idle shed, opens cmd-edit pre-filled with `argv ` (trailing space) so you can append args + Enter. |
| `/aliases` at prompt     | open the manage view: `↑↓` navigate, `x` (or `d`) delete, `Esc` / `q` back. |

Storage is `$XDG_CONFIG_HOME/shed/aliases.json` (fallback
`~/.config/shed/aliases.json`), versioned JSON, written on every change.
Aliases are cross-session and not tied to any single notebook.

Edit by re-invoking: type the alias, modify the argv / pipeline as
needed, then `A` again with the same name and confirm overwrite.

### Undo / redo

`Ctrl-Z` and `Ctrl-Y` step backwards and forwards through every
*structural* change you've made: adding a shed, deleting a shed,
pinning / unpinning, editing the command, adding / dropping / reordering
filters, editing pre / post notes, saving a shed as an alias. The
history is in-memory only (not persisted across sessions) and capped at
50 entries.

Captures and run-state are preserved across an undo/redo round-trip:
adding a filter, undoing, and redoing won't reset your shed to Idle —
the output you've already produced stays put. The one exception is
undoing the *creation* of a shed: the shed disappears (and so does
its capture) until you redo. Resurrecting a deleted shed via undo
brings back its argv / pipeline / notes, but its capture comes back as
whatever the snapshot held (no fresh run is triggered).

Running a command (Space, Enter, re-snapshot of `@name`) is *not*
undoable — re-runs are runtime, not structural, and don't dirty the
notebook. Undo restores notebook structure; if you want to revert a
re-run's output, save before re-running and reload.

### Copy and paste

shed enables mouse capture so it can route clicks to the per-shed `[×]`
buttons and the right-click context menu, which blocks the terminal's
native click-and-drag selection. Two ways to get text out:

- **Shift-drag** to bypass mouse capture and use the terminal's own
  selection (works in kitty, iTerm2, wezterm, alacritty, foot, modern
  xterm; behaviour varies by emulator). Whatever you select is sent to
  the clipboard the same way as in a normal terminal.
- **Right-click** on a shed body opens a small context menu. Items depend
  on what's under the cursor:
  - On a **table cell** (structured row output):
    - **Copy cell** — the cell's typed value as text
    - **Add cell to prompt** — splice it into the prompt (prompt focus only)
    - **Copy filename** — for path-like cells (`/etc/passwd`, `./foo.txt`,
      `~/dir`, anything with `/` and no whitespace), just the basename
    - **Add filename to prompt** — same, into the prompt
  - On a non-cell line (or non-table output):
    - **Copy line** — the rendered line under the cursor
    - **Add line to prompt** — splice that line into the prompt
  - Always available:
    - **Copy whole output** — the shed's raw captured stdout
      (pre-pipeline), or the structured table as text for snapshot sheds

  Navigate with `↑↓`, **Enter** to activate, **Esc** to dismiss. Clicking
  outside the menu also dismisses.
- **Shift-Left-Click** *or* **Ctrl-Left-Click** on a cell or line is a
  shortcut for "add to prompt" — skips the menu, splices the cell value
  (or the line, if no cell is hit) into the prompt at the cursor, and
  switches focus to Prompt if you weren't already there. The `[×]`
  delete button is ignored while a modifier is held so the click reaches
  the body behind it. Both modifiers are accepted because most terminals
  (kitty, iTerm2, wezterm, alacritty, foot, modern xterm) intercept
  Shift+click to bypass mouse capture for their own text selection,
  while a few use Ctrl+click for "open link" — whichever your terminal
  lets through wins.

Copy uses OSC 52 to write to the system clipboard (both CLIPBOARD and
PRIMARY selections, for X11 environments). Most modern terminals support
it; some older ones silently ignore it.

Inside **tmux**, OSC 52 forwarding requires `set -g set-clipboard on` in
your tmux config — without it tmux discards the sequence. Once that's
on, tmux forwards application OSC 52 to the outer terminal natively (no
extra `allow-passthrough` setting needed). Reload tmux config and try
again if the first copy attempt doesn't land.

Type-aware cell actions (copy a specific column value, open a URL, `cd`
to a path) are planned for the same menu once structured-row filters
land and the renderer can map screen coordinates back to a field.

### Tab completion

Available at the Prompt and inside the in-place command editor (the bar
that appears after `e` → `f` on a shed's command). Tab cycles forward
through matches; Shift-Tab cycles backward. There's no popup list — the
input itself is rewritten to the next candidate. Any non-Tab keystroke
ends the cycle.

The token under the cursor (everything after the last whitespace)
decides what to complete:

| Token shape | Source |
|-------------|--------|
| `$FOO`      | environment variable names |
| `@name`     | pinned shed names |
| `/cmd` (Prompt only, first token) | slash commands (`/aliases`) |
| first token, otherwise | commands on `$PATH` ∪ saved aliases ∪ shell builtins (`cd`, `export`, `unset`, `exit`) |
| second token onward, or any path-shaped token (`./`, `../`, `/...`, `~/`) | [carapace](https://carapace.sh/) if installed and it returns matches for the command (e.g. `git checkout <Tab>` → branch names, `kubectl --<Tab>` → flag names); otherwise filesystem paths (directories get a trailing `/`; hidden entries appear only when the prefix starts with `.`) |

If `carapace` is on `$PATH`, shed shells out to `carapace <argv0> export <argv0> <tokens>` for argv1+ completion. It ships completion specs for ~1000 commands, so this is the easiest way to get rich, command-aware completions without writing per-command logic in shed. If carapace isn't installed (or doesn't know the command), shed falls back to plain filesystem path completion.

If no candidates match, the flash bar shows "no completions" and the
input is left untouched.

### Line editing

Every single-line input bar in shed (the main prompt, in-place command
editor, pin / rerun / write / alias-name / save / open paths, pager
search) supports readline-style editing. The cursor is rendered inline
as an inverted shed; arrow keys move it; characters insert at the
cursor; Backspace / Delete remove around it.

| Key | Action |
|-----|--------|
| `←` / `→`         | move one char left / right |
| Ctrl-B / Ctrl-F   | same as `←` / `→` |
| Alt-`←` / Alt-`→` | move one word left / right |
| Alt-B / Alt-F     | same as Alt-`←` / Alt-`→` |
| Home / End        | jump to beginning / end |
| Ctrl-A / Ctrl-E   | same as Home / End |
| Backspace         | delete char before the cursor |
| Delete            | delete char under the cursor |
| Ctrl-W            | kill the word before the cursor |
| Ctrl-U            | kill from cursor to beginning of line |
| Ctrl-K            | kill from cursor to end of line |

The palette lives on **Ctrl-P** (so that **Ctrl-K** is free for
kill-to-end), and the env editor is reachable via the palette only
(rather than a Ctrl-E shortcut, which is now end-of-line).

### Notebooks

A *notebook* is the saveable form of a session: an ordered list of
commands plus the retroactive pipeline you built around each one. The
on-disk format is JSON (versioned via a top-level `version` field) and
holds *only* structure — argv, optional pinned name, pipeline. Captures,
exit codes, and timestamps are not persisted on purpose: a notebook is a
recipe, not a frozen view.

| Action                 | How |
|------------------------|-----|
| Open a notebook on launch | `shed PATH.json` (the file is created if it doesn't exist; it just binds the save target) |
| Save                   | `Ctrl-S` — saves to the bound path; if none, opens an input bar to pick one |
| Save as / open another | `Ctrl-O` — input bar to load a different notebook (replaces the current session) |
| Run a loaded shed     | move the cursor onto it (`Esc`, `↑↓`) and press `Space` (or `x`). Loaded sheds start in `Idle` state with a hollow `○` glyph; running them swaps the capture in place. |
| Quit with unsaved work | `Ctrl-D` shows `unsaved changes — save before quitting? [y]es [n]o [c]ancel` instead of quitting straight away |

Shed re-runs and pipeline edits set a *dirty* flag. Save clears it;
quitting while dirty triggers the confirmation prompt. `Ctrl-S` from
anywhere (Prompt, ShedCursor, even mid-FilterEdit) writes the current
session out.

### Fullscreen handover

For interactive programs (`top`, `vim`, `less`, `man`, `tmux`, `ssh`,
`tig`, `ranger`, `fzf`, …), shed temporarily yields the entire terminal:
tears down its TUI, runs the child with inherited stdio, restores. The
shed records the exit code (shown in its status footer) with no
captured output.

Detection has three paths, in order:
- **Built-in blacklist** of common interactive programs — handover
  before spawn.
- **Explicit `!` prefix** — type `!cmd` to force handover for anything
  not on the blacklist.
- **Auto-detect at runtime** — the PTY reader watches each chunk for
  alt-screen-enter sequences (`\x1b[?1049h` and the older
  `\x1b[?47h` / `\x1b[?1047h`). If it sees one mid-capture, it kills
  the child, signals the TUI, and the same shed is retried with
  inherited stdio. A flash message confirms: `%N switched to
  fullscreen mode`. Means programs not on the blacklist (`htop` aliases,
  custom TUIs, …) Just Work after the first attempt.

## Filter reference

| Filter        | Class       | Params                    | Notes |
|--------------:|-------------|---------------------------|-------|
| `from-lines`  | parser      | (none)                    | one record per line, column `line` |
| `from-fields` | parser      | (none)                    | whitespace split; columns `_1`, `_2`, … (max width across all rows) |
| `from-csv`    | parser      | `delim`, `has_header`     | flexible — mixed-field rows survive |
| `from-json`   | parser      | (none)                    | array of objects → rows; object → 1 row; scalar → wrapped in `value` column |
| `from-regex`  | parser      | `pattern`                 | named captures become columns; non-matching lines are dropped |
| `where`       | row filter  | clauses + AND/OR          | each clause is `column`/`op`/`value` with op in `matches` (regex), `contains` (substring), `=`, `≠`, `<`, `≤`, `>`, `≥`. Multiple clauses combine with a single AND or OR. Numeric coercion when both sides parse. Lenient per-row (drops rows on null/type mismatch); hard-fail on bad regex / unknown column. |
| `select`      | columns     | `columns`                 | keep listed columns in given order |
| `drop`        | columns     | `columns`                 | remove listed columns |
| `rename`      | columns     | pairs                     | each form row is one column → new name (blank means unchanged) |
| `take`        | rows        | `n`                       | first N rows |
| `skip`        | rows        | `n`                       | drop first N rows |
| `sort-by`     | rows        | `keys` (up to 5)          | each key is (column, asc/desc); numeric coercion when both sides parse |
| `uniq`        | rows        | `by` (optional)           | dedupe by all columns by default; if `by` set, dedupe keyed by those columns |
| `count`       | aggregation | (none)                    | single row `{count: N}` |
| `split`       | rows        | `column`, `delimiter`     | each row's `column` is split into pieces; one row per piece, other columns duplicated |
| `join`        | rows        | `column`, `delimiter`     | concatenate every row's `column` value with `delimiter` into a single row; other columns dropped |
| `parse-time`  | columns     | `columns`                 | space-join the chosen columns, parse as a datetime, replace the first column with the result (others dropped). Renders as relative time ("3 minutes ago") and sorts chronologically; an unparseable join keeps the raw string |
| `pipe`        | bytes       | `argv`                    | spawn an external command (`awk`, `jq`, `sed`, …), write the current pipeline bytes to its stdin, replace the pipeline with its stdout. Requires `Bytes` input — chain `to-json` first when piping structured data. Each invocation is cached per shed by `(argv, input-bytes-hash)`, so steady-state renders don't re-spawn. Non-zero exits and a 5s timeout surface as filter errors. |
| `to-json`     | bytes       | (none)                    | serialize structured input to compact JSON bytes; `Bytes` input passes through unchanged. The bridge between structured pipelines and `pipe`. |
| `combine`     | columns     | `range`, `separator`      | merge a contiguous slice of columns into the first one of the slice, joined by `separator`; the rest are dropped. `range` is a comma-separated list of 1-based positions over the current schema — `3-9`, `1, 2, 3-9`, or the headline `11-` ("everything from column 11 onward"). Useful when `from-fields` over-splits a value (e.g. `ps aux`'s command spans `_11..`). |

### The `⓲ -N` annotation

When a `where` filter silently drops rows because the predicate errored
on those rows (e.g., `Null > 1000`), an inline yellow `ⓘ-N` appears next
to that filter. The filter still hard-fails on schema-level mistakes
(unknown column, bad regex) — those show as a red `filter error: …`.

This means rows that don't fit the predicate's type expectations
disappear silently the way SQL handles NULLs, but the count is visible
so you don't lose data without noticing.

## Keybindings

### Prompt

| Key       | Action |
|-----------|--------|
| Enter     | run command |
| `↑` / `↓` | recall previous / next command from history (persisted across sessions in `$XDG_CACHE_HOME/shed/history`, default `~/.cache/shed/history`) |
| Tab / Shift-Tab | cycle through completions for the token at the cursor (see Tab completion). |
| Ctrl-A / Ctrl-E | jump to beginning / end of line |
| Ctrl-U / Ctrl-K | kill to beginning / end of line |
| Ctrl-W          | kill the word before the cursor |
| Ctrl-B / Ctrl-F | move one char left / right (same as ← / →) |
| Alt-B / Alt-F   | move one word left / right (same as Alt-← / Alt-→) |
| Home / End      | jump to beginning / end of line |
| Left / Right    | move one char |
| Delete          | delete the char under the cursor |
| `!cmd`    | force fullscreen handover (typed prefix) |
| `@name`   | snapshot the output of pinned shed `@name` into a new shed (see Pinned references) |
| Esc       | focus newest shed |
| Ctrl-D    | quit (or prompt if unsaved) |
| Ctrl-C    | quit (no running selection) |
| Ctrl-P    | open the command palette |
| Ctrl-S    | save notebook (input bar if no path bound) |
| Ctrl-O    | open notebook (replaces the current session) |
| Ctrl-Z / Ctrl-Y | undo / redo the most recent structural change (see Undo / redo) |

### ShedCursor

The compact, default focus. Each shed is headed by a top rule carrying
`id/name + glyph + argv`; the body below it is just the output. The
per-filter pipeline detail stays hidden until you press
`e` to enter EditShed. Pressing `↓` past the last real shed *selects*
the scratch box (still in ShedCursor, rendered in cyan); `↑` walks
back to the last shed, and Enter / Space / `e` activates the scratch
for typing (focus shifts to Prompt, rendered in green).

| Key       | Action |
|-----------|--------|
| `↑↓`      | navigate between sheds; `↓` past the last shed selects the scratch box (still ShedCursor); `↑` from the scratch returns to the last shed. |
| `j` / `k` | scroll the selected shed's body down / up by one line. |
| PgDn / PgUp | scroll the selected shed's body down / up by a page. |
| `e`       | enter EditShed — reveals the command and each filter on its own line so you can navigate / edit them. On the scratch box, `e` activates the prompt for typing. |
| `v`       | view the selected shed in the fullscreen pager. |
| `/`       | jump to the scratch (Prompt) with `/` typed for slash commands like `/aliases`. |
| Enter     | on the scratch box, activate the prompt for typing. |
| Space     | run the selected shed in place (re-spawns its argv, replaces the capture). On the scratch box, activate the prompt. For `@name` snapshot sheds this re-snapshots from the source. |
| `x`       | delete the selected shed from the session. If its command is still running, a `y`/`n` confirmation is shown first (deleting kills the child). Cursor advances to the next sibling, or returns to the prompt if the session becomes empty. |
| `w`       | write the shed's filtered output to a file path you type. The output format is inferred from the path extension: `.csv` → comma-separated, `.tsv` → tab-separated, `.json` → pretty JSON, anything else → plain text. |
| `p`       | pin the selected shed under a name (input bar pre-fills with the existing name if any). Pinned sheds render their box title as `@name` and never evict on capture-budget pressure. Empty name unpins. |
| `u`       | unpin the selected shed (clear its name) |
| `r`       | open a rerun input bar pre-filled with the shed's argv (shlex-quoted). Edit and Enter to spawn a new shed with the edited command and the same pipeline copied over; Esc cancels. The original shed is unchanged. |
| `n` / `N` | edit the shed's pre-note / post-note (multi-line text rendered above / below the shed, persisted to the notebook) |
| `A`       | save the shed as a global alias (input bar; overwrites prompt for confirmation) |
| Ctrl-C    | cancel a running command (kills the child) |
| Ctrl-S    | save notebook |
| Ctrl-O    | open notebook |
| Esc       | back to prompt |
| Ctrl-D    | quit (or prompt if unsaved) |

### EditShed

Entered by pressing `e` on a shed. The shed's command appears on the
first line; each filter on its own indented `│ filter` row below it.
Shed-level actions (run, delete, pin, etc.) live one focus up — `Esc`
returns to ShedCursor for those.

| Key       | Action |
|-----------|--------|
| `↑↓`      | navigate vertically through the command, each filter, the `+ add filter` slot, each output row, and the `+ add output` slot. `↑` at the first filter steps onto the command (highlighted in magenta); `↓` from the command returns to the filter list. `←→` are aliases for muscle memory. |
| `f` / Enter | edit the active slot — opens the filter form for a filter, the in-place command editor when the command is focused, or the `name=spec` input bar when an output is focused. Committing a command edit re-runs the shed; if the shed is pinned, every shed whose argv is `@<name>` (and theirs, recursively) is queued to re-run too. |
| Tab / Shift-Tab | (in-place command editor only) cycle through completions for the token at the end of the line (see Tab completion). |
| `i`       | insert: a new filter before the cursor's filter when on a filter row, or a new output when on an output row (both fall through to "add" when on the `+ add` slot) |
| `<` / `>` | reorder: swap the cursor's filter with its left / right neighbor |
| `d` / `x` | drop the filter or output at the cursor |
| Esc       | back to ShedCursor |

### FilterEdit

| Key                  | Action |
|----------------------|--------|
| Tab / Shift-Tab      | next/prev field |
| ←→                   | cycle Select fields (Kind, Column, Op, Direction); on Columns multi-select moves cursor; on Pattern/RegexPattern/N: ignored |
| Type                 | edit text fields (Pattern, RegexPattern, N digits, Rename "to" inputs) |
| Backspace            | text fields: delete last char; sort-keys / where-clause: remove the active row |
| Space                | toggle Bool fields (CsvHasHeader); flip direction (SortDir); toggle column in Columns multi-select; toggle direction on a sort key |
| `a`                  | (on SortKeys / where-clause) append a new row |
| `x` / Backspace      | (on SortKeys / where-clause) remove the active row (min 1) |
| Enter                | apply — commits the in-progress filter to the pipeline |
| Esc                  | cancel — restores the saved filter, returns to ShedCursor |

### Palette (command palette)

Opened from any focus by **Ctrl-P**. A fuzzy-search list of every named
action shed supports — quit, focus newest shed, open env editor, pin /
unpin / expand / write / rerun the selected shed, open the filter
form, etc. Actions whose preconditions aren't met (e.g. "Pin shed"
when no shed is selected) are filtered out, so the list never offers
something it can't do.

| Key      | Action |
|----------|--------|
| (typing) | filter actions by case-insensitive word substring on the action name. Multiple words must all appear (in any order). |
| `↑` / `↓` | navigate filtered list |
| Enter    | run the selected action |
| Esc      | close the palette without running anything |
| Ctrl-D   | quit shed |

### EnvEdit (environment-variable editor)

Reachable from the **command palette** (Ctrl-P → "Open env editor").
A scrollable list of every
environment variable in shed's process, sorted by key, with edit /
add / delete affordances. Changes are visible to subsequent spawned
commands immediately.

| Key                 | Action |
|---------------------|--------|
| `↑` / `↓` / `j`/`k` | navigate |
| `/`                 | filter the list (case-insensitive substring on the key, live) |
| `e` / Enter         | edit the selected var's value (input bar pre-fills with current) |
| `a`                 | add a new var; type `KEY=VALUE`, Enter to commit |
| `d` / Delete        | unset the selected var |
| Esc                 | exit the input mode you're in (filter / edit / add); a second Esc / `q` leaves the editor |
| Ctrl-D              | quit shed |

### ShedExpand (pager)

Entered via `e` from ShedCursor. The selected shed's full pipeline
output fills the screen.

| Key                 | Action |
|---------------------|--------|
| `↑↓` or `j`/`k`     | scroll one line |
| PgUp / PgDn / Space / `b` / `f` | scroll one page (~20 lines) |
| Home / `g`          | jump to top |
| End / `G`           | jump to bottom |
| `/` / `?`           | start incremental search forward / backward (anchors at current scroll) |
| (typing in /-mode)  | search updates live; scroll jumps to first match at-or-after the anchor (forward) or last at-or-before (backward) |
| Enter               | commit the search (exit input mode; query stays for n/N) |
| Esc (in /-mode)     | cancel; revert scroll to anchor; clear query |
| `n` / `N`           | jump to next / previous match (wraps; always forward / backward regardless of how the search was initiated) |
| `i`                 | toggle case-insensitive matching (re-runs the active search) |
| Esc                 | clear active search; or, if no search, back to ShedCursor |
| `q`                 | back to ShedCursor (always) |
| Ctrl-D              | quit |

Search is **regex** (Rust `regex` crate). Default is case-sensitive; `i` toggles case-insensitivity (header shows `(i)` next to the query, achieved by prefixing the pattern with `(?i)`). The header shows `/<query>  (N matches)` while a query is active. Matched **substrings** within each line render with reversed video, leaving the rest of the line's ANSI styling intact. While typing, an invalid regex shows `(invalid regex)` next to the input bar but doesn't break typing — once it parses again, the search resumes.

## Architecture

```
shed/
├── Cargo.toml          workspace, edition 2024, resolver 3
├── crates/
│   ├── shed-core/      lib: data model + filter execution (no I/O, no UI)
│   │   ├── value.rs    Value enum
│   │   ├── capture.rs  Capture struct
│   │   ├── filter.rs   FilterSpec, Predicate, Filter trait, apply_with_notes
│   │   ├── shed.rs    Shed, ShedId, ShedState (incl. Idle for loaded notebooks)
│   │   ├── notebook.rs Notebook save/load (JSON, structure-only)
│   │   ├── aliases.rs  Cross-session named (argv, pipeline) saves
│   │   └── session.rs  Session with LRU eviction
│   └── shed/           bin: TUI + exec + ANSI
│       ├── main.rs     entry point
│       ├── tui.rs      ratatui TUI; focus model; filter form
│       ├── exec.rs     PTY-based command execution (portable-pty)
│       └── ansi.rs     vte-based ANSI → ratatui spans
└── README.md
```

### Key dependencies

| Crate          | What for |
|----------------|----------|
| `ratatui` + `crossterm` | TUI rendering and event loop |
| `portable-pty` | PTY-based capture so programs see a real terminal |
| `vte`          | ANSI escape parser for color rendering |
| `tokio`        | async runtime; tasks for concurrent commands |
| `shlex`        | argv tokenization with quote/escape handling |
| `csv`          | `from-csv` parser |
| `serde_json`   | `from-json` parser |
| `regex`        | `from-regex` and `where matches` |
| `bytes`        | zero-copy byte slices for capture buffers |
| `indexmap`     | ordered map for `Record` (preserves column order) |
| `serde` / `serde_json` | notebook persistence (JSON) and `from-json` parsing |
| `thiserror`    | error types |

### Design choices

- **No shell metacharacters.** shed does not interpret `|`, `>`, `>>`,
  `<`, `&&`, `||`, `;`, `$(...)`, or backticks. The retroactive pipeline
  replaces shell pipelines. If you need true shell semantics, use
  `bash -c '...'`.
- **Hard-fail on schema/regex errors; silent-drop on per-row data
  weirdness.** With an inline `ⓘ -N` annotation on the affected filter
  so the loss is visible.
- **Capture is bounded.** Default 16 MB per command; once full, the cap
  is held but the child keeps running (we drain the rest to /dev/null
  so the pipe doesn't shed).
- **PTY merges stdout/stderr.** That's an unavoidable PTY tradeoff. If
  separate streams matter for a workflow, `bash -c 'cmd 2>file'` is the
  workaround.
- **Filter set is fixed in v0.** No plugins. The set is small enough to
  surface in a single dropdown; richness comes from composition.

## Roadmap

Known gaps and likely next steps, in rough priority order:

- Predicate negation in the `where` form (the data model has
  `Predicate::Not`; the form has `≠` for compares but no negation
  for `matches`/`contains`)
- Multi-line cursor manipulation (cursor up / absolute position).
  Single-line cursor effects (`\r`, `\x1b[K`, `\t`) now collapse
  cargo-style progress bars; tools that update several lines via
  cursor up still produce stacked output.
- Saved/named pipelines as reusable computations
- Standalone note entries between sheds (today notes attach as
  pre / post text on a specific command shed — fine for commentary
  alongside a command but not for prose-only sections)

## License

MIT. See [LICENSE](LICENSE).
