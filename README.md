# tmux-session-picker

Small terminal session picker for tmux.

Sessions can be organized into named, collapsible groups. Group membership and
collapse state are stored in `~/.config/tmux/session-groups.toml`.

## Build

```bash
cargo build --release
```

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move selection |
| `g` / `G` | Jump to first / last session |
| `/` | Search sessions and groups by name |
| `Backspace` | Remove the last search character |
| `Esc` | Clear search |
| `Enter` | Switch session or toggle group |
| `h` / `l` | Collapse / expand group |
| `n` | Create group |
| `m` | Move session to a group |
| `r` | Rename group |
| `d` | Delete group; sessions become ungrouped |
| `p` | Pin / unpin session |
| `J` / `K` | Reorder group or pinned session |
| `x` | Kill selected session |
| `q` | Quit |

Set `TMUX_SESSION_GROUP_FILE` to use another group state file. This is useful
for testing without changing your normal configuration.
