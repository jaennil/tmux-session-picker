use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

mod groups;

use groups::GroupState;

type AppResult<T> = Result<T, Box<dyn Error>>;

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

struct App {
    sessions: Vec<Session>,
    groups: GroupState,
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
}

#[derive(Clone, Copy)]
enum NameAction {
    Create,
    Rename(usize),
    CreateAndMove(usize),
}

enum Prompt {
    Name {
        label: &'static str,
        value: String,
        action: NameAction,
    },
    Move {
        session_index: usize,
        choice: usize,
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
        })
    }

    fn run(&mut self) -> AppResult<()> {
        self.ensure_visible();
        self.render_full()?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];

        loop {
            stdin.read_exact(&mut byte)?;

            if self.prompt.is_some() {
                self.handle_prompt(byte[0])?;
                self.ensure_visible();
                self.render_full()?;
                continue;
            }

            if self.searching {
                match byte[0] {
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

            match byte[0] {
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
                b'p' => self.toggle_pin()?,
                b'x' => self.kill_selected()?,
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
        let Some(session_index) = self.selected_session_index() else {
            self.status = "Select a session to pin it".to_string();
            return Ok(());
        };
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
        let Some(session_index) = self.selected_session_index() else {
            self.status = "Select a session to move it".to_string();
            return;
        };
        let choice = self
            .groups
            .group_for_session(&self.sessions[session_index].name)
            .unwrap_or(self.groups.groups.len());
        self.prompt = Some(Prompt::Move {
            session_index,
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
                    let result = self.submit_group_name(value, *action);
                    if let Err(err) = result {
                        self.status = err;
                        self.prompt = Some(prompt);
                    }
                }
                0x1b | 0x03 => self.status = "Cancelled".to_string(),
                0x7f | 0x08 => {
                    value.pop();
                    self.prompt = Some(prompt);
                }
                value_byte if value_byte.is_ascii_graphic() || value_byte == b' ' => {
                    value.push(char::from(value_byte));
                    self.prompt = Some(prompt);
                }
                _ => self.prompt = Some(prompt),
            },
            Prompt::Move {
                session_index,
                choice,
            } => {
                let option_count = self.groups.groups.len() + 2;
                match byte {
                    b'j' => *choice = (*choice + 1).min(option_count - 1),
                    b'k' => *choice = choice.saturating_sub(1),
                    b'g' => *choice = 0,
                    b'G' => *choice = option_count - 1,
                    b'\r' | b'\n' => {
                        let session_index = *session_index;
                        let choice = *choice;
                        if choice < self.groups.groups.len() {
                            self.move_session_to(session_index, Some(choice))?;
                            return Ok(());
                        }
                        if choice == self.groups.groups.len() {
                            self.move_session_to(session_index, None)?;
                            return Ok(());
                        }
                        self.prompt = Some(Prompt::Name {
                            label: "NEW GROUP",
                            value: String::new(),
                            action: NameAction::CreateAndMove(session_index),
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
            NameAction::CreateAndMove(session_index) => {
                let group_index = self.groups.add_group(value)?;
                let session_name = self.sessions[session_index].name.clone();
                self.groups.move_session(&session_name, Some(group_index));
                self.groups.save(&self.group_file)?;
                self.select_session_by_name(&session_name);
                self.status = format!(
                    "Moved {session_name} to {}",
                    self.groups.groups[group_index].name
                );
            }
        }
        Ok(())
    }

    fn move_session_to(
        &mut self,
        session_index: usize,
        group_index: Option<usize>,
    ) -> AppResult<()> {
        let session_name = self.sessions[session_index].name.clone();
        self.groups.move_session(&session_name, group_index);
        if let Some(group) = group_index.and_then(|index| self.groups.groups.get_mut(index)) {
            group.collapsed = false;
        } else {
            self.ungrouped_collapsed = false;
        }
        self.write_groups()?;
        self.select_session_by_name(&session_name);
        let destination = group_index
            .and_then(|index| self.groups.groups.get(index))
            .map(|group| group.name.as_str())
            .unwrap_or(groups::UNGROUPED_NAME);
        self.status = format!("Moved {session_name} to {destination}");
        Ok(())
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
                "Enter open/toggle  h/l fold  n new  m move  r rename  d delete  / search"
                    .to_string()
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
                let count = self
                    .sessions
                    .iter()
                    .filter(|session| {
                        self.groups.group_for_session(&session.name)
                            == (group_index < self.groups.groups.len()).then_some(group_index)
                    })
                    .count();
                let marker = if collapsed { "▸" } else { "▼" };
                format!("{pointer} {marker} {name} ({count})")
            }
            Some(VisibleRow::Session(session_index)) => {
                let session = &self.sessions[session_index];
                let pin = if session.pinned { "!" } else { " " };
                let current = if session.is_current { "*" } else { "" };
                let last = format_relative_activity(session.last_activity);
                format!(
                    "{pointer}   {:<name_width$}  {:>activity_width$}  {:^3} {pin}",
                    session.name,
                    last,
                    current,
                    name_width = layout.name_width,
                    activity_width = layout.activity_width,
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
                    session_index,
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
                        "MOVE {} -> [{destination}]  j/k choose  Enter confirm  Esc cancel",
                        self.sessions[*session_index].name
                    )
                }
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
        Session, VisibleRow, arrange_sessions, build_visible_rows, first_session_row_position,
        format_relative_activity, pinned_names_from_sessions, session_name_matches,
        write_pinned_names,
    };
    use crate::groups::{Group, GroupState};
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
}
