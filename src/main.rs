use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod groups;

use groups::GroupState;

type AppResult<T> = Result<T, Box<dyn Error>>;
const MOUSE_BUTTON_MASK: u16 = 0b11;
const MOUSE_DRAG_FLAG: u16 = 0b10_0000;
const MOUSE_WHEEL_FLAG: u16 = 0b100_0000;
const MOUSE_WHEEL_ROWS: isize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Session {
    name: String,
    last_activity: u64,
    pinned: bool,
    is_current: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisibleRow {
    Group(usize),
    Session(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MouseEvent {
    button: u16,
    col: usize,
    row: usize,
    pressed: bool,
}

enum InputEvent {
    Key(u8),
    Mouse(MouseEvent),
    Ignore,
}

fn parse_sgr_mouse_body(input: &[u8]) -> Option<MouseEvent> {
    if !input.starts_with(b"[<") {
        return None;
    }
    let (&terminator, body) = input[2..].split_last()?;
    if terminator != b'M' && terminator != b'm' {
        return None;
    }

    let body = std::str::from_utf8(body).ok()?;
    let mut parts = body.split(';');
    let button = parts.next()?.parse().ok()?;
    let col = parts.next()?.parse().ok()?;
    let row = parts.next()?.parse().ok()?;
    if parts.next().is_some() || col == 0 || row == 0 {
        return None;
    }

    Some(MouseEvent {
        button,
        col,
        row,
        pressed: terminator == b'M',
    })
}

fn parse_x10_mouse_body(input: &[u8]) -> Option<MouseEvent> {
    if input.len() != 5 || !input.starts_with(b"[M") {
        return None;
    }

    let button = u16::from(input[2]).checked_sub(32)?;
    let col = usize::from(input[3]).checked_sub(32)?;
    let row = usize::from(input[4]).checked_sub(32)?;
    if col == 0 || row == 0 {
        return None;
    }

    Some(MouseEvent {
        button,
        col,
        row,
        pressed: button & 0b11 != 0b11,
    })
}

fn parse_mouse_escape(input: &[u8]) -> Option<MouseEvent> {
    if !input.starts_with(b"\x1b") {
        return None;
    }
    let body = &input[1..];
    parse_sgr_mouse_body(body).or_else(|| parse_x10_mouse_body(body))
}

fn mouse_wheel_delta(button: u16) -> Option<isize> {
    if button & MOUSE_WHEEL_FLAG == 0 {
        return None;
    }

    match button & MOUSE_BUTTON_MASK {
        0 => Some(-MOUSE_WHEEL_ROWS),
        1 => Some(MOUSE_WHEEL_ROWS),
        _ => None,
    }
}

fn mouse_plain_button(button: u16) -> Option<u16> {
    (button & (MOUSE_DRAG_FLAG | MOUSE_WHEEL_FLAG) == 0).then_some(button & MOUSE_BUTTON_MASK)
}

fn mouse_drag_button(button: u16) -> Option<u16> {
    (button & MOUSE_DRAG_FLAG != 0 && button & MOUSE_WHEEL_FLAG == 0)
        .then_some(button & MOUSE_BUTTON_MASK)
}

fn visible_index_for_mouse_row(
    mouse_row: usize,
    list_row_start: usize,
    list_height: usize,
    top: usize,
    row_count: usize,
) -> Option<usize> {
    let offset = mouse_row.checked_sub(list_row_start)?;
    if offset >= list_height {
        return None;
    }
    let index = top.checked_add(offset)?;
    (index < row_count).then_some(index)
}

fn input_is_ready(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result > 0 && poll_fd.revents & libc::POLLIN != 0)
}

fn read_fd_byte(fd: RawFd) -> io::Result<u8> {
    let mut byte = 0_u8;
    loop {
        let result = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        if result == 1 {
            return Ok(byte);
        }
        if result == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }

        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn read_input_event(fd: RawFd) -> io::Result<InputEvent> {
    let byte = read_fd_byte(fd)?;
    if byte != 0x1b {
        return Ok(InputEvent::Key(byte));
    }

    let mut sequence = vec![byte];
    while sequence.len() < 32 && input_is_ready(fd, 80)? {
        let byte = read_fd_byte(fd)?;
        sequence.push(byte);
        if sequence.starts_with(b"\x1b[M") && sequence.len() == 6 {
            break;
        }
        if sequence.starts_with(b"\x1b[<") && (byte == b'M' || byte == b'm') {
            break;
        }
    }

    if sequence.len() > 1 && parse_mouse_escape(&sequence).is_none() {
        return Ok(InputEvent::Ignore);
    }

    Ok(parse_mouse_escape(&sequence)
        .map(InputEvent::Mouse)
        .unwrap_or(InputEvent::Key(0x1b)))
}

fn build_visible_rows(
    sessions: &[Session],
    groups: &GroupState,
    query: &str,
    ungrouped_collapsed: bool,
) -> Vec<VisibleRow> {
    let searching = !query.is_empty();
    let mut rows = Vec::new();

    for (group_index, group) in groups.groups.iter().enumerate() {
        let group_matches = session_name_matches(&group.name, query);
        let matching_sessions = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| groups.group_for_session(&session.name) == Some(group_index))
            .filter(|(_, session)| group_matches || session_name_matches(&session.name, query))
            .map(|(session_index, _)| session_index)
            .collect::<Vec<_>>();

        if !searching || group_matches || !matching_sessions.is_empty() {
            rows.push(VisibleRow::Group(group_index));
            if searching || !group.collapsed {
                rows.extend(matching_sessions.into_iter().map(VisibleRow::Session));
            }
        }
    }

    let ungrouped_index = groups.groups.len();
    let ungrouped_matches = session_name_matches(groups::UNGROUPED_NAME, query);
    let matching_sessions = sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| groups.group_for_session(&session.name).is_none())
        .filter(|(_, session)| ungrouped_matches || session_name_matches(&session.name, query))
        .map(|(session_index, _)| session_index)
        .collect::<Vec<_>>();

    if !searching || ungrouped_matches || !matching_sessions.is_empty() {
        rows.push(VisibleRow::Group(ungrouped_index));
        if searching || !ungrouped_collapsed {
            rows.extend(matching_sessions.into_iter().map(VisibleRow::Session));
        }
    }

    rows
}

fn first_session_row_position(rows: &[VisibleRow]) -> usize {
    rows.iter()
        .position(|row| matches!(row, VisibleRow::Session(_)))
        .unwrap_or(0)
}

fn session_indices_for_group(
    sessions: &[Session],
    groups: &GroupState,
    group_index: usize,
) -> Vec<usize> {
    sessions
        .iter()
        .enumerate()
        .filter_map(|(session_index, session)| {
            let session_group = groups.group_for_session(&session.name);
            let target_group = (group_index < groups.groups.len()).then_some(group_index);
            (session_group == target_group).then_some(session_index)
        })
        .collect()
}

fn selected_count_for_group(
    sessions: &[Session],
    groups: &GroupState,
    selected_sessions: &BTreeSet<String>,
    group_index: usize,
) -> (usize, usize) {
    let group_sessions = session_indices_for_group(sessions, groups, group_index);
    let selected = group_sessions
        .iter()
        .filter(|index| selected_sessions.contains(&sessions[**index].name))
        .count();
    (selected, group_sessions.len())
}

fn toggle_selection_for_session(selected_sessions: &mut BTreeSet<String>, session_name: &str) {
    if !selected_sessions.remove(session_name) {
        selected_sessions.insert(session_name.to_string());
    }
}

fn toggle_selection_for_group(
    selected_sessions: &mut BTreeSet<String>,
    sessions: &[Session],
    groups: &GroupState,
    group_index: usize,
) {
    let group_sessions = session_indices_for_group(sessions, groups, group_index);
    let all_selected = group_sessions
        .iter()
        .all(|index| selected_sessions.contains(&sessions[*index].name));

    for index in group_sessions {
        if all_selected {
            selected_sessions.remove(&sessions[index].name);
        } else {
            selected_sessions.insert(sessions[index].name.clone());
        }
    }
}

fn toggle_selection_for_rows(
    selected_sessions: &mut BTreeSet<String>,
    sessions: &[Session],
    rows: &[VisibleRow],
) {
    let session_indices = rows
        .iter()
        .filter_map(|row| match row {
            VisibleRow::Session(index) => Some(*index),
            VisibleRow::Group(_) => None,
        })
        .collect::<Vec<_>>();
    let all_selected = session_indices
        .iter()
        .all(|index| selected_sessions.contains(&sessions[*index].name));

    for index in session_indices {
        if all_selected {
            selected_sessions.remove(&sessions[index].name);
        } else {
            selected_sessions.insert(sessions[index].name.clone());
        }
    }
}

fn prune_selected_sessions(selected_sessions: &mut BTreeSet<String>, sessions: &[Session]) {
    selected_sessions.retain(|name| sessions.iter().any(|session| session.name == *name));
}

fn bulk_pin_target_state(sessions: &[Session], selected_sessions: &BTreeSet<String>) -> bool {
    sessions
        .iter()
        .any(|session| selected_sessions.contains(&session.name) && !session.pinned)
}

const SHORTCUTS: &[(&str, &str)] = &[
    ("j/k", "move cursor"),
    ("g/G", "jump first/last"),
    ("/", "search sessions and groups"),
    ("Backspace", "delete search character"),
    ("Esc", "clear search, cancel prompt, or quit"),
    ("Enter", "switch session, toggle group, or act on selection"),
    ("h/l", "collapse or expand group"),
    ("n", "create group"),
    ("Space", "toggle selected session or group"),
    ("a", "toggle current group selection"),
    ("A", "toggle visible session selection"),
    ("v", "clear selected sessions"),
    ("m", "move selected or highlighted sessions"),
    ("r", "rename group"),
    ("d", "delete group"),
    ("p", "pin or unpin selected sessions"),
    ("J/K", "reorder group or pinned session"),
    ("x", "kill selected sessions"),
    ("?", "show shortcuts"),
    ("Mouse", "click select; double-click activate"),
    ("Right click", "pin or unpin session"),
    ("Drag", "move pinned session up or down"),
    ("Wheel", "scroll sessions"),
    ("q", "quit"),
];

fn help_popup_lines(index: usize, max_entries: usize) -> Vec<String> {
    let max_entries = max_entries.max(1);
    let half = max_entries / 2;
    let mut start = index.saturating_sub(half);
    if start + max_entries > SHORTCUTS.len() {
        start = SHORTCUTS.len().saturating_sub(max_entries);
    }
    let end = (start + max_entries).min(SHORTCUTS.len());

    let mut lines = Vec::with_capacity(end.saturating_sub(start) + 3);
    lines.push("Shortcuts".to_string());
    for (shortcut_index, (key, action)) in SHORTCUTS[start..end].iter().enumerate() {
        let shortcut_index = start + shortcut_index;
        let marker = if shortcut_index == index { ">" } else { " " };
        lines.push(format!("{marker} {key:<10} {action}"));
    }
    lines.push(format!(
        "{}/{}  j/k scroll  g/G first/last  Esc close",
        index + 1,
        SHORTCUTS.len()
    ));
    lines
}

fn help_popup_height(rows: usize) -> usize {
    rows.saturating_sub(2).max(5)
}

fn move_popup_lines(groups: &GroupState, choice: usize, session_count: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(groups.groups.len() + 4);
    lines.push(format!("Move {session_count} sessions to"));
    for (index, group) in groups.groups.iter().enumerate() {
        let marker = if index == choice { ">" } else { " " };
        lines.push(format!("{marker} {}", group.name));
    }

    let ungrouped_index = groups.groups.len();
    let marker = if choice == ungrouped_index { ">" } else { " " };
    lines.push(format!("{marker} {}", groups::UNGROUPED_NAME));

    let new_group_index = groups.groups.len() + 1;
    let marker = if choice == new_group_index { ">" } else { " " };
    lines.push(format!("{marker} New group..."));
    lines.push("j/k choose  Enter confirm  Esc cancel".to_string());
    lines
}

fn mode_line(
    prompt: Option<&Prompt>,
    searching: bool,
    query: &str,
    has_matches: bool,
    selected_count: usize,
) -> String {
    match prompt {
        Some(Prompt::Name { label, value, .. }) => {
            format!("MODE input: {label}: {value}_")
        }
        Some(Prompt::Move { session_names, .. }) => {
            format!(
                "MODE move: {} sessions  j/k choose  Enter confirm",
                session_names.len()
            )
        }
        Some(Prompt::ConfirmKill { session_names, .. }) => {
            format!("MODE confirm kill: {} sessions  y/N", session_names.len())
        }
        Some(Prompt::Action { .. }) => {
            format!("MODE selected actions: {selected_count} sessions")
        }
        Some(Prompt::Help { .. }) => "MODE help: j/k scroll  Esc close".to_string(),
        None if searching => {
            let suffix = if has_matches { "" } else { "  no matches" };
            format!("MODE search: /{query}{suffix}")
        }
        None if selected_count > 0 => {
            format!("MODE selection: {selected_count} sessions  m move  p pin  x kill  v clear")
        }
        None => "MODE normal: j/k move  Space select  / search  ? help".to_string(),
    }
}

fn session_row_line(
    pointer: &str,
    session: &Session,
    selected_sessions: &BTreeSet<String>,
    name_width: usize,
    activity_width: usize,
) -> String {
    let pin = if session.pinned { "!" } else { " " };
    let current = if session.is_current { "*" } else { "" };
    let last = format_relative_activity(session.last_activity);

    if selected_sessions.is_empty() {
        return format!(
            "{pointer}   {:<name_width$}  {:>activity_width$}  {:^3} {pin}",
            session.name, last, current,
        );
    }

    let checkbox = if selected_sessions.contains(&session.name) {
        "[x]"
    } else {
        "[ ]"
    };
    format!(
        "{pointer} {checkbox} {:<name_width$}  {:>activity_width$}  {:^3} {pin}",
        session.name, last, current,
    )
}

fn next_help_index(current: usize, offset: isize) -> usize {
    current
        .checked_add_signed(offset)
        .unwrap_or(0)
        .min(SHORTCUTS.len().saturating_sub(1))
}

struct App {
    sessions: Vec<Session>,
    groups: GroupState,
    selected_sessions: BTreeSet<String>,
    selected: usize,
    top: usize,
    rows: usize,
    cols: usize,
    pin_file: PathBuf,
    group_file: PathBuf,
    tmux_socket_name: Option<String>,
    tmux_socket_path: Option<String>,
    status: String,
    query: String,
    searching: bool,
    ungrouped_collapsed: bool,
    prompt: Option<Prompt>,
    last_click: Option<(VisibleRow, Instant)>,
}

#[derive(Clone)]
enum NameAction {
    Create,
    Rename(usize),
    CreateAndMove(Vec<String>),
}

enum Prompt {
    Name {
        label: &'static str,
        value: String,
        action: NameAction,
    },
    Move {
        session_names: Vec<String>,
        choice: usize,
    },
    ConfirmKill {
        session_names: Vec<String>,
        skipped_current: usize,
    },
    Action {
        choice: usize,
    },
    Help {
        index: usize,
    },
}

struct Layout {
    table_col: usize,
    name_width: usize,
    activity_width: usize,
    list_row_start: usize,
    blank_row: usize,
    status_row: usize,
}

impl App {
    fn new() -> AppResult<Self> {
        let tmux_socket_name = env::var("TMUX_SOCKET_NAME").ok();
        let tmux_socket_path = env::var("TMUX_SOCKET_PATH").ok();
        let pin_file = pin_file_path();
        let group_file = group_file_path();
        let (rows, cols) = terminal_size().unwrap_or((24, 80));
        let sessions = load_sessions(&pin_file, &tmux_socket_name, &tmux_socket_path)?;
        let groups =
            GroupState::load(&group_file).map_err(|err| format!("failed to load groups: {err}"))?;
        let current_session = sessions
            .iter()
            .position(|session| session.is_current)
            .unwrap_or(0);
        let visible_rows = build_visible_rows(&sessions, &groups, "", false);
        let selected = visible_rows
            .iter()
            .position(|row| *row == VisibleRow::Session(current_session))
            .unwrap_or(0);

        Ok(Self {
            sessions,
            groups,
            selected_sessions: BTreeSet::new(),
            selected,
            top: 0,
            rows,
            cols,
            pin_file,
            group_file,
            tmux_socket_name,
            tmux_socket_path,
            status: String::new(),
            query: String::new(),
            searching: false,
            ungrouped_collapsed: false,
            prompt: None,
            last_click: None,
        })
    }

    fn run(&mut self) -> AppResult<()> {
        self.ensure_visible();
        self.render_full()?;

        let stdin_fd = io::stdin().as_raw_fd();

        loop {
            let input = read_input_event(stdin_fd)?;
            if let InputEvent::Mouse(mouse) = input {
                if self.handle_mouse(mouse)? {
                    break;
                }
                self.ensure_visible();
                self.render_full()?;
                continue;
            }
            if let InputEvent::Ignore = input {
                continue;
            }
            let InputEvent::Key(key) = input else {
                unreachable!();
            };

            if self.prompt.is_some() {
                self.handle_prompt(key)?;
                self.ensure_visible();
                self.render_full()?;
                continue;
            }

            if self.searching {
                match key {
                    b'\r' | b'\n' if self.activate_selected()? => break,
                    b'\r' | b'\n' => {}
                    0x1b => {
                        let selected_row = self.selected_row();
                        self.query.clear();
                        self.searching = false;
                        if let Some(row) = selected_row {
                            self.select_row(row);
                        }
                    }
                    0x03 => break,
                    0x7f | 0x08 => {
                        self.query.pop();
                        self.select_first_match();
                    }
                    b' ' => self.toggle_selected_row_selection(),
                    value if value.is_ascii_graphic() || value == b' ' => {
                        self.query.push(char::from(value));
                        self.select_first_match();
                    }
                    _ => {}
                }

                self.ensure_visible();
                self.render_full()?;
                continue;
            }

            match key {
                b'j' => self.move_down(),
                b'k' => self.move_up(),
                b'g' => self.jump_first(),
                b'G' => self.jump_last(),
                b'/' => {
                    self.searching = true;
                    self.status.clear();
                }
                b'n' => self.begin_create_group(),
                b'm' => self.begin_move_session(),
                b'r' => self.begin_rename_group(),
                b'd' => self.delete_selected_group()?,
                b'h' => self.collapse_selected()?,
                b'l' => self.expand_selected()?,
                b' ' => self.toggle_selected_row_selection(),
                b'a' => self.toggle_current_group_selection(),
                b'A' => self.toggle_visible_selection(),
                b'v' => self.clear_selection(),
                b'p' => self.toggle_pin()?,
                b'x' => self.kill_selected()?,
                b'?' => self.show_help(),
                b'J' => self.reorder_down()?,
                b'K' => self.reorder_up()?,
                b'\r' | b'\n' if self.activate_selected()? => break,
                b'\r' | b'\n' => {}
                b'q' | 0x1b | 0x03 => break,
                _ => {}
            }

            self.ensure_visible();
            self.render_full()?;
        }

        Ok(())
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> AppResult<bool> {
        if let Some(delta) = mouse_wheel_delta(event.button) {
            if event.pressed && self.prompt.is_none() {
                self.scroll_visible_rows(delta);
            }
            self.last_click = None;
            return Ok(false);
        }

        if !event.pressed || self.prompt.is_some() {
            return Ok(false);
        }

        if mouse_plain_button(event.button) == Some(2) {
            return self.handle_right_click(event);
        }
        if mouse_drag_button(event.button) == Some(0) {
            self.last_click = None;
            self.drag_selected_session_to_mouse_row(event)?;
            return Ok(false);
        }
        if mouse_plain_button(event.button) != Some(0) {
            return Ok(false);
        }

        let Some((index, row)) = self.visible_mouse_row(event) else {
            self.last_click = None;
            return Ok(false);
        };
        self.selected = index;

        let layout = self.layout();
        let checkbox_start = layout.table_col + 2;
        let checkbox_end = checkbox_start + 2;
        if !self.selected_sessions.is_empty()
            && matches!(row, VisibleRow::Session(_))
            && (checkbox_start..=checkbox_end).contains(&event.col)
        {
            self.toggle_selected_row_selection();
            self.last_click = None;
            return Ok(false);
        }

        let now = Instant::now();
        let repeated = self.last_click.is_some_and(|(previous_row, previous_at)| {
            previous_row == row
                && now.saturating_duration_since(previous_at) <= Duration::from_millis(400)
        });
        self.last_click = Some((row, now));

        if repeated && self.selected_sessions.is_empty() {
            self.last_click = None;
            return self.activate_selected();
        }
        Ok(false)
    }

    fn handle_right_click(&mut self, event: MouseEvent) -> AppResult<bool> {
        let Some((index, row)) = self.visible_mouse_row(event) else {
            self.last_click = None;
            return Ok(false);
        };
        self.selected = index;
        self.last_click = None;

        let VisibleRow::Session(session_index) = row else {
            self.status = "Right-click a session to pin it".to_string();
            return Ok(false);
        };
        self.toggle_session_pin(session_index)?;
        Ok(false)
    }

    fn drag_selected_session_to_mouse_row(&mut self, event: MouseEvent) -> AppResult<()> {
        let Some((target_index, VisibleRow::Session(_))) = self.visible_mouse_row(event) else {
            return Ok(());
        };
        if !matches!(self.selected_row(), Some(VisibleRow::Session(_))) {
            return Ok(());
        }

        let mut remaining = self.visible_rows().len();
        while self.selected > target_index && remaining > 0 {
            let previous = self.selected;
            self.reorder_up()?;
            if self.selected == previous {
                break;
            }
            remaining -= 1;
        }
        while self.selected < target_index && remaining > 0 {
            let previous = self.selected;
            self.reorder_down()?;
            if self.selected == previous {
                break;
            }
            remaining -= 1;
        }
        Ok(())
    }

    fn visible_mouse_row(&self, event: MouseEvent) -> Option<(usize, VisibleRow)> {
        let layout = self.layout();
        let rows = self.visible_rows();
        let index = visible_index_for_mouse_row(
            event.row,
            layout.list_row_start,
            self.visible_list_height(),
            self.top,
            rows.len(),
        )?;
        Some((index, rows[index]))
    }

    fn scroll_visible_rows(&mut self, delta: isize) {
        let row_count = self.visible_rows().len();
        if row_count == 0 {
            self.selected = 0;
            self.top = 0;
            return;
        }

        let viewport = self.viewport_height().min(row_count);
        let max_top = row_count.saturating_sub(viewport);
        let new_top = if delta.is_negative() {
            self.top.saturating_sub(delta.unsigned_abs())
        } else {
            self.top.saturating_add(delta as usize)
        };
        self.top = new_top.min(max_top);
        self.selected = self.selected.min(row_count - 1);

        if self.selected < self.top {
            self.selected = self.top;
        }
        let bottom = self.top + viewport - 1;
        if self.selected > bottom {
            self.selected = bottom;
        }
    }

    fn visible_rows(&self) -> Vec<VisibleRow> {
        build_visible_rows(
            &self.sessions,
            &self.groups,
            &self.query,
            self.ungrouped_collapsed,
        )
    }

    fn selected_row(&self) -> Option<VisibleRow> {
        self.visible_rows().get(self.selected).copied()
    }

    fn selected_session_index(&self) -> Option<usize> {
        match self.selected_row() {
            Some(VisibleRow::Session(index)) => Some(index),
            _ => None,
        }
    }

    fn selected_group_index(&self) -> Option<usize> {
        match self.selected_row() {
            Some(VisibleRow::Group(index)) => Some(index),
            _ => None,
        }
    }

    fn select_row(&mut self, target: VisibleRow) {
        if let Some(index) = self.visible_rows().iter().position(|row| *row == target) {
            self.selected = index;
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.visible_rows().len() {
            self.selected += 1;
        }
    }

    fn jump_first(&mut self) {
        self.selected = 0;
    }

    fn jump_last(&mut self) {
        self.selected = self.visible_rows().len().saturating_sub(1);
    }

    fn has_matches(&self) -> bool {
        !self.visible_rows().is_empty()
    }

    fn select_first_match(&mut self) {
        self.selected = first_session_row_position(&self.visible_rows());
        self.top = 0;
    }

    fn reorder_up(&mut self) -> AppResult<()> {
        if let Some(group_index) = self.selected_group_index() {
            if group_index >= self.groups.groups.len() {
                self.status = "Ungrouped cannot be reordered".to_string();
            } else if let Some(new_index) = self.groups.move_group(group_index, -1) {
                self.write_groups()?;
                self.select_row(VisibleRow::Group(new_index));
                self.status = "Moved group up".to_string();
            } else {
                self.status = "Group is already first".to_string();
            }
            return Ok(());
        }

        let Some(session_index) = self.selected_session_index() else {
            return Ok(());
        };
        if !self.sessions[session_index].pinned {
            self.status = "Pin session first".to_string();
            return Ok(());
        }
        let group_index = self
            .groups
            .group_for_session(&self.sessions[session_index].name);
        let previous = (0..session_index).rev().find(|index| {
            self.sessions[*index].pinned
                && self.groups.group_for_session(&self.sessions[*index].name) == group_index
        });
        let Some(previous) = previous else {
            self.status = "Pinned session is already first".to_string();
            return Ok(());
        };

        let name = self.sessions[session_index].name.clone();
        self.sessions.swap(session_index, previous);
        self.write_pins()?;
        self.select_session_by_name(&name);
        self.status = format!("Moved {name} up");
        Ok(())
    }

    fn reorder_down(&mut self) -> AppResult<()> {
        if let Some(group_index) = self.selected_group_index() {
            if group_index >= self.groups.groups.len() {
                self.status = "Ungrouped cannot be reordered".to_string();
            } else if let Some(new_index) = self.groups.move_group(group_index, 1) {
                self.write_groups()?;
                self.select_row(VisibleRow::Group(new_index));
                self.status = "Moved group down".to_string();
            } else {
                self.status = "Group is already last".to_string();
            }
            return Ok(());
        }

        let Some(session_index) = self.selected_session_index() else {
            return Ok(());
        };
        if !self.sessions[session_index].pinned {
            self.status = "Pin session first".to_string();
            return Ok(());
        }
        let group_index = self
            .groups
            .group_for_session(&self.sessions[session_index].name);
        let next = (session_index + 1..self.sessions.len()).find(|index| {
            self.sessions[*index].pinned
                && self.groups.group_for_session(&self.sessions[*index].name) == group_index
        });
        let Some(next) = next else {
            self.status = "Pinned session is already last".to_string();
            return Ok(());
        };

        let name = self.sessions[session_index].name.clone();
        self.sessions.swap(session_index, next);
        self.write_pins()?;
        self.select_session_by_name(&name);
        self.status = format!("Moved {name} down");
        Ok(())
    }

    fn toggle_pin(&mut self) -> AppResult<()> {
        if !self.selected_sessions.is_empty() {
            let target_state = bulk_pin_target_state(&self.sessions, &self.selected_sessions);
            let selected_count = self.selected_live_session_names().len();
            for session in &mut self.sessions {
                if self.selected_sessions.contains(&session.name) {
                    session.pinned = target_state;
                }
            }

            let pinned_names = pinned_names_from_sessions(&self.sessions);
            arrange_sessions(&mut self.sessions, &pinned_names);
            self.write_pins()?;
            self.status = if target_state {
                format!("Pinned {selected_count} sessions")
            } else {
                format!("Unpinned {selected_count} sessions")
            };
            return Ok(());
        }

        let Some(session_index) = self.selected_session_index() else {
            self.status = "Select a session to pin it".to_string();
            return Ok(());
        };
        self.toggle_session_pin(session_index)
    }

    fn toggle_session_pin(&mut self, session_index: usize) -> AppResult<()> {
        let current_name = self.sessions[session_index].name.clone();
        let was_pinned = self.sessions[session_index].pinned;

        if let Some(session) = self.sessions.get_mut(session_index) {
            session.pinned = !session.pinned;
        }

        let pinned_names = pinned_names_from_sessions(&self.sessions);
        arrange_sessions(&mut self.sessions, &pinned_names);
        self.select_session_by_name(&current_name);
        self.write_pins()?;
        self.status = if was_pinned {
            format!("Unpinned {current_name}")
        } else {
            format!("Pinned {current_name}")
        };
        Ok(())
    }

    fn kill_selected(&mut self) -> AppResult<()> {
        if !self.selected_sessions.is_empty() {
            self.begin_kill_selected_sessions();
            return Ok(());
        }

        let Some(session_index) = self.selected_session_index() else {
            self.status = "Select a session to kill it".to_string();
            return Ok(());
        };
        if self.sessions[session_index].is_current {
            self.status = "Cannot kill current session".to_string();
            return Ok(());
        }

        let session_name = self.sessions[session_index].name.clone();
        let rows = self.visible_rows();
        let next_selected_name = rows
            .iter()
            .skip(self.selected + 1)
            .chain(rows[..self.selected].iter().rev())
            .find_map(|row| match row {
                VisibleRow::Session(index) if *index != session_index => {
                    Some(self.sessions[*index].name.clone())
                }
                _ => None,
            });
        tmux_status(
            &self.tmux_socket_name,
            &self.tmux_socket_path,
            &["kill-session", "-t", &session_name],
        )?;

        self.reload_sessions(next_selected_name.as_deref())?;
        self.status = format!("Killed {session_name}");
        Ok(())
    }

    fn activate_selected(&mut self) -> AppResult<bool> {
        if !self.selected_sessions.is_empty() {
            self.prompt = Some(Prompt::Action { choice: 0 });
            return Ok(false);
        }

        let Some(row) = self.selected_row() else {
            return Ok(false);
        };
        if let VisibleRow::Group(group_index) = row {
            self.toggle_group(group_index)?;
            return Ok(false);
        }

        let VisibleRow::Session(session_index) = row else {
            return Ok(false);
        };
        let session_name = self.sessions[session_index].name.clone();
        tmux_status(
            &self.tmux_socket_name,
            &self.tmux_socket_path,
            &["switch-client", "-t", &session_name],
        )?;
        Ok(true)
    }

    fn begin_create_group(&mut self) {
        self.prompt = Some(Prompt::Name {
            label: "NEW GROUP",
            value: String::new(),
            action: NameAction::Create,
        });
    }

    fn begin_rename_group(&mut self) {
        let Some(group_index) = self.selected_group_index() else {
            self.status = "Select a group to rename it".to_string();
            return;
        };
        let Some(group) = self.groups.groups.get(group_index) else {
            self.status = "Ungrouped cannot be renamed".to_string();
            return;
        };
        self.prompt = Some(Prompt::Name {
            label: "RENAME GROUP",
            value: group.name.clone(),
            action: NameAction::Rename(group_index),
        });
    }

    fn begin_move_session(&mut self) {
        let Some(session_names) = self.target_session_names() else {
            self.status = "Select a session to move it".to_string();
            return;
        };
        let choice = if session_names.len() == 1 {
            self.groups
                .group_for_session(&session_names[0])
                .unwrap_or(self.groups.groups.len())
        } else {
            self.groups.groups.len()
        };
        self.prompt = Some(Prompt::Move {
            session_names,
            choice,
        });
    }

    fn handle_prompt(&mut self, byte: u8) -> AppResult<()> {
        let Some(mut prompt) = self.prompt.take() else {
            return Ok(());
        };

        match &mut prompt {
            Prompt::Name { value, action, .. } => match byte {
                b'\r' | b'\n' => {
                    let result = self.submit_group_name(value, action.clone());
                    if let Err(err) = result {
                        self.status = err;
                        self.prompt = Some(prompt);
                    }
                }
                0x1b | 0x03 => self.status = "Cancelled".to_string(),
                0x7f | 0x08 => {
                    if value.is_empty() {
                        self.status = "Cancelled".to_string();
                    } else {
                        value.pop();
                        self.prompt = Some(prompt);
                    }
                }
                value_byte if value_byte.is_ascii_graphic() || value_byte == b' ' => {
                    value.push(char::from(value_byte));
                    self.prompt = Some(prompt);
                }
                _ => self.prompt = Some(prompt),
            },
            Prompt::Move {
                session_names,
                choice,
            } => {
                let option_count = self.groups.groups.len() + 2;
                match byte {
                    b'j' => *choice = (*choice + 1).min(option_count - 1),
                    b'k' => *choice = choice.saturating_sub(1),
                    b'g' => *choice = 0,
                    b'G' => *choice = option_count - 1,
                    b'\r' | b'\n' => {
                        let session_names = session_names.clone();
                        let choice = *choice;
                        if choice < self.groups.groups.len() {
                            self.move_sessions_to(&session_names, Some(choice))?;
                            return Ok(());
                        }
                        if choice == self.groups.groups.len() {
                            self.move_sessions_to(&session_names, None)?;
                            return Ok(());
                        }
                        self.prompt = Some(Prompt::Name {
                            label: "NEW GROUP",
                            value: String::new(),
                            action: NameAction::CreateAndMove(session_names),
                        });
                        return Ok(());
                    }
                    0x1b | 0x03 => {
                        self.status = "Cancelled".to_string();
                        return Ok(());
                    }
                    _ => {}
                }
                self.prompt = Some(prompt);
            }
            Prompt::ConfirmKill {
                session_names,
                skipped_current,
            } => match byte {
                b'y' | b'Y' => {
                    let session_names = session_names.clone();
                    let skipped_current = *skipped_current;
                    self.kill_session_names(&session_names, skipped_current)?;
                }
                b'n' | b'N' | 0x1b | 0x03 => self.status = "Cancelled".to_string(),
                _ => self.prompt = Some(prompt),
            },
            Prompt::Action { choice } => {
                let action_count = 4;
                match byte {
                    b'j' => *choice = (*choice + 1).min(action_count - 1),
                    b'k' => *choice = choice.saturating_sub(1),
                    b'\r' | b'\n' => {
                        let choice = *choice;
                        match choice {
                            0 => self.begin_move_session(),
                            1 => self.toggle_pin()?,
                            2 => self.begin_kill_selected_sessions(),
                            _ => self.clear_selection(),
                        }
                        return Ok(());
                    }
                    0x1b | 0x03 => {
                        self.status = "Cancelled".to_string();
                        return Ok(());
                    }
                    _ => {}
                }
                self.prompt = Some(prompt);
            }
            Prompt::Help { index } => {
                match byte {
                    b'j' | b'l' | b' ' => *index = next_help_index(*index, 1),
                    b'k' | b'h' | 0x7f | 0x08 => *index = next_help_index(*index, -1),
                    b'g' => *index = 0,
                    b'G' => *index = SHORTCUTS.len().saturating_sub(1),
                    b'q' | b'?' | 0x1b | 0x03 | b'\r' | b'\n' => {
                        self.status = "Closed help".to_string();
                        return Ok(());
                    }
                    _ => {}
                }
                self.prompt = Some(prompt);
            }
        }
        Ok(())
    }

    fn submit_group_name(&mut self, value: &str, action: NameAction) -> Result<(), String> {
        match action {
            NameAction::Create => {
                let group_index = self.groups.add_group(value)?;
                self.groups.save(&self.group_file)?;
                self.query.clear();
                self.select_row(VisibleRow::Group(group_index));
                self.status = format!("Created {}", self.groups.groups[group_index].name);
            }
            NameAction::Rename(group_index) => {
                self.groups.rename_group(group_index, value)?;
                self.groups.save(&self.group_file)?;
                self.status = format!("Renamed group to {}", self.groups.groups[group_index].name);
            }
            NameAction::CreateAndMove(session_names) => {
                let group_index = self.groups.add_group(value)?;
                for session_name in &session_names {
                    self.groups.move_session(session_name, Some(group_index));
                }
                self.groups.save(&self.group_file)?;
                if let Some(session_name) = session_names.first() {
                    self.select_session_by_name(session_name);
                }
                self.status = format!(
                    "Moved {} sessions to {}",
                    session_names.len(),
                    self.groups.groups[group_index].name
                );
            }
        }
        Ok(())
    }

    fn move_sessions_to(
        &mut self,
        session_names: &[String],
        group_index: Option<usize>,
    ) -> AppResult<()> {
        for session_name in session_names {
            self.groups.move_session(session_name, group_index);
        }
        if let Some(group) = group_index.and_then(|index| self.groups.groups.get_mut(index)) {
            group.collapsed = false;
        } else {
            self.ungrouped_collapsed = false;
        }
        self.write_groups()?;
        if let Some(session_name) = session_names.first() {
            self.select_session_by_name(session_name);
        }
        let destination = group_index
            .and_then(|index| self.groups.groups.get(index))
            .map(|group| group.name.as_str())
            .unwrap_or(groups::UNGROUPED_NAME);
        self.status = if session_names.len() == 1 {
            format!("Moved {} to {destination}", session_names[0])
        } else {
            format!("Moved {} sessions to {destination}", session_names.len())
        };
        Ok(())
    }

    fn begin_kill_selected_sessions(&mut self) {
        let (session_names, skipped_current) = self.killable_selected_session_names();
        if session_names.is_empty() {
            self.status = if skipped_current > 0 {
                "Cannot kill current session".to_string()
            } else {
                "No selected sessions to kill".to_string()
            };
            return;
        }
        self.prompt = Some(Prompt::ConfirmKill {
            session_names,
            skipped_current,
        });
    }

    fn kill_session_names(
        &mut self,
        session_names: &[String],
        skipped_current: usize,
    ) -> AppResult<()> {
        for session_name in session_names {
            tmux_status(
                &self.tmux_socket_name,
                &self.tmux_socket_path,
                &["kill-session", "-t", session_name],
            )?;
            self.selected_sessions.remove(session_name);
        }
        self.reload_sessions(None)?;
        self.status = if skipped_current > 0 {
            format!("Killed {}; skipped current", session_names.len())
        } else {
            format!("Killed {} sessions", session_names.len())
        };
        Ok(())
    }

    fn selected_live_session_names(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|session| self.selected_sessions.contains(&session.name))
            .map(|session| session.name.clone())
            .collect()
    }

    fn target_session_names(&self) -> Option<Vec<String>> {
        let selected_names = self.selected_live_session_names();
        if !selected_names.is_empty() {
            return Some(selected_names);
        }

        self.selected_session_index()
            .map(|index| vec![self.sessions[index].name.clone()])
    }

    fn killable_selected_session_names(&self) -> (Vec<String>, usize) {
        let mut skipped_current = 0;
        let session_names = self
            .sessions
            .iter()
            .filter(|session| self.selected_sessions.contains(&session.name))
            .filter_map(|session| {
                if session.is_current {
                    skipped_current += 1;
                    None
                } else {
                    Some(session.name.clone())
                }
            })
            .collect();
        (session_names, skipped_current)
    }

    fn toggle_selected_row_selection(&mut self) {
        match self.selected_row() {
            Some(VisibleRow::Session(index)) => toggle_selection_for_session(
                &mut self.selected_sessions,
                &self.sessions[index].name,
            ),
            Some(VisibleRow::Group(index)) => toggle_selection_for_group(
                &mut self.selected_sessions,
                &self.sessions,
                &self.groups,
                index,
            ),
            None => {}
        }
        self.update_selection_status();
    }

    fn toggle_current_group_selection(&mut self) {
        let group_index = match self.selected_row() {
            Some(VisibleRow::Group(index)) => index,
            Some(VisibleRow::Session(index)) => self
                .groups
                .group_for_session(&self.sessions[index].name)
                .unwrap_or(self.groups.groups.len()),
            None => return,
        };
        toggle_selection_for_group(
            &mut self.selected_sessions,
            &self.sessions,
            &self.groups,
            group_index,
        );
        self.update_selection_status();
    }

    fn toggle_visible_selection(&mut self) {
        let rows = self.visible_rows();
        toggle_selection_for_rows(&mut self.selected_sessions, &self.sessions, &rows);
        self.update_selection_status();
    }

    fn clear_selection(&mut self) {
        self.selected_sessions.clear();
        self.status = "Selection cleared".to_string();
    }

    fn show_help(&mut self) {
        self.prompt = Some(Prompt::Help { index: 0 });
    }

    fn update_selection_status(&mut self) {
        let count = self.selected_live_session_names().len();
        self.status = if count == 0 {
            "Selection cleared".to_string()
        } else {
            format!("Selected {count} sessions")
        };
    }

    fn delete_selected_group(&mut self) -> AppResult<()> {
        let Some(group_index) = self.selected_group_index() else {
            self.status = "Select a group to delete it".to_string();
            return Ok(());
        };
        if group_index >= self.groups.groups.len() {
            self.status = "Ungrouped cannot be deleted".to_string();
            return Ok(());
        }
        let name = self.groups.groups[group_index].name.clone();
        self.groups
            .delete_group(group_index)
            .map_err(|err| -> Box<dyn Error> { err.into() })?;
        self.write_groups()?;
        self.selected = self
            .selected
            .min(self.visible_rows().len().saturating_sub(1));
        self.status = format!("Deleted {name}; sessions are ungrouped");
        Ok(())
    }

    fn collapse_selected(&mut self) -> AppResult<()> {
        let group_index = match self.selected_row() {
            Some(VisibleRow::Group(index)) => index,
            Some(VisibleRow::Session(index)) => self
                .groups
                .group_for_session(&self.sessions[index].name)
                .unwrap_or(self.groups.groups.len()),
            None => return Ok(()),
        };
        self.set_group_collapsed(group_index, true)
    }

    fn expand_selected(&mut self) -> AppResult<()> {
        let group_index = match self.selected_row() {
            Some(VisibleRow::Group(index)) => index,
            Some(VisibleRow::Session(index)) => self
                .groups
                .group_for_session(&self.sessions[index].name)
                .unwrap_or(self.groups.groups.len()),
            None => return Ok(()),
        };
        self.set_group_collapsed(group_index, false)
    }

    fn toggle_group(&mut self, group_index: usize) -> AppResult<()> {
        let collapsed = if group_index < self.groups.groups.len() {
            !self.groups.groups[group_index].collapsed
        } else {
            !self.ungrouped_collapsed
        };
        self.set_group_collapsed(group_index, collapsed)
    }

    fn set_group_collapsed(&mut self, group_index: usize, collapsed: bool) -> AppResult<()> {
        if let Some(group) = self.groups.groups.get_mut(group_index) {
            group.collapsed = collapsed;
            self.write_groups()?;
        } else {
            self.ungrouped_collapsed = collapsed;
        }
        self.select_row(VisibleRow::Group(group_index));
        Ok(())
    }

    fn select_session_by_name(&mut self, name: &str) {
        if let Some(session_index) = self
            .sessions
            .iter()
            .position(|session| session.name == name)
        {
            self.select_row(VisibleRow::Session(session_index));
        }
    }

    fn ensure_visible(&mut self) {
        let row_count = self.visible_rows().len();
        if row_count == 0 {
            self.selected = 0;
            self.top = 0;
            return;
        }
        self.selected = self.selected.min(row_count - 1);
        let viewport = self.viewport_height();
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + viewport {
            self.top = self.selected + 1 - viewport;
        }
    }

    fn viewport_height(&self) -> usize {
        let reserved_rows = 4;
        self.rows.saturating_sub(reserved_rows).max(1)
    }

    fn visible_list_height(&self) -> usize {
        self.viewport_height().min(self.visible_rows().len().max(1))
    }

    fn layout(&self) -> Layout {
        let max_name_width = self
            .sessions
            .iter()
            .map(|session| session.name.chars().count())
            .chain(
                self.groups
                    .groups
                    .iter()
                    .map(|group| group.name.chars().count()),
            )
            .max()
            .unwrap_or(8);
        let min_name_width = 16;
        let activity_width = 4;
        let fixed_width = 7 + min_name_width + 2 + activity_width + 2 + 3;
        let max_name_width = self
            .cols
            .saturating_sub(fixed_width)
            .clamp(min_name_width, max_name_width.max(min_name_width));
        let name_width = max_name_width.max(min_name_width).min(max_name_width);
        let table_width = 2 + name_width + 2 + activity_width + 2 + 3;
        let list_height = self.visible_list_height();
        let content_height = list_height + 2;
        let top_padding = self.rows.saturating_sub(content_height);
        let _table_width = table_width;
        let table_col = 1;
        let list_row_start = top_padding + 1;
        let blank_row = list_row_start + list_height;
        let status_row = blank_row + 1;

        Layout {
            table_col,
            name_width,
            activity_width,
            list_row_start,
            blank_row,
            status_row,
        }
    }

    fn render_full(&self) -> AppResult<()> {
        let mut stdout = io::stdout().lock();
        write!(stdout, "\x1b[H\x1b[2J")?;
        self.render_static(&mut stdout)?;
        self.render_list_area(&mut stdout)?;
        self.render_footer(&mut stdout)?;
        self.render_move_popup(&mut stdout)?;
        self.render_help_popup(&mut stdout)?;
        stdout.flush()?;
        Ok(())
    }

    fn render_static(&self, stdout: &mut impl Write) -> AppResult<()> {
        let layout = self.layout();
        if layout.list_row_start > 1 {
            let row = layout.list_row_start - 1;
            let label = mode_line(
                self.prompt.as_ref(),
                self.searching,
                &self.query,
                self.has_matches(),
                self.selected_live_session_names().len(),
            );
            write_at(
                stdout,
                row,
                layout.table_col,
                &truncate(&label, self.cols),
                false,
            )?;
        }
        Ok(())
    }

    fn render_list_area(&self, stdout: &mut impl Write) -> AppResult<()> {
        let rows = self.visible_rows();
        for visible_row in 0..self.visible_list_height() {
            let row = rows.get(self.top + visible_row).copied();
            self.render_row_at(stdout, visible_row, row)?;
        }
        Ok(())
    }

    fn render_row_at(
        &self,
        stdout: &mut impl Write,
        visible_row: usize,
        visible: Option<VisibleRow>,
    ) -> AppResult<()> {
        let layout = self.layout();
        let row = layout.list_row_start + visible_row;
        let selected = self.top + visible_row == self.selected;
        let pointer = if selected { ">" } else { " " };

        let line = match visible {
            Some(VisibleRow::Group(group_index)) => {
                let (name, collapsed) = if let Some(group) = self.groups.groups.get(group_index) {
                    (group.name.as_str(), group.collapsed)
                } else {
                    (groups::UNGROUPED_NAME, self.ungrouped_collapsed)
                };
                let (selected_count, count) = selected_count_for_group(
                    &self.sessions,
                    &self.groups,
                    &self.selected_sessions,
                    group_index,
                );
                let count_label = if selected_count > 0 {
                    format!("{selected_count}/{count}")
                } else {
                    count.to_string()
                };
                let marker = if collapsed { "▸" } else { "▼" };
                format!("{pointer} {marker} {name} ({count_label})")
            }
            Some(VisibleRow::Session(session_index)) => {
                let session = &self.sessions[session_index];
                session_row_line(
                    pointer,
                    session,
                    &self.selected_sessions,
                    layout.name_width,
                    layout.activity_width,
                )
            }
            None => String::new(),
        };
        let line = truncate(
            &line,
            self.cols.saturating_sub(layout.table_col.saturating_sub(1)),
        );
        if line.is_empty() {
            write_row(stdout, row, "", false)?;
        } else {
            write_at(stdout, row, layout.table_col, &line, selected)?;
        }
        Ok(())
    }

    fn render_footer(&self, stdout: &mut impl Write) -> AppResult<()> {
        let layout = self.layout();
        write_row(stdout, layout.blank_row, "", false)?;
        if let Some(prompt) = &self.prompt {
            let text = match prompt {
                Prompt::Name { label, value, .. } => format!("{label}: {value}"),
                Prompt::Move {
                    session_names,
                    choice,
                } => {
                    let destination = if *choice < self.groups.groups.len() {
                        self.groups.groups[*choice].name.as_str()
                    } else if *choice == self.groups.groups.len() {
                        groups::UNGROUPED_NAME
                    } else {
                        "New group..."
                    };
                    format!(
                        "MOVE {} sessions -> [{destination}]  j/k choose  Enter confirm  Esc cancel",
                        session_names.len()
                    )
                }
                Prompt::ConfirmKill {
                    session_names,
                    skipped_current,
                } => {
                    let suffix = if *skipped_current > 0 {
                        " (current skipped)"
                    } else {
                        ""
                    };
                    format!("KILL {} sessions{suffix}? y/N", session_names.len())
                }
                Prompt::Action { choice } => {
                    let actions = ["move", "pin/unpin", "kill", "clear"];
                    format!(
                        "SELECTED {} sessions: [{}]  j/k choose  Enter confirm  Esc cancel",
                        self.selected_live_session_names().len(),
                        actions[*choice]
                    )
                }
                Prompt::Help { .. } => "Shortcut help open".to_string(),
            };
            write_at(
                stdout,
                layout.status_row,
                1,
                &truncate(&text, self.cols),
                false,
            )?;
        } else if self.searching {
            let suffix = if self.has_matches() {
                ""
            } else {
                "  No matching sessions"
            };
            write_at(
                stdout,
                layout.status_row,
                1,
                &truncate(&format!("SEARCH /{}{suffix}", self.query), self.cols),
                false,
            )?;
        } else if self.status.is_empty() {
            write_row(stdout, layout.status_row, "", false)?;
        } else {
            write_at(
                stdout,
                layout.status_row,
                1,
                &truncate(&self.status, self.cols),
                false,
            )?;
        }
        write!(stdout, "\x1b[{};1H\x1b[J", layout.status_row + 1)?;
        Ok(())
    }

    fn render_help_popup(&self, stdout: &mut impl Write) -> AppResult<()> {
        let Some(Prompt::Help { index }) = self.prompt.as_ref() else {
            return Ok(());
        };

        let popup_height = help_popup_height(self.rows).min(self.rows.max(1));
        let max_entries = popup_height.saturating_sub(4).max(1);
        let lines = help_popup_lines(*index, max_entries);
        let content_width = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .min(self.cols.saturating_sub(4).max(1));
        let popup_width = (content_width + 4).min(self.cols.max(1));
        let row_start = self.rows.saturating_sub(popup_height) / 2 + 1;
        let col_start = self.cols.saturating_sub(popup_width) / 2 + 1;
        let inner_width = popup_width.saturating_sub(2);

        let top = format!("+{}+", "-".repeat(inner_width));
        write_at(stdout, row_start, col_start, &top, false)?;

        let body_height = popup_height.saturating_sub(2);
        for offset in 0..body_height {
            let line = lines.get(offset).map(String::as_str).unwrap_or("");
            let line = truncate(line, inner_width.saturating_sub(2));
            let padded = format!(
                "| {:<width$} |",
                line,
                width = inner_width.saturating_sub(2)
            );
            write_at(stdout, row_start + offset + 1, col_start, &padded, false)?;
        }

        let bottom = format!("+{}+", "-".repeat(inner_width));
        write_at(
            stdout,
            row_start + popup_height - 1,
            col_start,
            &bottom,
            false,
        )?;
        Ok(())
    }

    fn render_move_popup(&self, stdout: &mut impl Write) -> AppResult<()> {
        let Some(Prompt::Move {
            session_names,
            choice,
        }) = self.prompt.as_ref()
        else {
            return Ok(());
        };

        let lines = move_popup_lines(&self.groups, *choice, session_names.len());
        let content_width = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .min(self.cols.saturating_sub(4).max(1));
        let popup_width = (content_width + 4).min(self.cols.max(1));
        let popup_height = (lines.len() + 2).min(self.rows.max(1));
        let row_start = self.rows.saturating_sub(popup_height) / 2 + 1;
        let col_start = self.cols.saturating_sub(popup_width) / 2 + 1;
        let inner_width = popup_width.saturating_sub(2);

        let top = format!("+{}+", "-".repeat(inner_width));
        write_at(stdout, row_start, col_start, &top, false)?;

        for (offset, line) in lines
            .iter()
            .take(popup_height.saturating_sub(2))
            .enumerate()
        {
            let line = truncate(line, inner_width.saturating_sub(2));
            let padded = format!(
                "| {:<width$} |",
                line,
                width = inner_width.saturating_sub(2)
            );
            write_at(stdout, row_start + offset + 1, col_start, &padded, false)?;
        }

        let bottom = format!("+{}+", "-".repeat(inner_width));
        write_at(
            stdout,
            row_start + popup_height - 1,
            col_start,
            &bottom,
            false,
        )?;
        Ok(())
    }

    fn write_pins(&self) -> AppResult<()> {
        write_pinned_names(&self.pin_file, &pinned_names_from_sessions(&self.sessions))
    }

    fn write_groups(&self) -> AppResult<()> {
        self.groups.save(&self.group_file).map_err(|err| err.into())
    }

    fn reload_sessions(&mut self, preferred_name: Option<&str>) -> AppResult<()> {
        self.sessions = load_sessions(
            &self.pin_file,
            &self.tmux_socket_name,
            &self.tmux_socket_path,
        )?;
        prune_selected_sessions(&mut self.selected_sessions, &self.sessions);
        let selected_name = preferred_name.map(str::to_string).or_else(|| {
            self.sessions
                .iter()
                .find(|session| session.is_current)
                .map(|session| session.name.clone())
        });
        if let Some(name) = selected_name {
            self.select_session_by_name(&name);
        } else {
            self.selected = self
                .selected
                .min(self.visible_rows().len().saturating_sub(1));
        }
        self.top = self.top.min(self.selected);
        Ok(())
    }
}

fn session_name_matches(name: &str, query: &str) -> bool {
    name.to_lowercase().contains(&query.to_lowercase())
}

struct TerminalGuard {
    fd: RawFd,
    saved_state: libc::termios,
}

impl TerminalGuard {
    fn enter() -> AppResult<Self> {
        let fd = io::stdin().as_raw_fd();
        let saved_state = get_termios(fd)?;
        let mut raw_state = saved_state;
        unsafe { libc::cfmakeraw(&mut raw_state) };
        set_termios(fd, &raw_state)?;

        let mut stdout = io::stdout().lock();
        write!(
            stdout,
            "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        )?;
        stdout.flush()?;

        Ok(Self { fd, saved_state })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = set_termios(self.fd, &self.saved_state);
        let mut stdout = io::stdout().lock();
        let _ = write!(
            stdout,
            "\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l"
        );
        let _ = stdout.flush();
    }
}

fn pin_file_path() -> PathBuf {
    if let Ok(path) = env::var("TMUX_SESSION_PIN_FILE") {
        return PathBuf::from(path);
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/tmux/session-pins")
}

fn group_file_path() -> PathBuf {
    if let Ok(path) = env::var("TMUX_SESSION_GROUP_FILE") {
        return PathBuf::from(path);
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/tmux/session-groups.toml")
}

fn load_sessions(
    pin_file: &PathBuf,
    socket_name: &Option<String>,
    socket_path: &Option<String>,
) -> AppResult<Vec<Session>> {
    let pinned_names = read_pinned_names(pin_file);
    let current_session = tmux_output(socket_name, socket_path, &["display-message", "-p", "#S"])?
        .trim()
        .to_string();
    let raw_sessions = tmux_output(
        socket_name,
        socket_path,
        &[
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_activity}",
        ],
    )?;

    let mut sessions = parse_sessions(&raw_sessions, &current_session, &pinned_names)?;
    arrange_sessions(&mut sessions, &pinned_names);
    write_pinned_names(pin_file, &pinned_names_from_sessions(&sessions))?;
    Ok(sessions)
}

fn tmux_output(
    socket_name: &Option<String>,
    socket_path: &Option<String>,
    args: &[&str],
) -> AppResult<String> {
    let mut cmd = Command::new("tmux");
    if let Some(name) = socket_name {
        cmd.args(["-L", name]);
    }
    if let Some(path) = socket_path {
        cmd.args(["-S", path]);
    }
    cmd.args(args);

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux command failed: {}", stderr.trim()).into());
    }

    Ok(String::from_utf8(output.stdout)?)
}

fn tmux_status(
    socket_name: &Option<String>,
    socket_path: &Option<String>,
    args: &[&str],
) -> AppResult<()> {
    let mut cmd = Command::new("tmux");
    if let Some(name) = socket_name {
        cmd.args(["-L", name]);
    }
    if let Some(path) = socket_path {
        cmd.args(["-S", path]);
    }
    cmd.args(args);

    let status = cmd.status()?;
    if !status.success() {
        return Err("tmux command failed".into());
    }

    Ok(())
}

fn parse_sessions(
    raw: &str,
    current_session: &str,
    pinned_names: &[String],
) -> AppResult<Vec<Session>> {
    let mut sessions = Vec::new();

    for line in raw.lines() {
        let mut parts = line.split('\t');
        let name = parts.next().unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }

        let last_activity = parts
            .next()
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(|err| format!("invalid last activity for {name}: {err}"))?;

        sessions.push(Session {
            name: name.clone(),
            last_activity,
            pinned: pinned_names.iter().any(|pinned_name| pinned_name == &name),
            is_current: name == current_session,
        });
    }

    if sessions.is_empty() {
        return Err("no tmux sessions found".into());
    }

    Ok(sessions)
}

fn read_pinned_names(pin_file: &PathBuf) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(pin_file) else {
        return Vec::new();
    };

    let mut names = Vec::new();
    for line in contents.lines().filter(|line| !line.is_empty()) {
        if !names.iter().any(|name| name == line) {
            names.push(line.to_string());
        }
    }
    names
}

fn write_pinned_names(pin_file: &PathBuf, pinned_names: &[String]) -> AppResult<()> {
    if let Some(parent) = pin_file.parent() {
        fs::create_dir_all(parent)?;
    }

    let contents = pinned_names.join("\n");
    fs::write(pin_file, format!("{contents}\n"))?;
    Ok(())
}

fn pinned_names_from_sessions(sessions: &[Session]) -> Vec<String> {
    sessions
        .iter()
        .filter(|session| session.pinned)
        .map(|session| session.name.clone())
        .collect()
}

fn arrange_sessions(sessions: &mut Vec<Session>, pinned_names: &[String]) {
    let mut remaining = std::mem::take(sessions);
    let mut pinned = Vec::with_capacity(remaining.len());
    let mut unpinned = Vec::with_capacity(remaining.len());

    for pinned_name in pinned_names {
        if let Some(index) = remaining
            .iter()
            .position(|session| session.pinned && session.name == *pinned_name)
        {
            pinned.push(remaining.remove(index));
        }
    }

    for session in remaining.drain(..) {
        if session.pinned {
            pinned.push(session);
        } else {
            unpinned.push(session);
        }
    }

    unpinned.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.name.cmp(&b.name))
    });

    pinned.extend(unpinned);
    *sessions = pinned;
}

fn terminal_size() -> AppResult<(usize, usize)> {
    let fd = io::stdout().as_raw_fd();
    let mut winsize = MaybeUninit::<libc::winsize>::zeroed();
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, winsize.as_mut_ptr()) };
    if rc != 0 {
        return Err("failed to read terminal size".into());
    }

    let winsize = unsafe { winsize.assume_init() };
    let rows = usize::from(winsize.ws_row).max(1);
    let cols = usize::from(winsize.ws_col).max(1);
    Ok((rows, cols))
}

fn truncate(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }

    if max_width <= 1 {
        return "…".to_string();
    }

    let mut result = text.chars().take(max_width - 1).collect::<String>();
    result.push('…');
    result
}

fn format_relative_activity(timestamp: u64) -> String {
    if timestamp == 0 {
        return "-".to_string();
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(timestamp);

    if now <= timestamp {
        return "now".to_string();
    }

    let delta = now - timestamp;

    match delta {
        0..=4 => "now".to_string(),
        5..=59 => format!("{delta}s"),
        60..=3599 => format!("{}m", delta / 60),
        3600..=86399 => format!("{}h", delta / 3600),
        86400..=604799 => format!("{}d", delta / 86400),
        _ => format!("{}w", delta / 604800),
    }
}

fn write_row(stdout: &mut impl Write, row: usize, text: &str, selected: bool) -> io::Result<()> {
    write!(stdout, "\x1b[{};1H\x1b[2K", row)?;
    if selected {
        write!(stdout, "\x1b[7m{text}\x1b[0m")?;
    } else {
        write!(stdout, "{text}")?;
    }
    Ok(())
}

fn write_at(
    stdout: &mut impl Write,
    row: usize,
    col: usize,
    text: &str,
    selected: bool,
) -> io::Result<()> {
    write!(stdout, "\x1b[{};1H\x1b[2K", row)?;
    if selected {
        write!(stdout, "\x1b[{};{}H\x1b[7m{text}\x1b[0m", row, col)?;
    } else {
        write!(stdout, "\x1b[{};{}H{text}", row, col)?;
    }
    Ok(())
}

fn get_termios(fd: RawFd) -> AppResult<libc::termios> {
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc != 0 {
        return Err("failed to read terminal mode".into());
    }
    Ok(unsafe { termios.assume_init() })
}

fn set_termios(fd: RawFd, termios: &libc::termios) -> AppResult<()> {
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) };
    if rc != 0 {
        return Err("failed to update terminal mode".into());
    }
    Ok(())
}

fn main() -> AppResult<()> {
    let _terminal = TerminalGuard::enter()?;
    let mut app = App::new()?;
    app.run()
}

#[cfg(test)]
mod tests {
    use super::{
        App, MouseEvent, NameAction, Prompt, SHORTCUTS, Session, VisibleRow, arrange_sessions,
        build_visible_rows, bulk_pin_target_state, first_session_row_position,
        format_relative_activity, help_popup_height, help_popup_lines, mode_line,
        mouse_wheel_delta, move_popup_lines, next_help_index, parse_mouse_escape,
        pinned_names_from_sessions, prune_selected_sessions, selected_count_for_group,
        session_name_matches, session_row_line, toggle_selection_for_group,
        toggle_selection_for_rows, visible_index_for_mouse_row, write_pinned_names,
    };
    use crate::groups::{Group, GroupState};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn session(name: &str, last_activity: u64, pinned: bool) -> Session {
        Session {
            name: name.to_string(),
            last_activity,
            pinned,
            is_current: false,
        }
    }

    fn temp_state_file(suffix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tmux-session-picker-test-{nanos}.{suffix}"))
    }

    fn app_with_sessions(sessions: Vec<Session>) -> App {
        App {
            sessions,
            groups: GroupState::default(),
            selected_sessions: BTreeSet::new(),
            selected: 0,
            top: 0,
            rows: 24,
            cols: 80,
            pin_file: PathBuf::new(),
            group_file: PathBuf::new(),
            tmux_socket_name: None,
            tmux_socket_path: None,
            status: String::new(),
            query: String::new(),
            searching: false,
            ungrouped_collapsed: false,
            prompt: None,
            last_click: None,
        }
    }

    #[test]
    fn arrange_sessions_keeps_pinned_first_and_sorts_unpinned_by_activity() {
        let mut sessions = vec![
            session("pin-a", 5, true),
            session("old", 10, false),
            session("new", 100, false),
            session("pin-b", 50, true),
            session("mid", 50, false),
        ];
        let pinned_names = vec!["pin-b".to_string(), "pin-a".to_string()];
        arrange_sessions(&mut sessions, &pinned_names);

        let names = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["pin-b", "pin-a", "new", "mid", "old"]);
    }

    #[test]
    fn pinned_names_follow_current_pinned_order() {
        let sessions = vec![
            session("pin-a", 0, true),
            session("pin-b", 0, true),
            session("free", 0, false),
        ];
        assert_eq!(
            pinned_names_from_sessions(&sessions),
            vec!["pin-a".to_string(), "pin-b".to_string()]
        );
    }

    #[test]
    fn write_pinned_names_creates_plain_line_file() {
        let pin_file = temp_state_file("pins");
        let pins = vec!["config".to_string(), "dashboard".to_string()];
        write_pinned_names(&pin_file, &pins).unwrap();
        assert_eq!(
            fs::read_to_string(&pin_file).unwrap(),
            "config\ndashboard\n"
        );
        let _ = fs::remove_file(pin_file);
    }

    #[test]
    fn format_relative_activity_renders_short_age() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_eq!(format_relative_activity(0), "-");
        assert_eq!(format_relative_activity(now), "now");
        assert_eq!(format_relative_activity(now.saturating_sub(12)), "12s");
        assert_eq!(format_relative_activity(now.saturating_sub(180)), "3m");
        assert_eq!(format_relative_activity(now.saturating_sub(7200)), "2h");
    }

    #[test]
    fn session_name_search_is_case_insensitive() {
        assert!(session_name_matches("Guide-Helper", "helper"));
        assert!(session_name_matches("Guide-Helper", "GUIDE"));
        assert!(!session_name_matches("Guide-Helper", "dashboard"));
    }

    #[test]
    fn visible_rows_include_group_headers_and_ungrouped_sessions() {
        let sessions = vec![
            session("api", 0, false),
            session("notes", 0, false),
            session("scratch", 0, false),
        ];
        let groups = GroupState {
            version: 1,
            groups: vec![
                Group {
                    name: "Work".to_string(),
                    collapsed: false,
                    sessions: vec!["api".to_string()],
                },
                Group {
                    name: "Personal".to_string(),
                    collapsed: false,
                    sessions: vec!["notes".to_string()],
                },
            ],
        };

        assert_eq!(
            build_visible_rows(&sessions, &groups, "", false),
            vec![
                VisibleRow::Group(0),
                VisibleRow::Session(0),
                VisibleRow::Group(1),
                VisibleRow::Session(1),
                VisibleRow::Group(2),
                VisibleRow::Session(2),
            ]
        );
    }

    #[test]
    fn collapsed_groups_hide_sessions_but_search_reveals_matches() {
        let sessions = vec![session("api", 0, false), session("database", 0, false)];
        let groups = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: true,
                sessions: vec!["api".to_string(), "database".to_string()],
            }],
        };

        assert_eq!(
            build_visible_rows(&sessions, &groups, "", false),
            vec![VisibleRow::Group(0), VisibleRow::Group(1)]
        );
        assert_eq!(
            build_visible_rows(&sessions, &groups, "data", false),
            vec![VisibleRow::Group(0), VisibleRow::Session(1)]
        );
        assert!(groups.groups[0].collapsed);
    }

    #[test]
    fn matching_a_group_name_reveals_all_of_its_sessions() {
        let sessions = vec![session("api", 0, false), session("database", 0, false)];
        let groups = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: true,
                sessions: vec!["api".to_string(), "database".to_string()],
            }],
        };

        assert_eq!(
            build_visible_rows(&sessions, &groups, "work", false),
            vec![
                VisibleRow::Group(0),
                VisibleRow::Session(0),
                VisibleRow::Session(1),
            ]
        );
    }

    #[test]
    fn search_selection_skips_group_headers() {
        let rows = vec![
            VisibleRow::Group(0),
            VisibleRow::Session(3),
            VisibleRow::Session(4),
        ];

        assert_eq!(first_session_row_position(&rows), 1);
        assert_eq!(first_session_row_position(&[VisibleRow::Group(0)]), 0);
        assert_eq!(first_session_row_position(&[]), 0);
    }

    #[test]
    fn group_selection_counts_live_sessions_only() {
        let sessions = vec![
            session("api", 0, false),
            session("database", 0, false),
            session("scratch", 0, false),
        ];
        let groups = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: true,
                sessions: vec![
                    "api".to_string(),
                    "database".to_string(),
                    "stale".to_string(),
                ],
            }],
        };
        let selected = BTreeSet::from(["api".to_string(), "stale".to_string()]);

        assert_eq!(
            selected_count_for_group(&sessions, &groups, &selected, 0),
            (1, 2)
        );
        assert_eq!(
            selected_count_for_group(&sessions, &groups, &selected, 1),
            (0, 1)
        );
    }

    #[test]
    fn toggling_group_selects_all_then_clears_all_live_members() {
        let sessions = vec![
            session("api", 0, false),
            session("database", 0, false),
            session("scratch", 0, false),
        ];
        let groups = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: false,
                sessions: vec!["api".to_string(), "database".to_string()],
            }],
        };
        let mut selected = BTreeSet::from(["api".to_string()]);

        toggle_selection_for_group(&mut selected, &sessions, &groups, 0);
        assert_eq!(
            selected,
            BTreeSet::from(["api".to_string(), "database".to_string()])
        );

        toggle_selection_for_group(&mut selected, &sessions, &groups, 0);
        assert!(selected.is_empty());
    }

    #[test]
    fn toggling_visible_sessions_respects_search_results() {
        let sessions = vec![
            session("api", 0, false),
            session("database", 0, false),
            session("scratch", 0, false),
        ];
        let groups = GroupState::default();
        let rows = build_visible_rows(&sessions, &groups, "a", false);
        let mut selected = BTreeSet::new();

        toggle_selection_for_rows(&mut selected, &sessions, &rows);

        assert_eq!(
            selected,
            BTreeSet::from([
                "api".to_string(),
                "database".to_string(),
                "scratch".to_string()
            ])
        );
    }

    #[test]
    fn stale_selected_sessions_are_pruned_after_reload() {
        let sessions = vec![session("api", 0, false), session("database", 0, false)];
        let mut selected = BTreeSet::from(["api".to_string(), "gone".to_string()]);

        prune_selected_sessions(&mut selected, &sessions);

        assert_eq!(selected, BTreeSet::from(["api".to_string()]));
    }

    #[test]
    fn bulk_pin_pins_when_any_selected_session_is_unpinned() {
        let sessions = vec![
            session("api", 0, true),
            session("database", 0, false),
            session("scratch", 0, false),
        ];

        assert!(bulk_pin_target_state(
            &sessions,
            &BTreeSet::from(["api".to_string(), "database".to_string()])
        ));
        assert!(!bulk_pin_target_state(
            &sessions,
            &BTreeSet::from(["api".to_string()])
        ));
    }

    #[test]
    fn shortcut_help_includes_question_mark_entry() {
        assert!(SHORTCUTS.iter().any(|(key, _)| *key == "?"));
        assert_eq!(help_popup_lines(0, 4).first().unwrap(), "Shortcuts");
    }

    #[test]
    fn shortcut_help_navigation_clamps_to_bounds() {
        assert_eq!(next_help_index(0, -1), 0);
        assert_eq!(next_help_index(0, 1), 1);
        assert_eq!(next_help_index(SHORTCUTS.len() - 1, 1), SHORTCUTS.len() - 1);
    }

    #[test]
    fn show_help_opens_and_pages_shortcut_prompt() {
        let mut app = app_with_sessions(vec![session("api", 0, false)]);

        app.show_help();
        assert!(matches!(app.prompt, Some(Prompt::Help { index: 0 })));

        app.handle_prompt(b'j').unwrap();
        assert!(matches!(app.prompt, Some(Prompt::Help { index: 1 })));

        app.handle_prompt(0x1b).unwrap();
        assert!(app.prompt.is_none());
    }

    #[test]
    fn empty_name_prompt_backspace_cancels_like_escape() {
        let mut app = app_with_sessions(vec![session("api", 0, false)]);
        app.begin_create_group();

        app.handle_prompt(0x7f).unwrap();

        assert!(app.prompt.is_none());
        assert_eq!(app.status, "Cancelled");
    }

    #[test]
    fn non_empty_name_prompt_backspace_deletes_character() {
        let mut app = app_with_sessions(vec![session("api", 0, false)]);
        app.prompt = Some(Prompt::Name {
            label: "NEW GROUP",
            value: "Work".to_string(),
            action: NameAction::Create,
        });

        app.handle_prompt(0x7f).unwrap();

        assert!(matches!(
            app.prompt,
            Some(Prompt::Name {
                ref value,
                ..
            }) if value == "Wor"
        ));
    }

    #[test]
    fn shortcut_help_popup_marks_current_entry() {
        let lines = help_popup_lines(1, 3);

        assert!(lines.iter().any(|line| line.starts_with("> g/G")));
        assert!(lines.last().unwrap().contains("Esc close"));
    }

    #[test]
    fn shortcut_help_popup_uses_nearly_full_height() {
        assert_eq!(help_popup_height(24), 22);
        assert_eq!(help_popup_height(4), 5);

        let lines = help_popup_lines(0, help_popup_height(24).saturating_sub(4));
        let shortcut_rows = lines
            .iter()
            .filter(|line| line.starts_with("> ") || line.starts_with("  "))
            .count();
        assert!(shortcut_rows > 12);
    }

    #[test]
    fn move_popup_lists_groups_and_special_targets() {
        let groups = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: false,
                sessions: Vec::new(),
            }],
        };

        let lines = move_popup_lines(&groups, 1, 2);

        assert_eq!(lines[0], "Move 2 sessions to");
        assert!(lines.iter().any(|line| line == "  Work"));
        assert!(lines.iter().any(|line| line == "> Ungrouped"));
        assert!(lines.iter().any(|line| line == "  New group..."));
    }

    #[test]
    fn mode_line_describes_current_mode() {
        assert!(mode_line(None, false, "", true, 0).starts_with("MODE normal"));
        assert!(mode_line(None, false, "", true, 2).starts_with("MODE selection"));
        assert_eq!(
            mode_line(None, true, "api", false, 0),
            "MODE search: /api  no matches"
        );
        assert!(
            mode_line(Some(&Prompt::Help { index: 0 }), false, "", true, 0)
                .starts_with("MODE help")
        );
        assert!(
            mode_line(
                Some(&Prompt::Move {
                    session_names: vec!["api".to_string(), "db".to_string()],
                    choice: 0,
                }),
                false,
                "",
                true,
                2,
            )
            .starts_with("MODE move: 2 sessions")
        );
        assert_eq!(
            mode_line(
                Some(&Prompt::Name {
                    label: "NEW GROUP",
                    value: "Work".to_string(),
                    action: NameAction::Create,
                }),
                false,
                "",
                true,
                0,
            ),
            "MODE input: NEW GROUP: Work_"
        );
    }

    #[test]
    fn session_row_hides_checkbox_until_selection_mode() {
        let api = session("api", 0, false);

        let normal = session_row_line(">", &api, &BTreeSet::new(), 16, 4);
        assert!(!normal.contains("[ ]"));
        assert!(!normal.contains("[x]"));

        let selected = BTreeSet::from(["api".to_string()]);
        let selected_line = session_row_line(">", &api, &selected, 16, 4);
        assert!(selected_line.contains("[x]"));

        let db = session("db", 0, false);
        let unselected_line = session_row_line(" ", &db, &selected, 16, 4);
        assert!(unselected_line.contains("[ ]"));
    }

    #[test]
    fn sgr_mouse_parser_decodes_press_and_release() {
        let press = parse_mouse_escape(b"\x1b[<0;12;7M").unwrap();
        assert_eq!(press.button, 0);
        assert_eq!(press.col, 12);
        assert_eq!(press.row, 7);
        assert!(press.pressed);

        let release = parse_mouse_escape(b"\x1b[<0;12;7m").unwrap();
        assert!(!release.pressed);
    }

    #[test]
    fn sgr_mouse_parser_rejects_malformed_input() {
        assert!(parse_mouse_escape(b"\x1b[M !").is_none());
        assert!(parse_mouse_escape(b"\x1b[<0;12M").is_none());
        assert!(parse_mouse_escape(b"\x1b[<x;12;7M").is_none());
    }

    #[test]
    fn legacy_mouse_parser_decodes_press() {
        let event = parse_mouse_escape(b"\x1b[M *'").unwrap();

        assert_eq!(event.button, 0);
        assert_eq!(event.col, 10);
        assert_eq!(event.row, 7);
        assert!(event.pressed);
    }

    #[test]
    fn mouse_wheel_buttons_map_to_scroll_delta() {
        let down = parse_mouse_escape(b"\x1b[<65;12;7M").unwrap();
        let up = parse_mouse_escape(b"\x1b[<64;12;7M").unwrap();

        assert_eq!(mouse_wheel_delta(down.button), Some(3));
        assert_eq!(mouse_wheel_delta(up.button), Some(-3));
        assert_eq!(mouse_wheel_delta(0), None);
    }

    #[test]
    fn mouse_row_maps_into_scrolled_visible_list() {
        assert_eq!(visible_index_for_mouse_row(8, 8, 4, 3, 9), Some(3));
        assert_eq!(visible_index_for_mouse_row(11, 8, 4, 3, 9), Some(6));
    }

    #[test]
    fn mouse_row_outside_rendered_sessions_is_ignored() {
        assert_eq!(visible_index_for_mouse_row(7, 8, 4, 0, 9), None);
        assert_eq!(visible_index_for_mouse_row(12, 8, 4, 0, 9), None);
        assert_eq!(visible_index_for_mouse_row(10, 8, 4, 0, 2), None);
    }

    #[test]
    fn mouse_wheel_scrolls_visible_rows() {
        let sessions = (1..=20)
            .map(|index| session(&format!("s{index:02}"), 0, false))
            .collect::<Vec<_>>();
        let mut app = app_with_sessions(sessions);
        app.rows = 10;

        let activated = app
            .handle_mouse(MouseEvent {
                button: 65,
                col: 5,
                row: app.layout().list_row_start + 1,
                pressed: true,
            })
            .unwrap();

        assert!(!activated);
        assert_eq!(app.top, 3);
        assert_eq!(app.selected, 3);
    }

    #[test]
    fn right_click_pins_session_under_cursor() {
        let pin_file = temp_state_file("pins");
        let mut app = app_with_sessions(vec![session("api", 0, false)]);
        app.pin_file = pin_file.clone();
        let layout = app.layout();

        app.handle_mouse(MouseEvent {
            button: 2,
            col: 5,
            row: layout.list_row_start + 1,
            pressed: true,
        })
        .unwrap();

        assert!(
            app.sessions
                .iter()
                .any(|session| session.name == "api" && session.pinned)
        );
        assert_eq!(fs::read_to_string(&pin_file).unwrap(), "api\n");
        let _ = fs::remove_file(pin_file);
    }

    #[test]
    fn left_drag_reorders_pinned_session_up() {
        let pin_file = temp_state_file("pins");
        let mut app =
            app_with_sessions(vec![session("api", 0, true), session("database", 0, true)]);
        app.pin_file = pin_file.clone();
        write_pinned_names(&pin_file, &["api".to_string(), "database".to_string()]).unwrap();
        let layout = app.layout();

        app.handle_mouse(MouseEvent {
            button: 0,
            col: 5,
            row: layout.list_row_start + 2,
            pressed: true,
        })
        .unwrap();
        app.handle_mouse(MouseEvent {
            button: 32,
            col: 5,
            row: layout.list_row_start + 1,
            pressed: true,
        })
        .unwrap();

        let names = app
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["database", "api"]);
        assert_eq!(fs::read_to_string(&pin_file).unwrap(), "database\napi\n");
        let _ = fs::remove_file(pin_file);
    }

    #[test]
    fn left_click_moves_cursor_to_visible_session() {
        let mut app = app_with_sessions(vec![
            session("api", 0, false),
            session("database", 0, false),
        ]);
        let layout = app.layout();

        let activated = app
            .handle_mouse(MouseEvent {
                button: 0,
                col: 10,
                row: layout.list_row_start + 2,
                pressed: true,
            })
            .unwrap();

        assert!(!activated);
        assert_eq!(app.selected_row(), Some(VisibleRow::Session(1)));
    }

    #[test]
    fn checkbox_click_toggles_session_in_selection_mode() {
        let mut app = app_with_sessions(vec![session("api", 0, false)]);
        app.selected_sessions.insert("api".to_string());
        let layout = app.layout();

        app.handle_mouse(MouseEvent {
            button: 0,
            col: layout.table_col + 2,
            row: layout.list_row_start + 1,
            pressed: true,
        })
        .unwrap();

        assert!(app.selected_sessions.is_empty());
    }

    #[test]
    fn double_click_toggles_group() {
        let mut app = app_with_sessions(vec![session("api", 0, false)]);
        let layout = app.layout();
        let click = MouseEvent {
            button: 0,
            col: 4,
            row: layout.list_row_start,
            pressed: true,
        };

        app.handle_mouse(click).unwrap();
        app.handle_mouse(click).unwrap();

        assert!(app.ungrouped_collapsed);
    }

    #[test]
    fn popup_ignores_mouse_clicks() {
        let mut app = app_with_sessions(vec![session("api", 0, false)]);
        app.show_help();
        let layout = app.layout();

        app.handle_mouse(MouseEvent {
            button: 0,
            col: 10,
            row: layout.list_row_start + 1,
            pressed: true,
        })
        .unwrap();

        assert_eq!(app.selected_row(), Some(VisibleRow::Group(0)));
        assert!(matches!(app.prompt, Some(Prompt::Help { .. })));
    }
}
