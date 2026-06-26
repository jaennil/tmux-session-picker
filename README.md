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
| `Enter` | Switch session, toggle group, or open selected-session actions |
| `h` / `l` | Collapse / expand group |
| `n` | Create group |
| `Space` | Toggle selected session or all sessions in a group |
| `a` | Toggle all sessions in the current group |
| `A` | Toggle all visible sessions |
| `v` | Clear selected sessions |
| `m` | Move selected sessions, or the highlighted session, to a group |
| `r` | Rename group |
| `d` | Delete group; sessions become ungrouped |
| `p` | Pin / unpin selected sessions, or the highlighted session |
| `J` / `K` | Reorder group or pinned session |
| `x` | Kill selected sessions after confirmation, or kill highlighted session |
| `?` | Show shortcut help |
| `q` | Quit |
| Left click | Move the cursor to a session |
| Double-click | Switch session or toggle group |
| Left drag | Move a pinned session up or down |
| Right click | Pin / unpin the session under the cursor |
| Checkbox click | Toggle a session while selection mode is active |
| Mouse wheel | Scroll the session list |

Set `TMUX_SESSION_GROUP_FILE` to use another group state file. This is useful
for testing without changing your normal configuration.

Group headers are displayed but skipped by cursor navigation. The `Ungrouped`
header is hidden when it has no sessions.
