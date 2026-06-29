# tmux-session-picker

Small terminal session picker for tmux.

Sessions are shown in two side-by-side views: `Active` on the left and `All` on
the right. Use `Active` for sessions currently in use; `All` always shows every
session. The picker starts focused on `Active`. Sessions can also be organized
into named, collapsible groups. Group membership and collapse state are stored in
`~/.config/tmux/session-groups.toml`.

## Build

```bash
cargo build --release
```

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move selection |
| `Ctrl+h` / `Ctrl+l` | Switch focus between Active and All views |
| `Ctrl+j` / `Ctrl+k` | Move between group headers |
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
| `p` | Add selected/highlighted sessions to Active from All, or remove from Active |
| `J` / `K` | Reorder group or active session |
| `x` | Kill selected sessions after confirmation, or kill highlighted session |
| `?` | Show shortcut help |
| `q` | Quit |
| Left click | Move the cursor to a session |
| Double-click | Switch session or toggle group |
| Left drag | Move an active session up or down |
| Right click | Add/remove the session under the cursor from Active |
| Checkbox click | Toggle a session while selection mode is active |
| Mouse wheel | Scroll the session list |

Set `TMUX_SESSION_GROUP_FILE` to use another group state file. This is useful
for testing without changing your normal configuration.

Active sessions are stored in the existing `TMUX_SESSION_PIN_FILE` state file
for compatibility with older versions.

Group headers are displayed but skipped by normal cursor navigation. Use
`Ctrl+j` / `Ctrl+k` to select group headers explicitly. The `Ungrouped` header
is hidden when it has no sessions. Empty groups are hidden in the Active view.
