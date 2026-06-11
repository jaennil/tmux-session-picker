use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Session {
    name: String,
    last_activity: u64,
    pinned: bool,
    is_current: bool,
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

fn session_index_for_mouse_row(
    mouse_row: usize,
    list_row_start: usize,
    list_height: usize,
    top: usize,
    matches: &[usize],
) -> Option<usize> {
    let offset = mouse_row.checked_sub(list_row_start)?;
    if offset >= list_height {
        return None;
    }
    matches.get(top.checked_add(offset)?).copied()
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

struct App {
    sessions: Vec<Session>,
    selected: usize,
    top: usize,
    rows: usize,
    cols: usize,
    pin_file: PathBuf,
    tmux_socket_name: Option<String>,
    tmux_socket_path: Option<String>,
    status: String,
    query: String,
    searching: bool,
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
        let (rows, cols) = terminal_size().unwrap_or((24, 80));
        let sessions = load_sessions(&pin_file, &tmux_socket_name, &tmux_socket_path)?;

        let selected = sessions
            .iter()
            .position(|session| session.is_current)
            .unwrap_or(0);

        Ok(Self {
            sessions,
            selected,
            top: 0,
            rows,
            cols,
            pin_file,
            tmux_socket_name,
            tmux_socket_path,
            status: String::new(),
            query: String::new(),
            searching: false,
        })
    }

    fn run(&mut self) -> AppResult<()> {
        self.ensure_visible();
        self.render_full()?;

        let stdin_fd = io::stdin().as_raw_fd();

        loop {
            let previous_selected = self.selected;
            let previous_top = self.top;
            let previous_status = self.status.clone();
            let mut redraw_full = false;

            let input = read_input_event(stdin_fd)?;
            if let InputEvent::Mouse(mouse) = input {
                if self.handle_mouse(mouse) {
                    self.switch_selected()?;
                    break;
                }
                self.ensure_visible();
                self.render_incremental(previous_selected, previous_top, &previous_status)?;
                continue;
            }
            if let InputEvent::Ignore = input {
                continue;
            }
            let InputEvent::Key(key) = input else {
                unreachable!();
            };

            if self.searching {
                match key {
                    b'\r' | b'\n' => {
                        if self.has_matches() {
                            self.switch_selected()?;
                            break;
                        }
                    }
                    0x1b => {
                        self.query.clear();
                        self.searching = false;
                    }
                    0x03 => break,
                    0x7f | 0x08 => {
                        self.query.pop();
                        self.select_first_match();
                    }
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
                    redraw_full = true;
                }
                b'p' => {
                    self.toggle_pin()?;
                    redraw_full = true;
                }
                b'x' => {
                    self.kill_selected()?;
                    redraw_full = true;
                }
                b'J' => self.reorder_down()?,
                b'K' => self.reorder_up()?,
                b'\r' | b'\n' => {
                    self.switch_selected()?;
                    break;
                }
                b'q' | 0x1b | 0x03 => break,
                _ => {}
            }

            self.ensure_visible();
            if redraw_full {
                self.render_full()?;
            } else {
                self.render_incremental(previous_selected, previous_top, &previous_status)?;
            }
        }

        Ok(())
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let left_button = event.button & 0b11 == 0 && event.button & 0b100_0000 == 0;
        if !event.pressed || !left_button {
            return false;
        }

        let layout = self.layout();
        let matches = self.matching_indices();
        if let Some(session_index) = session_index_for_mouse_row(
            event.row,
            layout.list_row_start,
            self.visible_list_height(),
            self.top,
            &matches,
        ) {
            self.selected = session_index;
            return true;
        }
        false
    }

    fn move_up(&mut self) {
        let matches = self.matching_indices();
        if let Some(position) = matches.iter().position(|index| *index == self.selected)
            && position > 0
        {
            self.selected = matches[position - 1];
        }
    }

    fn move_down(&mut self) {
        let matches = self.matching_indices();
        if let Some(position) = matches.iter().position(|index| *index == self.selected)
            && position + 1 < matches.len()
        {
            self.selected = matches[position + 1];
        }
    }

    fn jump_first(&mut self) {
        if let Some(index) = self.matching_indices().first() {
            self.selected = *index;
        }
    }

    fn jump_last(&mut self) {
        if let Some(index) = self.matching_indices().last() {
            self.selected = *index;
        }
    }

    fn matching_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                session_name_matches(&session.name, &self.query).then_some(index)
            })
            .collect()
    }

    fn has_matches(&self) -> bool {
        self.sessions
            .iter()
            .any(|session| session_name_matches(&session.name, &self.query))
    }

    fn select_first_match(&mut self) {
        let matches = self.matching_indices();
        if !matches.contains(&self.selected)
            && let Some(index) = matches.first()
        {
            self.selected = *index;
        }
        self.top = 0;
    }

    fn reorder_up(&mut self) -> AppResult<()> {
        if !self.current().pinned {
            self.status = "Pin session first".to_string();
            return Ok(());
        }

        if self.selected == 0 {
            self.status = "Pinned session is already first".to_string();
            return Ok(());
        }

        self.sessions.swap(self.selected, self.selected - 1);
        self.selected -= 1;
        self.write_pins()?;
        self.status = format!("Moved {} up", self.current().name);
        Ok(())
    }

    fn reorder_down(&mut self) -> AppResult<()> {
        if !self.current().pinned {
            self.status = "Pin session first".to_string();
            return Ok(());
        }

        let pinned_count = self.pinned_count();
        if self.selected + 1 >= pinned_count {
            self.status = "Pinned session is already last".to_string();
            return Ok(());
        }

        self.sessions.swap(self.selected, self.selected + 1);
        self.selected += 1;
        self.write_pins()?;
        self.status = format!("Moved {} down", self.current().name);
        Ok(())
    }

    fn toggle_pin(&mut self) -> AppResult<()> {
        let current_name = self.current().name.clone();
        let was_pinned = self.current().pinned;

        if let Some(session) = self.sessions.get_mut(self.selected) {
            session.pinned = !session.pinned;
        }

        let pinned_names = pinned_names_from_sessions(&self.sessions);
        arrange_sessions(&mut self.sessions, &pinned_names);
        self.selected = self
            .sessions
            .iter()
            .position(|session| session.name == current_name)
            .unwrap_or(0);
        self.write_pins()?;
        self.status = if was_pinned {
            format!("Unpinned {current_name}")
        } else {
            format!("Pinned {current_name}")
        };
        Ok(())
    }

    fn kill_selected(&mut self) -> AppResult<()> {
        if self.current().is_current {
            self.status = "Cannot kill current session".to_string();
            return Ok(());
        }

        let session_name = self.current().name.clone();
        let next_selected_name = self
            .sessions
            .get(self.selected + 1)
            .or_else(|| {
                self.selected
                    .checked_sub(1)
                    .and_then(|index| self.sessions.get(index))
            })
            .map(|session| session.name.clone());
        tmux_status(
            &self.tmux_socket_name,
            &self.tmux_socket_path,
            &["kill-session", "-t", &session_name],
        )?;

        self.reload_sessions(next_selected_name.as_deref())?;
        self.status = format!("Killed {session_name}");
        Ok(())
    }

    fn switch_selected(&mut self) -> AppResult<()> {
        let session_name = self.current().name.clone();
        tmux_status(
            &self.tmux_socket_name,
            &self.tmux_socket_path,
            &["switch-client", "-t", &session_name],
        )?;
        Ok(())
    }

    fn current(&self) -> &Session {
        &self.sessions[self.selected]
    }

    fn pinned_count(&self) -> usize {
        self.sessions
            .iter()
            .take_while(|session| session.pinned)
            .count()
    }

    fn ensure_visible(&mut self) {
        let viewport = self.viewport_height();
        let Some(position) = self
            .matching_indices()
            .iter()
            .position(|index| *index == self.selected)
        else {
            self.top = 0;
            return;
        };

        if position < self.top {
            self.top = position;
        } else if position >= self.top + viewport {
            self.top = position + 1 - viewport;
        }
    }

    fn viewport_height(&self) -> usize {
        let reserved_rows = 4;
        self.rows.saturating_sub(reserved_rows).max(1)
    }

    fn visible_list_height(&self) -> usize {
        self.viewport_height()
            .min(self.matching_indices().len().max(1))
    }

    fn layout(&self) -> Layout {
        let max_name_width = self
            .sessions
            .iter()
            .map(|session| session.name.chars().count())
            .max()
            .unwrap_or(8);
        let min_name_width = 16;
        let activity_width = 4;
        let fixed_width = 3 + min_name_width + 2 + activity_width + 2 + 3;
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
        stdout.flush()?;
        Ok(())
    }

    fn render_incremental(
        &self,
        previous_selected: usize,
        previous_top: usize,
        previous_status: &str,
    ) -> AppResult<()> {
        let mut stdout = io::stdout().lock();

        if previous_top != self.top {
            self.render_list_area(&mut stdout)?;
            self.render_footer(&mut stdout)?;
        } else {
            self.render_session_row(&mut stdout, previous_selected)?;
            if previous_selected != self.selected {
                self.render_session_row(&mut stdout, self.selected)?;
            }
            if previous_status != self.status {
                self.render_footer(&mut stdout)?;
            }
        }

        stdout.flush()?;
        Ok(())
    }

    fn render_static(&self, stdout: &mut impl Write) -> AppResult<()> {
        let layout = self.layout();
        if layout.list_row_start > 1 {
            let row = layout.list_row_start - 1;
            let label = if self.searching {
                let suffix = if self.has_matches() {
                    ""
                } else {
                    "  No matching sessions"
                };
                format!("Search: /{}{suffix}", self.query)
            } else {
                "Press / to search sessions".to_string()
            };
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
        let matches = self.matching_indices();
        for visible_row in 0..self.visible_list_height() {
            let session_index = matches.get(self.top + visible_row).copied();
            self.render_session_row_at(stdout, visible_row, session_index)?;
        }
        Ok(())
    }

    fn render_session_row(&self, stdout: &mut impl Write, session_index: usize) -> AppResult<()> {
        let Some(position) = self
            .matching_indices()
            .iter()
            .position(|index| *index == session_index)
        else {
            return Ok(());
        };
        if position < self.top || position >= self.top + self.viewport_height() {
            return Ok(());
        }

        let visible_row = position - self.top;
        self.render_session_row_at(stdout, visible_row, Some(session_index))
    }

    fn render_session_row_at(
        &self,
        stdout: &mut impl Write,
        visible_row: usize,
        session_index: Option<usize>,
    ) -> AppResult<()> {
        let layout = self.layout();
        let row = layout.list_row_start + visible_row;

        if let Some((session_index, session)) =
            session_index.and_then(|index| self.sessions.get(index).map(|session| (index, session)))
        {
            let pointer = if session_index == self.selected {
                ">"
            } else {
                " "
            };
            let pin = if session.pinned { "!" } else { " " };
            let current = if session.is_current { "*" } else { "" };
            let last = format_relative_activity(session.last_activity);
            let line = format!(
                "{} {:<name_width$}  {:>activity_width$}  {:^3} {}",
                pointer,
                session.name,
                last,
                current,
                pin,
                name_width = layout.name_width,
                activity_width = layout.activity_width,
            );
            let line = truncate(
                &line,
                self.cols.saturating_sub(layout.table_col.saturating_sub(1)),
            );
            write_at(
                stdout,
                row,
                layout.table_col,
                &line,
                session_index == self.selected,
            )?;
        } else {
            write_row(stdout, row, "", false)?;
        }
        Ok(())
    }

    fn render_footer(&self, stdout: &mut impl Write) -> AppResult<()> {
        let layout = self.layout();
        write_row(stdout, layout.blank_row, "", false)?;
        if self.searching {
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

    fn write_pins(&self) -> AppResult<()> {
        write_pinned_names(&self.pin_file, &pinned_names_from_sessions(&self.sessions))
    }

    fn reload_sessions(&mut self, preferred_name: Option<&str>) -> AppResult<()> {
        let previous_selected = self.selected;
        self.sessions = load_sessions(
            &self.pin_file,
            &self.tmux_socket_name,
            &self.tmux_socket_path,
        )?;
        self.selected = preferred_name
            .and_then(|name| {
                self.sessions
                    .iter()
                    .position(|session| session.name == name)
            })
            .or_else(|| self.sessions.iter().position(|session| session.is_current))
            .unwrap_or_else(|| previous_selected.min(self.sessions.len().saturating_sub(1)));
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
        write!(stdout, "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h")?;
        stdout.flush()?;

        Ok(Self { fd, saved_state })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = set_termios(self.fd, &self.saved_state);
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l");
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
        App, MouseEvent, Session, arrange_sessions, format_relative_activity, parse_mouse_escape,
        pinned_names_from_sessions, session_index_for_mouse_row, session_name_matches,
        write_pinned_names,
    };
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
            selected: 0,
            top: 0,
            rows: 24,
            cols: 80,
            pin_file: PathBuf::new(),
            tmux_socket_name: None,
            tmux_socket_path: None,
            status: String::new(),
            query: String::new(),
            searching: false,
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
    fn sgr_mouse_parser_decodes_left_click() {
        let event = parse_mouse_escape(b"\x1b[<0;12;7M").unwrap();

        assert_eq!(event.button, 0);
        assert_eq!(event.col, 12);
        assert_eq!(event.row, 7);
        assert!(event.pressed);
    }

    #[test]
    fn legacy_mouse_parser_decodes_left_click() {
        let event = parse_mouse_escape(b"\x1b[M *'").unwrap();

        assert_eq!(event.button, 0);
        assert_eq!(event.col, 10);
        assert_eq!(event.row, 7);
        assert!(event.pressed);
    }

    #[test]
    fn sgr_mouse_parser_rejects_incomplete_sequence() {
        assert!(parse_mouse_escape(b"\x1b").is_none());
        assert!(parse_mouse_escape(b"\x1b[<0;12M").is_none());
    }

    #[test]
    fn mouse_row_selects_visible_scrolled_session() {
        let matches = vec![2, 4, 7, 9, 11];

        assert_eq!(session_index_for_mouse_row(8, 8, 3, 1, &matches), Some(4));
        assert_eq!(session_index_for_mouse_row(10, 8, 3, 1, &matches), Some(9));
    }

    #[test]
    fn mouse_row_outside_visible_sessions_is_ignored() {
        let matches = vec![2, 4];

        assert_eq!(session_index_for_mouse_row(7, 8, 3, 0, &matches), None);
        assert_eq!(session_index_for_mouse_row(11, 8, 3, 0, &matches), None);
        assert_eq!(session_index_for_mouse_row(10, 8, 3, 0, &matches), None);
    }

    #[test]
    fn mouse_click_selects_and_activates_session_under_cursor() {
        let mut app = app_with_sessions(vec![
            session("api", 0, false),
            session("database", 0, false),
        ]);
        let layout = app.layout();

        let activated = app.handle_mouse(MouseEvent {
            button: 0,
            col: 8,
            row: layout.list_row_start + 1,
            pressed: true,
        });

        assert!(activated);
        assert_eq!(app.selected, 1);
    }
}
