use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
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
        let pinned_names = read_pinned_names(&pin_file);
        let (rows, cols) = terminal_size().unwrap_or((24, 80));
        let current_session = tmux_output(
            &tmux_socket_name,
            &tmux_socket_path,
            &["display-message", "-p", "#S"],
        )?
        .trim()
        .to_string();

        let raw_sessions = tmux_output(
            &tmux_socket_name,
            &tmux_socket_path,
            &[
                "list-sessions",
                "-F",
                "#{session_name}\t#{session_activity}",
            ],
        )?;

        let mut sessions = parse_sessions(&raw_sessions, &current_session, &pinned_names)?;
        arrange_sessions(&mut sessions, &pinned_names);
        write_pinned_names(&pin_file, &pinned_names_from_sessions(&sessions))?;

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
        })
    }

    fn run(&mut self) -> AppResult<()> {
        self.ensure_visible();
        self.render_full()?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];

        loop {
            let previous_selected = self.selected;
            let previous_top = self.top;
            let previous_status = self.status.clone();

            stdin.read_exact(&mut byte)?;

            match byte[0] {
                b'j' => self.move_down(),
                b'k' => self.move_up(),
                b'g' => self.jump_first(),
                b'G' => self.jump_last(),
                b'p' => self.toggle_pin()?,
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
            self.render_incremental(previous_selected, previous_top, &previous_status)?;
        }

        Ok(())
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.sessions.len() {
            self.selected += 1;
        }
    }

    fn jump_first(&mut self) {
        self.selected = 0;
    }

    fn jump_last(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = self.sessions.len() - 1;
        }
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
        self.viewport_height().min(self.sessions.len().max(1))
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
        let _ = stdout;
        Ok(())
    }

    fn render_list_area(&self, stdout: &mut impl Write) -> AppResult<()> {
        for visible_row in 0..self.visible_list_height() {
            let session_index = self.top + visible_row;
            self.render_session_row_at(stdout, visible_row, session_index)?;
        }
        Ok(())
    }

    fn render_session_row(&self, stdout: &mut impl Write, session_index: usize) -> AppResult<()> {
        if session_index < self.top || session_index >= self.top + self.viewport_height() {
            return Ok(());
        }

        let visible_row = session_index - self.top;
        self.render_session_row_at(stdout, visible_row, session_index)
    }

    fn render_session_row_at(
        &self,
        stdout: &mut impl Write,
        visible_row: usize,
        session_index: usize,
    ) -> AppResult<()> {
        let layout = self.layout();
        let row = layout.list_row_start + visible_row;

        if let Some(session) = self.sessions.get(session_index) {
            let pointer = if session_index == self.selected { ">" } else { " " };
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
            write_at(stdout, row, layout.table_col, &line, session_index == self.selected)?;
        } else {
            write_row(stdout, row, "", false)?;
        }
        Ok(())
    }

    fn render_footer(&self, stdout: &mut impl Write) -> AppResult<()> {
        let layout = self.layout();
        write_row(stdout, layout.blank_row, "", false)?;
        if self.status.is_empty() {
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
        write!(stdout, "\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;

        Ok(Self { fd, saved_state })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = set_termios(self.fd, &self.saved_state);
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
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
        Session, arrange_sessions, format_relative_activity, pinned_names_from_sessions,
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
        assert_eq!(fs::read_to_string(&pin_file).unwrap(), "config\ndashboard\n");
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
}
