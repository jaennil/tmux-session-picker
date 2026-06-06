# Repository Guidelines

## Project Structure & Module Organization

This repository contains a small Rust 2024 terminal application for selecting tmux sessions. Application behavior and UI tests live in `src/main.rs`; group persistence and its tests live in `src/groups.rs`. `Cargo.toml` defines dependencies and release optimizations, while `Cargo.lock` pins versions. `README.md` documents user-facing controls. Cargo writes build artifacts to `target/`, which must remain untracked.

Keep terminal interaction, tmux command handling, session ordering, and rendering logic clearly separated within the source file. If the application grows, move cohesive areas into modules under `src/` and keep `main.rs` focused on startup and orchestration.

## Build, Test, and Development Commands

- `cargo build` compiles a debug binary for local development.
- `cargo run` launches the picker. Run it inside a tmux client with at least one active session.
- `cargo build --release` creates the optimized binary at `target/release/tmux-session-picker`.
- `cargo test` runs all inline unit tests.
- `cargo fmt --check` verifies standard Rust formatting; run `cargo fmt` to apply it.
- `cargo clippy --all-targets --all-features` reports common correctness and style issues.

The application accepts `TMUX_SOCKET_NAME` or `TMUX_SOCKET_PATH` for alternate tmux servers. Set `TMUX_SESSION_PIN_FILE` and `TMUX_SESSION_GROUP_FILE` to isolate state during manual testing.

## Coding Style & Naming Conventions

Use `rustfmt` defaults and four-space indentation. Follow Rust naming conventions: `snake_case` for functions and variables, `PascalCase` for structs, and `SCREAMING_SNAKE_CASE` for constants. Prefer small functions, explicit error propagation with `AppResult`, and standard-library APIs. Keep `unsafe` blocks narrowly scoped around required libc calls.

## Testing Guidelines

Place focused unit tests in the existing `#[cfg(test)]` module or beside newly extracted modules. Name tests after observable behavior, such as `session_name_search_is_case_insensitive`. Cover parsing, ordering, persistence, and boundary conditions without requiring a live tmux server. Manually verify interactive rendering and key handling inside tmux when UI behavior changes.

## Commit & Pull Request Guidelines

Use Conventional Commits, matching history such as `feat(tmux): add pinned session ordering` and `fix: render search prompt above sessions`. Keep commit subjects to exactly 50 characters and omit the body.

Pull requests should explain the behavior change, list verification commands, and note tmux or environment prerequisites. Link relevant issues. Include a terminal screenshot or recording for visible rendering changes.
