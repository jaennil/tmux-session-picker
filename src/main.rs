use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Session {
    name: String,
    windows: usize,
    attached_clients: usize,
    is_current: bool,
}

struct App {
    sessions: Vec<Session>,
    selected: usize,
    top: usize,
    rows: usize,
    cols: usize,
    order_file: PathBuf,
    tmux_socket_name: Option<String>,
    tmux_socket_path: Option<String>,
    status: String,
}

struct Layout {
    title_row: usize,
    title_col: usize,
    header_row: usize,
    table_col: usize,
    index_width: usize,
    name_width: usize,
    list_row_start: usize,
    blank_row: usize,
    status_row: usize,
}

impl App {
    fn new() -> AppResult<Self> {
        let tmux_socket_name = env::var("TMUX_SOCKET_NAME").ok();
        let tmux_socket_path = env::var("TMUX_SOCKET_PATH").ok();
        let order_file = order_file_path();
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
                "#{session_name}\t#{session_windows}\t#{session_attached}",
            ],
        )?;

        let mut sessions = parse_sessions(&raw_sessions, &current_session)?;
        sync_sessions(&mut sessions, &order_file)?;

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
            order_file,
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
        if self.selected == 0 {
            self.status = "Session is already first".to_string();
            return Ok(());
        }

        self.sessions.swap(self.selected, self.selected - 1);
        self.selected -= 1;
        self.write_order()?;
        self.status = format!("Moved {} up", self.current().name);
        Ok(())
    }

    fn reorder_down(&mut self) -> AppResult<()> {
        if self.selected + 1 >= self.sessions.len() {
            self.status = "Session is already last".to_string();
            return Ok(());
        }

        self.sessions.swap(self.selected, self.selected + 1);
        self.selected += 1;
        self.write_order()?;
        self.status = format!("Moved {} down", self.current().name);
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
        let index_width = self.sessions.len().to_string().len().max(1);
        let max_name_width = self
            .sessions
            .iter()
            .map(|session| session.name.chars().count())
            .max()
            .unwrap_or(8);
        let min_name_width = 16;
        let fixed_width = 2 + index_width + 2 + 2 + 3 + 2 + 3 + 2 + 3;
        let max_name_width = self
            .cols
            .saturating_sub(fixed_width)
            .clamp(min_name_width, max_name_width.max(min_name_width));
        let name_width = max_name_width.max(min_name_width).min(max_name_width);
        let table_width = 2 + index_width + 2 + name_width + 2 + 3 + 2 + 3 + 2 + 3;
        let list_height = self.visible_list_height();
        let content_height = list_height + 5;
        let top_padding = self.rows.saturating_sub(content_height);
        let _table_width = table_width;
        let table_col = 1;
        let title_col = 1;
        let title_row = top_padding + 1;
        let header_row = title_row + 2;
        let list_row_start = header_row + 1;
        let blank_row = list_row_start + list_height;
        let status_row = blank_row + 1;

        Layout {
            title_row,
            title_col,
            header_row,
            table_col,
            index_width,
            name_width,
            list_row_start,
            blank_row,
            status_row,
        }
    }

    fn title_text(&self) -> String {
        format!(
            "Tmux sessions | current {} | {} total",
            self.sessions
                .iter()
                .find(|session| session.is_current)
                .map(|session| session.name.as_str())
                .unwrap_or("-"),
            self.sessions.len()
        )
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
        let title = self.title_text();
        write_at(stdout, layout.title_row, layout.title_col, &truncate(&title, self.cols), false)?;
        write_row(stdout, layout.title_row + 1, "", false)?;

        let header = format!(
            "  {:>index_width$}  {:<name_width$}  {:>3}  {:>3}  {:^3}",
            "#",
            "session",
            "win",
            "cli",
            "cur",
            index_width = layout.index_width,
            name_width = layout.name_width,
        );
        write_at(
            stdout,
            layout.header_row,
            layout.table_col,
            &truncate(&header, self.cols.saturating_sub(layout.table_col.saturating_sub(1))),
            false,
        )?;
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
            let current = if session.is_current { "*" } else { "" };
            let line = format!(
                "{} {:>index_width$}  {:<name_width$}  {:>3}  {:>3}  {:^3}",
                pointer,
                session_index + 1,
                session.name,
                session.windows,
                session.attached_clients,
                current,
                index_width = layout.index_width,
                name_width = layout.name_width,
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

    fn write_order(&self) -> AppResult<()> {
        if let Some(parent) = self.order_file.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = self
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        fs::write(&self.order_file, format!("{contents}\n"))?;
        Ok(())
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

fn order_file_path() -> PathBuf {
    if let Ok(path) = env::var("TMUX_SESSION_ORDER_FILE") {
        return PathBuf::from(path);
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/tmux/session-order")
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

fn parse_sessions(raw: &str, current_session: &str) -> AppResult<Vec<Session>> {
    let mut sessions = Vec::new();

    for line in raw.lines() {
        let mut parts = line.split('\t');
        let name = parts.next().unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }

        let windows = parts
            .next()
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|err| format!("invalid windows count for {name}: {err}"))?;
        let attached_clients = parts
            .next()
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|err| format!("invalid attached client count for {name}: {err}"))?;

        sessions.push(Session {
            name: name.clone(),
            windows,
            attached_clients,
            is_current: name == current_session,
        });
    }

    if sessions.is_empty() {
        return Err("no tmux sessions found".into());
    }

    Ok(sessions)
}

fn sync_sessions(sessions: &mut Vec<Session>, order_file: &PathBuf) -> AppResult<()> {
    let mut ordered = Vec::with_capacity(sessions.len());
    let mut remaining = sessions.clone();
    let original_names = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect::<Vec<_>>();

    if let Ok(contents) = fs::read_to_string(order_file) {
        for line in contents.lines().filter(|line| !line.is_empty()) {
            if let Some(index) = remaining.iter().position(|session| session.name == line) {
                ordered.push(remaining.remove(index));
            }
        }
    }

    remaining.sort_by(|a, b| a.name.cmp(&b.name));
    ordered.extend(remaining);
    *sessions = ordered;

    if let Some(parent) = order_file.parent() {
        fs::create_dir_all(parent)?;
    }

    let new_names = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect::<Vec<_>>();

    if new_names != original_names || !order_file.exists() {
        let contents = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(order_file, format!("{contents}\n"))?;
    }
    Ok(())
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
    use super::{Session, sync_sessions};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn session(name: &str) -> Session {
        Session {
            name: name.to_string(),
            windows: 1,
            attached_clients: 0,
            is_current: false,
        }
    }

    fn temp_order_file() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tmux-session-picker-test-{nanos}.order"))
    }

    #[test]
    fn sync_sessions_keeps_existing_order_and_appends_new_names() {
        let order_file = temp_order_file();
        fs::write(&order_file, "dashboard\nconfig\n").unwrap();

        let mut sessions = vec![session("doc"), session("config"), session("dashboard")];
        sync_sessions(&mut sessions, &order_file).unwrap();

        let names = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["dashboard", "config", "doc"]);

        let _ = fs::remove_file(order_file);
    }

    #[test]
    fn sync_sessions_discards_missing_names_from_order_file() {
        let order_file = temp_order_file();
        fs::write(&order_file, "missing\nconfig\n").unwrap();

        let mut sessions = vec![session("dashboard"), session("config")];
        sync_sessions(&mut sessions, &order_file).unwrap();

        let names = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["config", "dashboard"]);

        let _ = fs::remove_file(order_file);
    }
}
