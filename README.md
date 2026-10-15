# hop

hop is a terminal UI for the SSH hosts in `~/.ssh/config`. It lists your hosts as a tree of groups with favorites pinned to the top, finds them with fuzzy search, shows which ones are reachable, and lets you add, edit, and delete them without opening the file. Enter connects by running your system `ssh`, so keys, agents, ProxyJump, ControlMaster, and Match blocks behave exactly as they do without hop. Your ssh config stays the only list of hosts. hop keeps only the extras ssh has no place for: groups, favorites, notes, and when you last connected.

hop runs on macOS and Linux.

## Install

With the shell installer from GitHub Releases:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mboisvertdupras/hop/releases/latest/download/hop-installer.sh | sh
```

The installer puts `hop` in `~/.local/bin` and adds that directory to your shell's `PATH` if needed.

With Homebrew:

```sh
brew install mboisvertdupras/tap/hop
```

### Build from source

You need Rust 1.88 or later.

```sh
git clone https://github.com/mboisvertdupras/hop
cd hop
cargo install --path . --locked
```

## Usage

Run `hop`. It reads `~/.ssh/config` and every file it includes, then probes each host in the background. Each row shows the latency in ms, `down`, or `via jump` for hosts behind a ProxyJump. Blocks whose names are all patterns, such as `Host *`, are hidden but still apply when ssh runs.

### Host list

| Key | Action |
| --- | --- |
| `j` `k` / Down Up | Move |
| `h` / Left | Collapse the group (on a host, collapse the group it is in) |
| `l` / Right | Expand the group |
| Enter | Connect to the host, or toggle the group |
| Tab | Switch between the tree and the recent view |
| `/` | Fuzzy search |
| `a` | Add a host |
| `e` | Edit the host |
| `d` | Delete the host, after a `y`/`n` prompt |
| `f` | Toggle favorite |
| `g` | Set the group (`a/b` for nested, blank to clear) |
| `r` | Reload the config and probe again |
| `?` | Show the keys (any key closes) |
| `q` | Quit |

### Search

| Key | Action |
| --- | --- |
| Any character, Backspace | Edit the query |
| Down Up / Ctrl-N Ctrl-P | Move |
| Enter | Connect |
| Esc | Return to the tree |

### Add and edit form

| Key | Action |
| --- | --- |
| Tab / Shift-Tab | Next or previous field |
| Enter | Next field (a newline in Other directives) |
| Ctrl-U | Clear the field |
| Ctrl-S | Save |
| Esc | Cancel |

## Where hop stores data

Groups, favorites, notes, and last-used times live in `$XDG_CONFIG_HOME/hop/meta.toml`, or `~/.config/hop/meta.toml` when `XDG_CONFIG_HOME` is unset. The file is keyed by host alias. Renaming a host in hop moves its entry. Entries for hosts that no longer exist are kept, so they come back if the host does.

## Back up your ssh config

hop edits `~/.ssh/config`, and any file it includes, in place. It does not write backups. A save rewrites only the edited Host block and leaves everything else in the file byte-identical, and it is refused if the block changed on disk since hop read it. Still, keep a copy before you first edit with hop:

```sh
cp ~/.ssh/config ~/.ssh/config.bak
```

If your config includes other files, copy those too, or keep `~/.ssh` under version control.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
