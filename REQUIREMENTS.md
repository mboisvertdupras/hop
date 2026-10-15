# hop: product requirements (v1)

`hop` is a terminal UI for browsing, organizing, editing, and connecting to the SSH hosts defined in `~/.ssh/config`. It is written in Rust and targets macOS and Linux.

## Principles

- `~/.ssh/config` is the single source of truth for hosts. `hop` never keeps its own list of connections.
- `hop` stores only metadata that ssh config has no place for. Each entry is keyed by host alias.
- Connecting runs the system `ssh`. Keys, agents, ProxyJump, ControlMaster, and Match blocks behave exactly as they do without `hop`.

## Data shape

```text
Host (parsed from ssh config, never persisted by hop)
  alias          String            first non-wildcard name on the Host line
  source         PathBuf + span    the file and byte range of the Host block
  hostname       Option<String>
  user           Option<String>
  port           Option<u16>
  identity_file  Option<PathBuf>
  proxy_jump     Option<String>
  extras         Vec<(key, value)> every other directive in the block, kept in order

Meta (hop's store at $XDG_CONFIG_HOME/hop/meta.toml, keyed by alias)
  group          Option<GroupPath>  e.g. "clients/acme"
  favorite       bool
  last_used      Option<timestamp>
  notes          Option<String>

Reachability (in memory only)
  Unknown | Probing | Up { latency } | Down | ViaJump
```

The list view joins `Host` with `Meta` by alias. A `Meta` entry with no matching host is ignored on read and kept on write. A host can reappear after the user fixes their config, so its metadata must survive.

## Features

### Host list

- Hosts come from `~/.ssh/config`, with `Include` directives resolved recursively. Glob paths are supported.
- Blocks whose names are all patterns (`Host *`, `Host *.prod`) are hidden. They still apply when ssh runs.
- A `Host` line with several concrete names (`Host web web.example.com`) shows one entry per concrete name. The entries share one block.
- Hosts appear as a collapsible tree of groups. Ungrouped hosts sit at the root.
- Favorites are pinned to the top. A "recent" view sorts by `last_used`.

### Search

- `/` opens fuzzy search over alias, hostname, user, group, and notes.
- Results are flattened and ranked while a query is active. Esc restores the tree.

### Connect

- Enter suspends the TUI, restores the terminal, and runs `ssh <alias>`, inheriting stdin, stdout, and stderr.
- When ssh exits, `hop` records `last_used`, redraws, and returns to the list.
- `ssh` exit codes and errors stay visible until a key is pressed, so a failed connection is never wiped by the redraw.

### Add, edit, delete

- The form fields are alias, HostName, User, Port, IdentityFile, ProxyJump, and a free-text area for other directives. Group and notes are part of the same form and are saved to the meta store.
- Saving rewrites only the edited Host block, in the file it came from. Comments, blank lines, ordering, and indentation elsewhere stay byte-identical. No backup file is written.
- New hosts are appended to `~/.ssh/config`.
- Renaming an alias moves its metadata to the new alias.
- Deleting a host removes its Host block (or only its name, when the block lists several) and its metadata, after a confirmation prompt.
- Before writing, `hop` re-reads the file. If the block changed since it was loaded, the save is refused and the list reloads.

### Reachability

- On load and on refresh (`r`), every host is probed in the background with a TCP connect to its effective HostName and Port. A short timeout keeps the probes cheap.
- The effective address comes from `ssh -G <alias>`, so wildcard blocks and defaults are applied the same way ssh applies them.
- Hosts with a ProxyJump show `via jump` and are not probed directly.
- Each row shows up, down, or latency in ms. The UI never blocks on probes.

### Keys

| Key | Action |
| --- | --- |
| `j` `k` / arrows | Move |
| `h` `l` / left right | Collapse or expand group |
| `/` | Search |
| Enter | Connect |
| `a` `e` `d` | Add, edit, delete |
| `f` | Toggle favorite |
| `g` | Set group |
| `r` | Refresh config and probes |
| `?` | Help |
| `q` | Quit |

## Out of scope for v1

- An embedded terminal or SSH library. `hop` always delegates to `ssh`.
- Port forwards and tunnels.
- File transfer.
- Native Windows.
- Backups of ssh config.

## Open questions

- Whether `Match` blocks should be shown or edited. Assumed no: they are left untouched and only influence `ssh -G` results.
- Whether a host defined in an included file that `hop` cannot write (permissions, read-only mount) should be shown as read-only. Assumed yes.
