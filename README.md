# shed

> An interactive shell where the pipeline comes after the command.

shed is a TUI shell (Linux/macOS) that captures every command's output as a
structured **block** and lets you build pipelines **retroactively** — adding,
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
   Enter run · !cmd fullscreen · Esc focus block · Ctrl-D quit
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

1. Type a command — `ls -la /etc` — and press **Enter**. Block `%1` appears
   with colored output (shed runs commands in a PTY so terminal-aware
   programs emit color).
2. Press **Esc** to focus the newest block. The prompt at the bottom
   changes to `(block %1 selected)`. The block highlights in cyan.
3. Press **f** to open the filter form. You're now in *FilterEdit* focus:
   the screen splits into preview / pipeline stack / form.
4. The form's first field is the filter type. Use **←→** to cycle:
   `from-lines`, `from-fields`, `from-csv`, `from-json`, `from-regex`,
   `where`, `select`, `drop`, `take`, `skip`, `sort-by`, `uniq`, `count`,
   `rename`. Pick `from-fields` and press **Enter**.
5. The block now shows columns `_1` through `_9` and the inline pipeline
   reads `from-fields`.
6. Press **f** again to add another filter. The form defaults to `where`
   because the schema is non-empty. **Tab** through fields:
   `column` → cycle to `_5`; `op` → cycle to `>`; `value` → type `1000`.
   Watch the preview shrink as you type.
7. Press **Enter** to apply; **Esc** to return to the prompt.

To quit: `Ctrl-D`.

## Concepts

### Blocks

Each command spawns a *block* with:

- captured stdout (PTY merges stderr into stdout; cap defaults to 16 MB)
- exit code
- a retroactive pipeline of filters
- a monotonic id (`%1`, `%2`, …) and an optional pinned name (`◉ name`)

Blocks live in a session-wide store. Unnamed blocks evict on memory
pressure (LRU); pinned blocks count toward the budget but never evict.
Pin a block with `p` from BlockCursor — it renders with `◉ <name>`
next to its command. `u` unpins.

### Pipelines

A pipeline is an ordered list of filters applied to a block's captured
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

Each block renders as a bordered box; the box title carries the
identifier (`%5` or `@name` for pinned blocks) plus a state glyph. A
single "scratch" box at the bottom is the place to type a new command —
it's always present and takes the next id.

- **BlockCursor** — navigate blocks (`↑↓`); the scratch sits one slot
  past the last real block. Compact view: id/name + glyph + output. To
  reveal the command and the pipeline, press `e` to enter EditBlock.
  Press `v` to open the fullscreen pager. Press `/` to jump to the
  prompt and start typing.
- **EditBlock** — pipeline-edit mode for the cursor block. The argv is
  shown on the first line and each filter on its own indented line;
  `←→` navigates between the command and the filters; `f`/Enter opens
  the form editor for the active slot; `i` inserts, `d` drops, `<>`
  reorder. `Esc` returns to BlockCursor.
- **Prompt** — typing in the scratch box. `Enter` runs (and the new
  block appears in place of the scratch; a fresh scratch is drawn for
  the next command). `Esc` returns to BlockCursor on the last block.
  `↑↓` walk persistent history.
- **FilterEdit** — schema-aware form for the active filter, with live
  preview at the top.

### Concurrency

Commands run in tokio tasks; the TUI never freezes. Multiple commands can
run side-by-side. Each block carries a status glyph:

| glyph | meaning |
|------:|---------|
| `⏵`   | running |
| `●`   | done, replayable |
| `⚠`   | failed (or non-zero exit) |
| `✂`   | output truncated at the cap |
| `❄`   | snapshotted from a stream (planned) |
| `◉`   | pinned with a name (planned UI) |

### PTY-based capture

shed spawns commands attached to a pseudo-terminal so terminal-aware
programs (`ls --color`, `cargo build`, `git status`, …) emit colored
output. The captured bytes include ANSI escape sequences; the renderer
parses SGR (color/style) sequences via `vte` and emits ratatui-styled
spans. Cursor-positioning sequences are dropped, so non-curses programs
render cleanly. Programs that try to take over the screen get
**fullscreen handover** instead.

### Builtins

A few commands aren't external programs and are handled inside shed
itself:

- **`cd [path]`** — change shed's current working directory. Subsequent
  commands inherit the new cwd. Without an argument, `cd` goes to
  `$HOME`. `cd -` swaps with the previous cwd. `~` and `~/path` are
  expanded. The cwd shows in the header bar (`shed  ·  ~/devel/shed`).
- **`exit`** / **`quit`** — quit shed (same as Ctrl-D).
- **`export KEY=VALUE [KEY=VALUE ...]`** — set environment variables in
  shed's process; subsequent spawned commands inherit them. `export`
  with no args, or a bare key without `=`, is rejected.
- **`unset NAME [NAME ...]`** — remove environment variables.

Most other "shell-isms" (`|`, `>`, `&&`, `$(…)`) are deliberately not
supported — see the design constraints below. Use `bash -c '…'` if you
need real shell semantics.

### Pinned references (`@name`)

A pinned block (`p` to pin) gets a name. Type `@name` at the prompt and
shed creates a new block whose capture is a *snapshot* of `@name`'s
current pipeline output:

- structured output (rows from a parser) is rendered as pretty JSON, so
  the snapshot block typically starts with `from-json`
- raw bytes are passed through unchanged

The snapshot is taken at create time. It does *not* re-evaluate when the
source changes — to refresh, focus the snapshot block and press `Space`
(re-run in place re-snapshots from the source's current state). If the
source is gone or has no capture, the snapshot block lands in `Failed`
state with a message saying so.

**Dependency auto-run.** If you run a snapshot block (`Space` or typing
`@name`) and the source itself hasn't run yet, shed walks the chain and
runs the prereqs first, in dep order. A flash message tells you what's
about to happen (`running 2 deps first, then %5`). If a prereq fails, the
rest of the chain is dropped rather than running against a stale source.
Cycles (`@a` pinned as `a`) terminate via cycle-detection.

`@name` blocks save and load through notebooks like any other block (the
argv is just `@name`); on re-open they're `Idle` and re-snapshot when
you run them.

### Block notes (pre / post text)

Each block can carry two free-form text notes — `pre_text` shown above
the block, `post_text` shown below. Use them for section headers,
running commentary, observations about the output, or anything else you
want to preserve alongside the command.

| Key | Action |
|-----|--------|
| `n` | edit the pre-note (above the block) |
| `N` | edit the post-note (below the block) |

In the editor: type to insert at the cursor, `Enter` for newline,
`Backspace` / `Delete`, `←→ ↑↓ Home End` to navigate, `Ctrl-S` to save,
`Esc` (or `Ctrl-C`) to cancel. An empty buffer clears the note.

Notes render dimmed and italicized with a `▎` left edge so they're
visually distinct from command output. They round-trip through
notebooks alongside argv, name, and pipeline.

### Aliases

A *global* alias is a saved `(argv, pipeline)` pair addressed by name.
Type the name at the prompt and shed materialises a fresh block with
that argv and pipeline pre-filled, then drops you into in-place command
edit so you can append args before running:

```
list           ↵    →   block %N appears with argv `ls -lat`,
                         cmd-edit bar shows  `ls -lat ` (cursor at end)
list /etc      ✗         (alias lookup is single-token only)
```

Aliases shadow real binaries — an alias named `ls` wins over `/usr/bin/ls`.
Use `bash -c 'ls'` if you need to bypass.

| How                      | Action |
|--------------------------|--------|
| `A` (in BlockCursor)     | save the cursor block as an alias (input bar `alias name:`). If the name already exists, a `[y]es / [n]o` prompt appears before overwriting. |
| type `<name>` at prompt  | invoke alias — creates an Idle block, opens cmd-edit pre-filled with `argv ` (trailing space) so you can append args + Enter. |
| `/aliases` at prompt     | open the manage view: `↑↓` navigate, `x` (or `d`) delete, `Esc` / `q` back. |

Storage is `$XDG_CONFIG_HOME/shed/aliases.json` (fallback
`~/.config/shed/aliases.json`), versioned JSON, written on every change.
Aliases are cross-session and not tied to any single notebook.

Edit by re-invoking: type the alias, modify the argv / pipeline as
needed, then `A` again with the same name and confirm overwrite.

### Undo / redo

`Ctrl-Z` and `Ctrl-Y` step backwards and forwards through every
*structural* change you've made: adding a block, deleting a block,
pinning / unpinning, editing the command, adding / dropping / reordering
filters, editing pre / post notes, saving a block as an alias. The
history is in-memory only (not persisted across sessions) and capped at
50 entries.

Captures and run-state are preserved across an undo/redo round-trip:
adding a filter, undoing, and redoing won't reset your block to Idle —
the output you've already produced stays put. The one exception is
undoing the *creation* of a block: the block disappears (and so does
its capture) until you redo. Resurrecting a deleted block via undo
brings back its argv / pipeline / notes, but its capture comes back as
whatever the snapshot held (no fresh run is triggered).

Running a command (Space, Enter, re-snapshot of `@name`) is *not*
undoable — re-runs are runtime, not structural, and don't dirty the
notebook. Undo restores notebook structure; if you want to revert a
re-run's output, save before re-running and reload.

### Tab completion

Available at the Prompt and inside the in-place command editor (the bar
that appears after `e` → `f` on a block's command). Tab cycles forward
through matches; Shift-Tab cycles backward. There's no popup list — the
input itself is rewritten to the next candidate. Any non-Tab keystroke
ends the cycle.

The token under the cursor (everything after the last whitespace)
decides what to complete:

| Token shape | Source |
|-------------|--------|
| `$FOO`      | environment variable names |
| `@name`     | pinned block names |
| `/cmd` (Prompt only, first token) | slash commands (`/aliases`) |
| first token, otherwise | commands on `$PATH` ∪ saved aliases ∪ shell builtins (`cd`, `export`, `unset`, `exit`) |
| second token onward, or any path-shaped token (`./`, `../`, `/...`, `~/`) | filesystem paths (directories get a trailing `/`; hidden entries appear only when the prefix starts with `.`) |

If no candidates match, the flash bar shows "no completions" and the
input is left untouched.

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
| Run a loaded block     | move the cursor onto it (`Esc`, `↑↓`) and press `Space` (or `x`). Loaded blocks start in `Idle` state with a hollow `○` glyph; running them swaps the capture in place. |
| Quit with unsaved work | `Ctrl-D` shows `unsaved changes — save before quitting? [y]es [n]o [c]ancel` instead of quitting straight away |

Block re-runs and pipeline edits set a *dirty* flag. Save clears it;
quitting while dirty triggers the confirmation prompt. `Ctrl-S` from
anywhere (Prompt, BlockCursor, even mid-FilterEdit) writes the current
session out.

### Fullscreen handover

For interactive programs (`top`, `vim`, `less`, `man`, `tmux`, `ssh`,
`tig`, `ranger`, `fzf`, …), shed temporarily yields the entire terminal:
tears down its TUI, runs the child with inherited stdio, restores. The
block records the exit code with no captured output (`(no captured
output)`).

Detection has three paths, in order:
- **Built-in blacklist** of common interactive programs — handover
  before spawn.
- **Explicit `!` prefix** — type `!cmd` to force handover for anything
  not on the blacklist.
- **Auto-detect at runtime** — the PTY reader watches each chunk for
  alt-screen-enter sequences (`\x1b[?1049h` and the older
  `\x1b[?47h` / `\x1b[?1047h`). If it sees one mid-capture, it kills
  the child, signals the TUI, and the same block is retried with
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
| Tab / Shift-Tab | cycle through completions for the token at the end of the line (see Tab completion). |
| `!cmd`    | force fullscreen handover (typed prefix) |
| `@name`   | snapshot the output of pinned block `@name` into a new block (see Pinned references) |
| Esc       | focus newest block |
| Ctrl-D    | quit (or prompt if unsaved) |
| Ctrl-C    | quit (no running selection) |
| Ctrl-K    | open the command palette |
| Ctrl-S    | save notebook (input bar if no path bound) |
| Ctrl-O    | open notebook (replaces the current session) |
| Ctrl-Z / Ctrl-Y | undo / redo the most recent structural change (see Undo / redo) |

### BlockCursor

The compact, default focus. Each block renders as a bordered box with
just `id/name + glyph + output`; the command and pipeline stay hidden
until you press `e` to enter EditBlock. Pressing `↓` past the last real
block lands on the scratch box (focus shifts to Prompt).

| Key       | Action |
|-----------|--------|
| `↑↓`      | navigate between blocks; `↓` past the last block jumps to the scratch (Prompt). |
| `e`       | enter EditBlock — reveals the command and each filter on its own line so you can navigate / edit them. |
| `v`       | view the selected block in the fullscreen pager. |
| `/`       | jump to the scratch (Prompt) with `/` typed for slash commands like `/aliases`. |
| Space     | run the selected block in place (re-spawns its argv, replaces the capture). Use this to execute Idle blocks loaded from a notebook, or to re-run a finished block without typing it again. For `@name` snapshot blocks this re-snapshots from the source. |
| `x`       | delete the selected block from the session (kills it if running). Cursor advances to the next sibling, or returns to the prompt if the session becomes empty. |
| `w`       | write the block's filtered output to a file path you type. The output format is inferred from the path extension: `.csv` → comma-separated, `.tsv` → tab-separated, `.json` → pretty JSON, anything else → plain text. |
| `p`       | pin the selected block under a name (input bar pre-fills with the existing name if any). Pinned blocks render their box title as `@name` and never evict on capture-budget pressure. Empty name unpins. |
| `u`       | unpin the selected block (clear its name) |
| `r`       | open a rerun input bar pre-filled with the block's argv (shlex-quoted). Edit and Enter to spawn a new block with the edited command and the same pipeline copied over; Esc cancels. The original block is unchanged. |
| `n` / `N` | edit the block's pre-note / post-note (multi-line text rendered above / below the block, persisted to the notebook) |
| `A`       | save the block as a global alias (input bar; overwrites prompt for confirmation) |
| Ctrl-C    | cancel a running command (kills the child) |
| Ctrl-S    | save notebook |
| Ctrl-O    | open notebook |
| Esc       | back to prompt |
| Ctrl-D    | quit (or prompt if unsaved) |

### EditBlock

Entered by pressing `e` on a block. The block's command appears on the
first line; each filter on its own indented `│ filter` row below it.
Block-level actions (run, delete, pin, etc.) live one focus up — `Esc`
returns to BlockCursor for those.

| Key       | Action |
|-----------|--------|
| `↑↓`      | navigate vertically through the command, each filter, and the `+ add` slot. `↑` at the first filter steps onto the command (highlighted in magenta); `↓` from the command returns to the filter list. `←→` are aliases for muscle memory. |
| `f` / Enter | edit the active slot — opens the filter form for a filter, or the in-place command editor when the command is focused. Committing a command edit re-runs the block; if the block is pinned, every block whose argv is `@<name>` (and theirs, recursively) is queued to re-run too. |
| Tab / Shift-Tab | (in-place command editor only) cycle through completions for the token at the end of the line (see Tab completion). |
| `i`       | insert a new filter before the cursor's filter (or add at end if on the `+ add` slot) |
| `<` / `>` | reorder: swap the cursor's filter with its left / right neighbor |
| `d`       | drop the filter at cursor (or last if on add slot) |
| Esc       | back to BlockCursor |

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
| Esc                  | cancel — restores the saved filter, returns to BlockCursor |

### Palette (command palette)

Opened from any focus by **Ctrl-K**. A fuzzy-search list of every named
action shed supports — quit, focus newest block, open env editor, pin /
unpin / expand / write / rerun the selected block, open the filter
form, etc. Actions whose preconditions aren't met (e.g. "Pin block"
when no block is selected) are filtered out, so the list never offers
something it can't do.

| Key      | Action |
|----------|--------|
| (typing) | filter actions by case-insensitive word substring on the action name. Multiple words must all appear (in any order). |
| `↑` / `↓` | navigate filtered list |
| Enter    | run the selected action |
| Esc      | close the palette without running anything |
| Ctrl-D   | quit shed |

### EnvEdit (environment-variable editor)

Triggered by `Ctrl-E` from the prompt. A scrollable list of every
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

### BlockExpand (pager)

Entered via `e` from BlockCursor. The selected block's full pipeline
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
| Esc                 | clear active search; or, if no search, back to BlockCursor |
| `q`                 | back to BlockCursor (always) |
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
│   │   ├── block.rs    Block, BlockId, BlockState (incl. Idle for loaded notebooks)
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
  so the pipe doesn't block).
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
- Standalone note entries between blocks (today notes attach as
  pre / post text on a specific command block — fine for commentary
  alongside a command but not for prose-only sections)
- Scrollback within long block previews (sub-block scroll without
  entering the pager)

## License

MIT. See [LICENSE](LICENSE).
