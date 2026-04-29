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
cargo run        # opens the TUI
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

shed has three focus contexts (no vim-style modes — the meaning of each
key changes with focus, and the bottom status bar lists what's
available):

- **Prompt** — type commands. `Enter` runs.
- **BlockCursor** — navigate blocks (`↑↓`) and filters within a block
  (`←→`); add/edit/drop filters; cancel running commands.
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

### Fullscreen handover

For interactive programs (`top`, `vim`, `less`, `man`, `tmux`, `ssh`,
`tig`, `ranger`, `fzf`, …), shed temporarily yields the entire terminal:
tears down its TUI, runs the child with inherited stdio, restores. The
block records the exit code with no captured output (`(no captured
output)`).

Detection is by built-in blacklist of common interactive programs. To
force handover for anything not on the list, prefix the command with
`!` — e.g., `!cargo run` if cargo's child wants the TTY.

## Filter reference

| Filter        | Class       | Params                    | Notes |
|--------------:|-------------|---------------------------|-------|
| `from-lines`  | parser      | (none)                    | one record per line, column `line` |
| `from-fields` | parser      | (none)                    | whitespace split; columns `_1`, `_2`, … (max width across all rows) |
| `from-csv`    | parser      | `delim`, `has_header`     | flexible — mixed-field rows survive |
| `from-json`   | parser      | (none)                    | array of objects → rows; object → 1 row; scalar → wrapped in `value` column |
| `from-regex`  | parser      | `pattern`                 | named captures become columns; non-matching lines are dropped |
| `where`       | row filter  | `column`, `op`, `value`   | ops: `matches` (regex), `contains` (substring), `=`, `≠`, `<`, `≤`, `>`, `≥`. Numeric coercion when both sides parse. Lenient per-row (drops rows on null/type mismatch); hard-fail on bad regex / unknown column. |
| `select`      | columns     | `columns`                 | keep listed columns in given order |
| `drop`        | columns     | `columns`                 | remove listed columns |
| `rename`      | columns     | pairs                     | each form row is one column → new name (blank means unchanged) |
| `take`        | rows        | `n`                       | first N rows |
| `skip`        | rows        | `n`                       | drop first N rows |
| `sort-by`     | rows        | `keys` (up to 5)          | each key is (column, asc/desc); numeric coercion when both sides parse |
| `uniq`        | rows        | `by` (optional)           | dedupe by all columns by default; if `by` set, dedupe keyed by those columns |
| `count`       | aggregation | (none)                    | single row `{count: N}` |

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
| `!cmd`    | force fullscreen handover (typed prefix) |
| Esc       | focus newest block |
| Ctrl-D    | quit |
| Ctrl-C    | quit (no running selection) |

### BlockCursor

| Key       | Action |
|-----------|--------|
| `↑↓`      | navigate between blocks |
| `←→`      | navigate filters within the selected block (and the `+ add` slot) |
| `f` / Enter | edit selected filter / add new |
| `d`       | drop the filter at cursor (or last if on add slot) |
| Ctrl-C    | cancel a running command (kills the child) |
| Esc       | back to prompt |
| Ctrl-D    | quit |

### FilterEdit

| Key                  | Action |
|----------------------|--------|
| Tab / Shift-Tab      | next/prev field |
| ←→                   | cycle Select fields (Kind, Column, Op, Direction); on Columns multi-select moves cursor; on Pattern/RegexPattern/N: ignored |
| Type                 | edit text fields (Pattern, RegexPattern, N digits, Rename "to" inputs) |
| Backspace            | text fields: delete last char; sort-keys: remove the active key |
| Space                | toggle Bool fields (CsvHasHeader); flip direction (SortDir); toggle column in Columns multi-select; toggle direction on a sort key |
| `a`                  | (on SortKeys) append a new sort key |
| `x` / Backspace      | (on SortKeys) remove the active sort key (min 1) |
| Enter                | apply — commits the in-progress filter to the pipeline |
| Esc                  | cancel — restores the saved filter, returns to BlockCursor |

## Architecture

```
shed/
├── Cargo.toml          workspace, edition 2024, resolver 3
├── crates/
│   ├── shed-core/      lib: data model + filter execution (no I/O, no UI)
│   │   ├── value.rs    Value enum
│   │   ├── capture.rs  Capture struct
│   │   ├── filter.rs   FilterSpec, Predicate, Filter trait, apply_with_notes
│   │   ├── block.rs    Block, BlockId, BlockState
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

- And/Or composition in `where` predicates (data model supports it; UI
  does not)
- Alt-screen auto-handover — detect `\x1b[?1049h` mid-capture and
  switch to handover mode
- vt100 emulation for cursor-positioning programs (cargo progress
  bars, etc.) — captured output currently shows the
  flattened-and-stacked version
- Insert filter at a specific index (currently you can append or
  replace, not insert in the middle)
- Reorder existing pipeline filters (Alt-↑/↓)
- Prompt history (Up arrow recall of previous commands)
- Command palette (universal `Space` / `Ctrl-K` action menu)
- Saved/named pipelines as reusable computations
- Block expand-to-fullscreen for inspecting long captures
- Scrollback within long block previews

## License

MIT. See [LICENSE](LICENSE).
