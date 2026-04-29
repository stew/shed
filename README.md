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
| `!cmd`    | force fullscreen handover (typed prefix) |
| Esc       | focus newest block |
| Ctrl-D    | quit |
| Ctrl-C    | quit (no running selection) |
| Ctrl-K    | open the command palette |

### BlockCursor

| Key       | Action |
|-----------|--------|
| `↑↓`      | navigate between blocks |
| `←→`      | navigate filters within the selected block (and the `+ add` slot) |
| `f` / Enter | edit selected filter / add new |
| `i`       | insert a new filter before the cursor's filter (or add at end if on the `+ add` slot) |
| `<` / `>` | reorder: swap the cursor's filter with its left / right neighbor |
| `d`       | drop the filter at cursor (or last if on add slot) |
| `e`       | expand the selected block to a fullscreen pager |
| `w`       | write the block's filtered output to a file path you type. The output format is inferred from the path extension: `.csv` → comma-separated, `.tsv` → tab-separated, `.json` → pretty JSON, anything else → plain text. |
| `p`       | pin the selected block under a name (input bar pre-fills with the existing name if any). Pinned blocks render with `◉ name` and never evict on capture-budget pressure. Empty name unpins. |
| `u`       | unpin the selected block (clear its name) |
| `r`       | open a rerun input bar pre-filled with the block's argv (shlex-quoted). Edit and Enter to spawn a new block with the edited command and the same pipeline copied over; Esc cancels. The original block is unchanged. |
| Ctrl-C    | cancel a running command (kills the child) |
| Esc       | back to prompt |
| Ctrl-D    | quit |

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

- Predicate negation in the `where` form (the data model has
  `Predicate::Not`; the form has `≠` for compares but no negation
  for `matches`/`contains`)
- Multi-line cursor manipulation (cursor up / absolute position).
  Single-line cursor effects (`\r`, `\x1b[K`, `\t`) now collapse
  cargo-style progress bars; tools that update several lines via
  cursor up still produce stacked output.
- Saved/named pipelines as reusable computations
- Scrollback within long block previews (sub-block scroll without
  entering the pager)

## License

MIT. See [LICENSE](LICENSE).
