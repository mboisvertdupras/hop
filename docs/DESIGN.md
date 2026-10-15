# hop design

`Cargo.toml` pins the crates. `src/*.rs` started as holds every type and signature with `todo!()` bodies. The connect flow and the event loop in `src/main.rs` are written out, because their exact order is the design.

## Problem

`hop` edits a file it does not own. `~/.ssh/config` is the source of truth, it can fan out through `Include` globs into files hop may not be allowed to write, and the user expects every byte hop did not mean to touch to stay identical. The same file may also change on disk while hop is open, from an editor or a dotfiles sync. Around that sits a TUI that must never block on network probes and that must hand the real terminal to `ssh` and take it back cleanly.

Three things make the shape non-obvious.

- Round-trip fidelity. A parse-then-print design has to prove its printer reproduces every quirk (`Key=value`, tabs, odd casing, quotes, no final newline). That proof never ends.
- Staleness at block granularity. The spec refuses a save only when *the block* changed, so offsets from load time cannot be trusted at write time.
- Terminal ownership. Any second reader of stdin (an input thread, say) steals keystrokes from `ssh`.

There is no surrounding system. Phase A grounding was skipped as greenfield.

## Usage (caller's view)

### What the user reads

```text
hop                      open the list of hosts from ~/.ssh/config
  j k / arrows           move           h l      collapse / expand group
  Enter                  connect        /        fuzzy search (Esc returns to the tree)
  a e d                  add, edit, delete      f    favorite    g    set group
  Tab                    tree / recent  r        reload config and re-probe
  ?                      help           q        quit
In the form: Tab moves, Ctrl-S saves, Esc cancels.
hop keeps group, favorite, notes and last-used in ~/.config/hop/meta.toml.
It never writes a backup and never touches lines outside the host you edit.
```

### Call site 1: the reducer turns a submitted form into a save

```rust
// app.rs, Mode::Form arm, on FormAction::Submit
let sub = match form.submission() {
    Ok(sub) => sub,
    Err(field_error) => { form.error = Some(field_error); app.mode = Mode::Form(form); return None; }
};
match app.catalog.config.plan(&sub.change) {
    Ok(ssh) => { app.mode = Mode::Form(form); Some(Effect::Save { ssh, meta: sub.meta }) }
    Err(PlanError::ReadOnly(path)) => { /* error on the form, naming path */ None }
    Err(e) => { /* AliasTaken -> alias field, NotFound -> banner */ None }
}
```

The form stays open until the shell answers with `Event::Loaded { reason: Saved | Refused }`.

### Call site 2: the shell executes it

```rust
// main.rs, Shell::save, called from Shell::run on Effect::Save { ssh, meta }
if let ConfigEdit::Write(plan) = ssh {                           // ssh file first, then meta
    match ssh_config::commit(&plan) {
        Ok(()) => {}
        Err(CommitError::Stale) => return vec![self.load(LoadReason::Refused)],
        Err(CommitError::Io(e)) => return vec![Event::Failed(e.to_string())],
    }
}
let meta_failure = self.store.update(meta).err();
let mut events = vec![self.load(LoadReason::Saved)];             // reload even if meta failed
events.extend(meta_failure.map(|e| Event::Failed(self.meta_error(e))));
events
```

### Call site 3: a test proves "bytes elsewhere unchanged"

```rust
let dir = tempdir();
write(dir.join("config"), "Include conf.d/*\n\n# work\nHost web web.example.com\n  HostName=10.0.0.5\n\tUser deploy  \n\nHost *\n  ServerAliveInterval 30\n");
let set = ConfigSet::load(&dir.join("config"), dir.path())?;
let web = set.host(&alias("web")).unwrap();
let spec = HostSpec { alias: alias("web"), core: core_with(&web.core, CoreKey::HostName, "10.0.0.6"), extras: web.extras.clone() };
let ConfigEdit::Write(plan) = set.plan(&HostChange::Update { alias: alias("web"), spec })? else { panic!("expected a change") };
write(dir.join("config"), read(dir.join("config")).replace("ServerAliveInterval 30", "ServerAliveInterval 60")); // concurrent edit elsewhere
ssh_config::commit(&plan)?;
assert_eq!(read(dir.join("config")), "Include conf.d/*\n\n# work\nHost web web.example.com\n  HostName=10.0.0.6\n\tUser deploy  \n\nHost *\n  ServerAliveInterval 60\n");
```

The `=` separator, the tab, the trailing spaces, the shared Host line, and the concurrent edit all survive. The types below are derived from these three call sites.

## Shape

### Crates

| Need | Crate | Why this one |
| --- | --- | --- |
| TUI | `ratatui` 0.30 | The mainstream Rust TUI. It re-exports crossterm 0.29 as `ratatui::crossterm`, so there is one terminal crate and one version pin. |
| Form inputs | `ratatui-textarea` 0.9 | The ratatui org's successor to tui-textarea, which is stuck on ratatui 0.29. The multi-line "other directives" field is the reason; hand-rolling cursor movement is not worth it. |
| Fuzzy match | `nucleo-matcher` 0.3 | Helix's matcher. It scores only and spawns no threads, which fits a few hundred hosts. Full `nucleo` would add a worker pool hop does not need. |
| meta.toml | `serde` + `toml` 1.1 | hop owns this file outright, so plain serde is enough. `toml_edit` would only matter if users hand-edit it with comments. |
| Include globs | `glob` 0.3 | Small, standard, and it sorts results the way ssh's glob(3) does. |
| Atomic write | `tempfile` 3 | Same-directory temp file, then `persist` (rename). |
| Read-only check | `rustix` 1 (`fs`) | `access(2)` with W_OK, which also reports read-only mounts. It is already in tempfile's dependency tree, so it costs no extra compile. |
| Signals | `signal-hook` 0.4 | Keeps hop alive when Ctrl-C hits the shared process group during ssh's connection setup. |

Probes use std threads and `std::sync::mpsc`. No async runtime. The work is 16 blocking workers doing `ssh -G` and a TCP connect. Tokio would bring a runtime, async process spawning, and an async input stream, and would buy nothing at this scale. No `dirs` crate, because the spec says `$XDG_CONFIG_HOME` with `~/.config` on both platforms. No date-time crate, because `last_used` is Unix seconds. No `anyhow` or `thiserror`, because the few error enums are shown in the UI, never chained.

### Module map

One binary crate, flat, six files. Each file owns one body of knowledge, not one execution phase.

| File | Owns | Public surface |
| --- | --- | --- |
| `ssh_config.rs` | ssh config text. Lexing, Include and glob resolution, block attribution, host extraction, edit planning, splicing, rebase, atomic commit. | `Alias`, `CoreKey`, `Core`, `ValidCore`, `Extras`, `HostSpec`, `HostChange`, `HostEntry`, `ConfigSet::{load, hosts, host, location, is_writable, plan}`, `Plan`, `commit`, error enums |
| `meta.rs` | hop's store. Schema, merge-on-write, idempotent ops. | `GroupPath`, `UnixTime`, `Meta`, `MetaMap`, `MetaOp`, `apply`, `MetaStore::{new, load, update}`, `age` |
| `probe.rs` | Reachability. Worker pool, `ssh -G` parsing, TCP connect. | `Reach`, `Prober::{start, drain}` |
| `form.rs` | The add/edit form, the validation boundary for user input. | `Form`, `FieldId`, `FormAction`, `Submission` |
| `app.rs` | UI state machine, row derivation, search. Pure. | `App`, `Catalog`, `Mode`, `Event`, `Effect`, `update`, `rows`, `search` |
| `ui.rs` | Drawing. | `render` |
| `main.rs` | Terminal, event loop, effect execution, suspend and resume. | none |

The longest trace is key press to disk. `main.rs` reads the key, `app::update` plans through `ssh_config`, then `main.rs` calls `ssh_config::commit`. That is three files, per laziness-protocol.

### Data structures

The ssh config is **original text plus spans**. `ConfigFile { path, text, parsed, writable }`. `Parsed.lines` is a partition of `text`, and every line, directive key, directive value, Host name token, and block is a `Range<usize>` into `text`. Nothing is re-serialized. This is the load-bearing choice. An edit is a list of `(span, replacement)` splices. A byte outside every replaced span is the original byte by construction, so fidelity is a property of `splice`, not of a printer. `parse_partitions_text` and the identity law (below) are the only round-trip proofs needed.

A **block** is a byte range: its attached leading comments, the `Host` or `Match` line, and the body through the last non-blank line before the next block's attached comments. Lines before the first block are the preamble and belong to no block. A comment run directly above a header (no blank line between, indent no deeper than the header's) is attached to that header. This decides what a delete removes and what counts as "the block changed".

`HostEntry` is one row of ssh data. It has an `alias`, a `BlockRef { file, block }` (index into the set, so path and span are derived, never copied), `core: Core`, `extras: Extras`, and `siblings`. The spec's `source: PathBuf + span` is `ConfigSet::location(entry)`. `ConfigSet.hosts` is built once at load in ssh evaluation order (Includes inlined where they appear), one entry per concrete name, first definition winning.

`CoreKey` is the single list of the five form fields. Parsing, patching, and the form's field order all iterate `CoreKey::ALL`. A sixth key is one enum variant, and the compiler finds every match.

Core values are **raw strings, not typed**. `Core = ByCore<Option<String>>`, one slot per `CoreKey`, holds what the file says, unquoted. hop never consumes the port or the identity path itself (probing asks `ssh -G`), so typing them would only add a normalization step that breaks the identity law. `Port 022` would become `22` and the line would be rewritten on an untouched save. `ValidCore` is the form-side wrapper. It checks that Port parses as 1..=65535 and keeps the text verbatim, per type-system-discipline ("strengthen a type only where partiality appears").

`Extras` is the verbatim, de-indented text of every non-core line in the block body. That covers other directives, comments, blank lines, and a second `IdentityFile`. It is not the spec's `Vec<(key, value)>`, because comments inside a block have to survive and the form shows this text in its free-text area. Its constructor rejects `Host` and `Match` lines, since one would split the block on write.

`Alias` is a newtype. It is non-empty, has no whitespace, has none of `* ? ! ,`, and has no leading `-`. A Host token that fails `Alias::parse` is a pattern and is hidden. Together with `ssh -- <alias>`, the no-leading-dash rule means a hostile config can never inject an ssh option.

`Meta` and `MetaMap = BTreeMap<Alias, Meta>` are a plain serde mirror of the TOML. `MetaOp` is the only way to change it.

`App` holds `catalog`, `reach: HashMap<Alias, Reach>`, `view`, `collapsed: BTreeSet<GroupPath>`, `cursor: Option<RowKey>`, `mode`, and `status`. Visible rows are **never stored**. `rows(&App)` derives them from the catalog, view, collapsed set, and mode, and search results are recomputed from the query. The cursor is a `RowKey` (group path or alias), not an index, so a reload or a probe update cannot move it. A few hundred hosts make per-frame derivation free, and there is no cache to keep in sync, per model-the-domain.

### How the surgical edit works

`ConfigSet::plan` is pure. It turns a `HostChange` into `ConfigEdit::Write(Plan { path, op })` against the loaded text, or `ConfigEdit::Unchanged` when the ssh config needs no change.

**Update** builds the new block text with splices relative to the block, in `rewrite_block`.

- Rename replaces only the alias token's span on the Host line. `Host web web.example.com` becomes `Host www web.example.com`.
- For each `CoreKey`, the first line with that key is compared, unquoted, to the submitted value. If they are equal, the line is untouched. If the value changed, only the value span is replaced, so key casing, `=` or spaces, and indentation stay. If the field was cleared, the whole line is deleted. If the field is new, a line `{indent}{Keyword} {value}` is inserted after the last surviving core line (or after the Host line). `indent` comes from the block's first directive, then the file's first indented directive, then four spaces.
- Extras that equal the loaded extras are untouched. Edited extras replace all extra lines and are appended, re-indented, at the block end. The rule is that untouched text is byte-identical and edited text is normalized.
- Values containing whitespace are written in double quotes.

The **identity law** follows. `rewrite_block` with the entry's own spec returns the original block text. `plan` then returns `Ok(ConfigEdit::Unchanged)`. A meta-only save (group, notes) never touches the ssh file. That is also why a host in a read-only Include can still be grouped, favorited, and annotated. `ReadOnly` is raised only when a non-empty ssh change is needed.

**Delete** removes the alias token (plus one adjacent whitespace run) when the Host line has other tokens. It removes the whole block when the alias is the only token. Whole-block removal also eats the following blank lines when the block is preceded by a blank line or the start of the file, so deletions do not accumulate double gaps. Comments outside the block span are never removed.

**Add** renders a fresh block for the root file. The insertion point is chosen against the fresh text at commit time. It goes before the trailing run of catch-all blocks (pattern-only `Host` lines such as `Host *`, and `Match` blocks), else at EOF. ssh takes the first value it sees for each option, so a host appended after a trailing `Host *` would have that block's `User` or `Port` silently win. Separating blank lines are added where missing, plus a newline if the file lacked a final one. This deviates from the spec's literal "appended" (see Open questions).

### How stale detection works

A `Plan` names its block **by content, not offset**. `PlanOp::Rewrite { expected, ordinal, replacement }` and `PlanOp::Remove { expected, ordinal }` carry the exact bytes of the block as loaded. `ordinal` says which of several byte-identical blocks is meant (the n-th). `commit` is the IO shell around a pure `rebase(fresh, op)`.

1. Re-read the canonical path (symlinks resolved at load, so a dotfiles symlink is written through, not replaced).
2. `parse(fresh)`. Find the `ordinal`-th block whose bytes equal `expected`. If there is none, return `Err(Stale)`.
3. Splice the replacement (or the removal range, recomputed on the fresh text) into the fresh text.
4. Write a temp file in the same directory with the original file's mode, fsync, then rename over the target. No backup.

Edits made on disk to *other* blocks of the same file survive the save. The spec refuses only when the block itself changed, and this enforces exactly that. A whole-file mtime or hash check would refuse more often than the spec asks. Offsets would corrupt the file. The commit is a compare-and-swap, per make-operations-idempotent. Running it a second time finds the block already rewritten and refuses with `Stale`, so it never applies twice. A crash before the rename leaves the original file intact, plus at worst an orphan temp file.

On `Stale`, the shell reloads and sends `Loaded { reason: Refused }`. The list data is fresh. An open form keeps the user's draft and shows "This host changed on disk. Review and save again." Saving again plans against the fresh file.

### Meta store writes merge

`MetaStore::update(ops)` re-reads `meta.toml`, applies each `MetaOp` with the pure `meta::apply`, writes atomically, and returns the map now on disk. The app replaces its copy with that return value (`Event::MetaSaved`). The UI therefore never shows a state that was not persisted, and two hop instances merge instead of clobbering. Orphan entries (no matching host) are never read for display and never dropped on write, because the write is the file plus ops, not hop's view. Every op is idempotent. `SetFavorite(alias, bool)` exists and a toggle does not, so the `f` key sends the inverse of the value on screen. `Touch` keeps the later timestamp, so concurrent touches commute. `Rename` is a no-op when the source is already gone. Per separate-before-serializing-shared-state, the shared file is unavoidable, so writes are structured as merges rather than locked.

Save ordering is ssh config first, then meta. A crash between them leaves meta under the old alias, which is an orphan and harmless. The reverse order could move metadata for a rename that was then refused.

### How probe results reach the UI

```text
Shell::load ──> Prober::start(aliases) ──> queue ──> 16 workers
                                                      │ ssh -G -- alias   (killed after 3s)
                                                      │ parse_ssh_g -> Direct{host,port} | ViaJump
                                                      │ connect_timeout(1.5s) per resolved addr
                                                      ▼
main loop: event::poll(50ms) ── key? ──> Event::Key
           prober.drain()  (try_iter, non-blocking) ──> Event::Probe(alias, reach)
           app::update  ──> reach.insert(alias, reach)        (main thread is the only writer)
```

Workers share nothing with the UI. They send messages, and the main thread is the sole writer of `reach`. Cancellation is structural. Refresh replaces `shell.prober`. The old `Prober`'s `Drop` sets its cancel flag, so workers stop taking jobs, and its receiver is gone, so in-flight results fail to send and disappear. No generation counter exists to get wrong.

`ViaJump` comes from `ssh -G`'s effective `proxyjump` *or* `proxycommand`. A `Host *.internal` block that sets ProxyJump marks every matching host. That is the spec's "effective" rule applied to jumps as well as addresses.

Why a 50 ms poll and not an input thread? A crossterm reader thread would still be blocked in `read()` when ssh starts and would eat ssh's keystrokes. Pausing it needs a handshake that races. With the main thread as the only terminal reader, the bug cannot happen. 50 ms of probe latency and 20 empty wakeups per second cost nothing visible. The loop redraws only when an event arrived (`dirty`).

### State machine

`update(&mut App, Event) -> Option<Effect>` is the whole UI. The modes are `List`, `Search { query }`, `Form(Form)`, `Confirm { prompt, on_yes: Effect }`, `SetGroup { alias, input }`, and `Help`. `SetGroup` is an extra mode for the `g` key. It is a one-line prompt, which is lighter than opening the full form.

| Mode | Input | Next mode | Effect |
| --- | --- | --- | --- |
| List | j k, arrows | List (cursor moves, skips section rows) | none |
| List | h l, left right | List (group collapsed or expanded) | none |
| List | Enter on host | List | `Connect(alias)` |
| List | Enter on group | List (toggle) | none |
| List | `/` | Search (empty query) | none |
| List | `a` | Form(Add) | none |
| List | `e` on host | Form(Edit) | none |
| List | `d` on host | Confirm (plan already computed) or List with status on `ReadOnly` | none |
| List | `f` on host | List | `Meta([SetFavorite(alias, !shown)])` |
| List | `g` on host | SetGroup (prefilled) | none |
| List | `r` | List | `Reload` |
| List | Tab | List (Tree and Recent swap) | none |
| List | `?` | Help | none |
| List | `q` | none | `Quit` |
| Search | text, Backspace | Search (cursor to top result) | none |
| Search | Up Down, Ctrl-n Ctrl-p | Search | none |
| Search | Enter | List (tree, cursor on alias) | `Connect(alias)` |
| Search | Esc | List (tree, cursor on alias, its groups expanded) | none |
| Form | Tab, text, Enter | Form | none |
| Form | Ctrl-S, invalid | Form (field error) | none |
| Form | Ctrl-S, valid | Form (awaits Loaded) | `Save { ssh, meta }` |
| Form | Esc | List | none |
| Confirm | `y` | List | the stored `on_yes` |
| Confirm | `n`, Esc | List | none |
| SetGroup | Enter | List | `Meta([SetGroup(alias, group)])` |
| SetGroup | Esc | List | none |
| Help | any key | List | none |
| any | `Loaded{Saved}` | Form or Confirm become List, cursor on the target alias | none |
| any | `Loaded{Refused}` | Form stays with draft and banner, others become List with status | none |
| any | `Loaded{Refresh}`, `Probe`, `MetaSaved`, `Failed` | unchanged | none |

`Confirm` stores the exact `Effect` it will emit, computed before asking. What the prompt describes ("Remove `web` from `Host web web.example.com` in ~/.ssh/conf.d/work:12?") is what runs. Each `Mode::Key` arm takes the mode by `mem::replace` and writes back its successor, so every transition names its next state.

### Suspend, run ssh, resume

The code is in `main.rs::connect`, in this order.

1. `ratatui::restore()` leaves the alternate screen, turns raw mode off, and shows the cursor.
2. `Command::new("ssh").arg("--").arg(alias).status()` inherits stdin, stdout, and stderr. The main thread blocks here, and no other thread reads the terminal.
3. On a non-zero exit, a signal, or a spawn error such as ssh missing, hop prints `hop: ssh exited with status 255. Press any key to return to hop.`. It then reads one key in raw mode, after discarding keys typed while ssh exited. The failure text stays on the normal screen until that key. `failure_message` is a pure function.
4. Raw mode back on, enter the alternate screen, `terminal.clear()` so ratatui's back buffer matches the blank screen.
5. The shell sends `MetaOp::Touch(alias, now)`. `Event::MetaSaved` updates the in-memory map and the next draw shows the list.

hop installs a flag handler for SIGINT and SIGQUIT at startup. Ctrl-C during ssh's connection phase (host-key prompt, hung connect) signals the whole foreground process group. With a handler installed hop survives. Because `exec` resets caught signals to their defaults, ssh still dies normally. `SIG_IGN` would have been inherited by ssh, so it is wrong here. In raw mode Ctrl-C is a key, so the handler never matters while the TUI is up.

### What the system deliberately does not do

- It does not interpret ssh semantics. Probing and jump detection ask `ssh -G`.
- It never edits `Match` blocks, and it never edits the preamble except for the root file's new-host insertion.
- It keeps no backup and no lock file.
- It never re-serializes a file.

### Interface depth

`ssh_config` exposes `load`, `hosts`, `host`, `location`, `is_writable`, `plan`, and `commit`, plus value types. Behind that surface sit lexing, Include and glob resolution with cycle guards, comment attribution, the edit splicer, the insertion policy, content-addressed rebase, symlink-preserving atomic writes, and mode preservation. Callers never see a span, a line, or a file id. `meta` exposes `load`, `update(ops)`, and `apply`. The merge-on-write policy is not optional for callers, because there is no "save map" call to misuse. `probe` exposes `start` and `drain`. Pool size, timeouts, `ssh -G`, and cancellation are hidden. `app::update` is one function with two enums.

## Test plan

Everything above the shell is a pure function on strings or structs. The test list sits in each file's `tests` module. The groups are below.

**Pure, table or fixture tests.** These are the bulk.
- `ssh_config::parse`. Partition property over all fixtures. Block attribution (attached versus separated comments, indented trailing comments, Match ending a block, preamble).
- `rewrite_block`. The identity law over every host in every fixture. One case per bullet in "How the surgical edit works", each asserting the **whole output file** so unchanged bytes are checked, not assumed.
- `remove_name`, `removal_range`, `insertion_point` (trailing `Host *`, trailing Match, empty file, no final newline), `render_value`.
- `rebase`. Change elsewhere survives, changed block refuses, identical blocks resolved by ordinal.
- `meta::apply`. Idempotence table over every `MetaOp` (`apply` twice equals once). Touch commutes. Rename overwrites an orphan. A default entry is removed.
- `probe::parse_ssh_g`, `meta::age`, `main::failure_message`, `GroupPath::parse`, `Alias::parse`, `ValidCore::parse`, `Extras::parse`.
- `app::rows`. Favorites pinned once, collapse hides the subtree, nested groups from paths, recent order, orphan meta ignored.
- `app::search`. Ranking behavior (an alias hit beats a notes hit), not score values.
- `app::update`. Table-driven `(mode, key sequence) -> (mode tag, effects)`, one row per transition-table row, plus `Loaded{Refused}` keeping the form draft and probe events never moving the cursor.

**Tempdir integration.** Real files, no terminal.
- `ConfigSet::load` with relative and globbed Includes, an Include cycle, an unmatched glob, first definition winning across files, and a `chmod 0444` file reported read-only.
- `commit` writing through a symlink and keeping mode 0600. A concurrent edit to another block survives. A stale block is refused with the file unchanged.
- `MetaStore` with orphans surviving, two stores on one path merging, and a corrupt file refused rather than overwritten.
- `probe` with a fake `ssh` script first on `PATH` printing `hostname 127.0.0.1` and the port of a test listener. That gives Up. A closed port gives Down. `proxyjump bastion` gives ViaJump.

**Manual, driven through the `run` skill in a real terminal.** These cannot be unit-tested without a pty harness that costs more than it earns in v1.
- Connect to a reachable host, exit, and check the list redraws with "now" as last used.
- Connect to an unreachable host and check the exit 255 text stays until a key.
- Ctrl-C during a hung connect returns to hop.
- Typing during the ssh session never reaches hop.
- Resize while suspended, then return.

## Synthesis decision

Two candidates were sketched independently on different models. Both arrived at the same core: ssh config files kept as raw text with byte-span splices as the only writer, content-addressed stale detection, a pure `update` reducer driving effects, a single terminal reader polled on the main thread, plain threads for probes, and read-merge-write for `meta.toml`. That convergence is the strongest evidence the shape is right.

This candidate was chosen as the base over the other for four reasons:

- It inserts new hosts before a trailing `Host *` or `Match` run. Appending after them would let `Host *` silently override the new host's User and Port.
- An ordinal disambiguates byte-identical blocks during stale detection. The other candidate refused the save in that case.
- Cancelling probes by dropping the channel needs no generation counter.
- It uses fewer crates: no `jiff`, `anyhow`, or separately pinned `crossterm`.

The other candidate wrote files in place with `fs::write` to keep symlinked configs working. This design keeps that property by resolving symlinks at load and renaming atomically into the real file, so nothing was grafted.

## Tradeoffs accepted

- We accept keeping every file's full text in memory in exchange for fidelity by construction and a stale check that needs no hashes. ssh configs are kilobytes.
- We accept that edited extras are normalized (re-indented, moved to the block end) in exchange for a simple rule. Untouched extras stay byte-identical, edited ones are rewritten.
- We accept a microsecond race between the fresh read and the rename in `commit` in exchange for no lock file. A lock would only coordinate hop with hop, not with an editor.
- We accept that rename-into-place needs write permission on the directory and replaces the inode, which breaks hard links. In exchange a crash never leaves a half-written ssh config. `is_writable` checks the directory too, so the UI agrees with what commit can do.
- We accept a 50 ms poll loop in exchange for having a single terminal reader. This makes the stdin-stealing bug impossible.
- We accept spawning `ssh -G` per host on every load (16 at a time) in exchange for exact ssh semantics, including Match and wildcard defaults. A `Match exec` in the user's config runs once per probe. That is what ssh itself would do.
- We accept that every reload re-probes every host and briefly shows them as probing, in exchange for one load path (startup, refresh, and after-save are the same code).
- We accept that core values stay untyped strings in exchange for the identity law holding on any input the file contains.
- We accept that hop's form puts new core lines after existing ones rather than in a canonical order. That is the price of never touching lines the user did not edit.

## Alternatives considered

- **Concrete syntax tree with a printer** (each node stores its whitespace and separators, printing reproduces the file). This loses because it hides less behind a larger surface. Every node type carries trivia fields, every edit must construct trivia, and fidelity depends on the printer covering every quirk forever. Span-splicing reaches the same guarantee with `splice` as the only writer.
- **Offset plus file hash for staleness** (store mtime or a hash at load, refuse if it differs). This is simpler, but it refuses saves the spec allows (another block changed) and gives offsets no way to rebase. Content-addressed blocks give both precision and rebase for about the same code.
- **Tokio with an async event stream** (`crossterm::EventStream`, `tokio::process`, `select!`). This loses on depth, because the runtime, futures, and cancellation tokens all reach the shell, and it still needs a plan for stdin ownership during ssh. Threads plus channel-drop cancellation hide more behind `start` and `drain`.
- **Dedicated input thread with a pause flag.** It loses because pausing a thread blocked in `read()` races with ssh's first keystrokes. Single-reader polling removes the race instead of managing it.
- **Optimistic meta updates in the reducer** (apply ops locally, persist in the background). This loses to the read-merge-write that returns the on-disk map. With optimistic updates, two writers could show state that was never saved.

## Implementation reconciliation

Where the code differs from the sketch or the text above, and why.

- Crates. The table above now names the pinned releases: ratatui 0.30, ratatui-textarea 0.9, toml 1.1, signal-hook 0.4. crossterm 0.29 comes only through `ratatui::crossterm`. crossterm still pulls signal-hook 0.3, so two signal-hook versions are in the tree.
- `Alias` and `GroupPath` implement serde with `#[serde(try_from = "String", into = "String")]`. A meta.toml key that is a pattern, or a group with an empty segment, is therefore `MetaError::Corrupt`, and the file is not overwritten.
- `Reach::Unknown` is removed. Every load resets every alias to `Probing`, so nothing ever constructed it. An alias missing from the reach map is the unknown state.
- `Block` gained `first`, the line index of its first attached comment (or of the header). `removal_range` needs it to see whether a blank line precedes the block, and it no longer takes `text`.
- A core keyword with no value (`User` alone on a line) is classified as an extra line, not a core line. Otherwise the form shows it blank, submits `None`, and an untouched save deletes the line, which breaks the identity law.
- Edited extras are de-indented line by line before re-indenting. "Normalized" in "How the surgical edit works" now includes the user's own indentation in the text area.
- `ssh_config::write_atomic` is `pub` and `MetaStore::update` uses it too, so there is one temp-file-and-rename implementation. It keeps an existing file's mode, and creates a missing parent directory with 0700 and a new file with 0600. That covers both the missing `~/.ssh` decision and `~/.config/hop`.
- `MetaStore::update` writes only when the ops changed the map. A meta-only save that changes nothing, or the first run, does not create meta.toml.
- `ConfigSet::host_line(entry)` was added. The delete prompt quotes the Host line as written ("Delete web from `Host web web.example.com` in path:12?"), and spans stay private to `ssh_config`.
- Include files that canonicalize to a directory or a dangling path are skipped, as ssh skips them. A read error on an included file aborts the load with a `LoadError` that names the file.
- Search ranks in two tiers. Hosts whose alias matches come first, then hosts that match only in hostname, user, group or notes. Each tier is ordered by score, then alias. With the single haystack, the query `db` ranked a host with notes "db backups" above the host `db`. Test `search_ranks_alias_hit_over_notes_hit` covers this.
- `Mode::SetGroup { input }` is a `String`, like `Search { query }`, not a `TextArea`. A `TextArea` made `Mode` 648 bytes (clippy `large_enum_variant`), and a one-line prompt needs only push and pop.
- `h` on a host inside a group collapses that group and moves the cursor onto it. Without this, the cursor would sit on a hidden row. More generally, after every event a host cursor hidden by a collapsed group expands its groups. This covers a favorite just unpinned, and Esc or Enter out of search. A cursor key that no longer exists falls back to the first row.
- Leaving search with Esc or Enter also returns the view to Tree when it was Recent ("Esc restores the tree").
- List and confirm commands ignore Ctrl and Alt chords. The shell forwards only `KeyEventKind::Press` events.
- `Prober::start` delegates to a private `Prober::with_ssh(program, aliases)`. Tests run a fake `ssh` script without touching `PATH`, because `std::env::set_var` is `unsafe` in edition 2024. The sketch said cancellation needs no test. One exists anyway, because it catches a real defect: workers that ignore the dropped receiver ran all 64 queued `ssh -G` jobs instead of at most 16.
- `Shell::read() -> Result<Catalog, String>` was split from `Shell::load(reason)`. `main` prints the first load failure to stderr and exits 1 before entering the TUI.
- `AliasError`, `GroupError`, `PlanError` and `MetaError` implement `Display`. Status lines and banners show sentences, not `{:?}` output.

## Decisions on open questions

These are reversible defaults set before implementation.

- New hosts are inserted before a trailing `Host *` or `Match` run.
- A refused save keeps the form and the draft open, with an error line.
- Tab toggles between the grouped tree and the Recent view.
- When an alias is defined twice, the first definition wins. No "also defined" note in v1.
- `ProxyCommand` hosts count as `via jump`.
- A trailing `# comment` after a value is part of the value.
- `Include` inside a `Host` or `Match` block is followed unconditionally.
- Adding a host when `~/.ssh` is missing creates it with mode 0700 and the file with 0600.

## Open questions and risks

- New hosts go before a trailing `Host *` or `Match` run, not literally at the end of the file. Is that deviation acceptable? A literal append silently lets `Host *` override the new host's User and Port.
- Should a refused save keep the form open with the user's draft (this design) or close it and return to the list as the spec literally says? Keeping it avoids losing typed input.
- What key opens the Recent view? This design uses Tab, which the key table leaves free.
- When an alias is defined in two blocks, the first wins and later ones are not shown. Should the detail pane say "also defined in file:line"?
- `ProxyCommand` hosts are shown as `via jump` and not probed. Is that the intended reading of "Hosts with a ProxyJump"?
- Trailing `# comments` after a value are treated as part of the value (hop does not interpret them). Editing that field replaces the comment. Is that acceptable for v1?
- `Include` inside a `Host` or `Match` block is conditional in ssh. hop lists the included hosts unconditionally. Is that correct for a browser?
- Risk: `ssh -G` evaluates `Match exec` commands on every probe. A config with a slow `Match exec` makes probes slow, though never the UI.
- Risk: a missing `~/.ssh` directory. Add has to create it with mode 0700 and the file with 0600. That is easy to forget. It needs a tempdir test.

## Next implementation step

Implement `ssh_config::parse` and `splice` with the partition property test and the identity-law test over a fixture set. Every other feature stands on them.
