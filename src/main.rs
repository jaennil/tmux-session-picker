use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
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
            status: "j/k move  J/K reorder  g/G first/last  Enter switch  q quit".to_string(),
        })
    }

    fn run(&mut self) -> AppResult<()> {
        self.ensure_visible();
        self.render()?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];

        loop {
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
            self.render()?;
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
        self.rows.saturating_sub(4).max(1)
    }

    fn render(&self) -> AppResult<()> {
        let mut stdout = io::stdout().lock();
        write!(stdout, "\x1b[2J\x1b[H")?;

        let title = format!(
            "Tmux Sessions  current: {}  total: {}",
            self.sessions
                .iter()
                .find(|session| session.is_current)
                .map(|session| session.name.as_str())
                .unwrap_or("-"),
            self.sessions.len()
        );
        write_line(&mut stdout, &truncate(&title, self.cols))?;
        write_line(
            &mut stdout,
            &truncate(
                "j/k navigate  J/K reorder  g/G first/last  Enter switch  q quit",
                self.cols,
            ),
        )?;
        write_line(&mut stdout, "")?;

        let name_width = self
            .sessions
            .iter()
            .map(|session| session.name.chars().count())
            .max()
            .unwrap_or(8)
            .min(self.cols.saturating_sub(20).max(8));

        for (index, session) in self
            .sessions
            .iter()
            .enumerate()
            .skip(self.top)
            .take(self.viewport_height())
        {
            let pointer = if index == self.selected { ">" } else { " " };
            let current = if session.is_current { "*" } else { " " };
            let line = format!(
                "{} {:>2} {:<name_width$} {:>2}w {:>2}c {}",
                pointer,
                index + 1,
                session.name,
                session.windows,
                session.attached_clients,
                current,
                name_width = name_width
            );
            write_line(&mut stdout, &truncate(&line, self.cols))?;
        }

        let used_rows = self.viewport_height().min(self.sessions.len().saturating_sub(self.top));
        for _ in used_rows..self.viewport_height() {
            write_line(&mut stdout, "")?;
        }

        write_line(&mut stdout, "")?;
        write_line(&mut stdout, &truncate(&self.status, self.cols))?;
        stdout.flush()?;
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
    saved_state: String,
}

impl TerminalGuard {
    fn enter() -> AppResult<Self> {
        let saved_state = String::from_utf8(
            Command::new("stty")
                .arg("-g")
                .output()?
                .stdout,
        )?;
        let saved_state = saved_state.trim().to_string();

        let status = Command::new("stty").args(["raw", "-echo"]).status()?;
        if !status.success() {
            return Err("failed to enter raw mode".into());
        }

        let mut stdout = io::stdout().lock();
        write!(stdout, "\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;

        Ok(Self { saved_state })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = Command::new("stty").arg(&self.saved_state).status();
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

    let contents = sessions
        .iter()
        .map(|session| session.name.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(order_file, format!("{contents}\n"))?;
    Ok(())
}

fn terminal_size() -> AppResult<(usize, usize)> {
    let output = Command::new("stty").arg("size").output()?;
    if !output.status.success() {
        return Err("failed to read terminal size".into());
    }

    let size = String::from_utf8(output.stdout)?;
    let mut parts = size.split_whitespace();
    let rows = parts.next().unwrap_or("24").parse::<usize>()?;
    let cols = parts.next().unwrap_or("80").parse::<usize>()?;
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

fn write_line(stdout: &mut impl Write, text: &str) -> io::Result<()> {
    write!(stdout, "{text}\r\n")
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
