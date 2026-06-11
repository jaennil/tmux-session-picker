# tmux-session-picker

Small terminal session picker for tmux.

## Build

```bash
cargo build --release
```

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move selection |
| `g` / `G` | Jump to first / last session |
| `/` | Search sessions by name |
| `Backspace` | Remove the last search character |
| `Esc` | Clear search |
| `Enter` | Switch to selected session |
| `p` | Pin / unpin session |
| `J` / `K` | Reorder pinned session |
| `x` | Kill selected session |
| `q` | Quit |
| Left click | Switch to the session under the cursor |
