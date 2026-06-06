use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub const UNGROUPED_NAME: &str = "Ungrouped";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GroupState {
    #[serde(default = "current_version")]
    pub version: u8,
    #[serde(default)]
    pub groups: Vec<Group>,
}

impl Default for GroupState {
    fn default() -> Self {
        Self {
            version: current_version(),
            groups: Vec::new(),
        }
    }
}

impl GroupState {
    pub fn from_toml(contents: &str) -> Result<Self, String> {
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }

        let mut state = toml::from_str::<Self>(contents).map_err(|err| err.to_string())?;
        if state.version != current_version() {
            return Err(format!("unsupported group file version: {}", state.version));
        }
        state.normalize()?;
        Ok(state)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|err| err.to_string())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::from_toml(&contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.to_string()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        fs::write(path, self.to_toml()?).map_err(|err| err.to_string())
    }

    pub fn add_group(&mut self, name: &str) -> Result<usize, String> {
        let name = valid_name(name, self.groups.iter().map(|group| group.name.as_str()))?;
        self.groups.push(Group {
            name,
            collapsed: false,
            sessions: Vec::new(),
        });
        Ok(self.groups.len() - 1)
    }

    pub fn rename_group(&mut self, index: usize, name: &str) -> Result<(), String> {
        if index >= self.groups.len() {
            return Err("group does not exist".to_string());
        }
        let other_names = self
            .groups
            .iter()
            .enumerate()
            .filter_map(|(other_index, group)| {
                (other_index != index).then_some(group.name.as_str())
            });
        self.groups[index].name = valid_name(name, other_names)?;
        Ok(())
    }

    pub fn delete_group(&mut self, index: usize) -> Result<(), String> {
        if index >= self.groups.len() {
            return Err("group does not exist".to_string());
        }
        self.groups.remove(index);
        Ok(())
    }

    pub fn move_group(&mut self, index: usize, offset: isize) -> Option<usize> {
        let target = index.checked_add_signed(offset)?;
        if index >= self.groups.len() || target >= self.groups.len() {
            return None;
        }
        self.groups.swap(index, target);
        Some(target)
    }

    pub fn move_session(&mut self, session_name: &str, group_index: Option<usize>) {
        for group in &mut self.groups {
            group.sessions.retain(|name| name != session_name);
        }
        if let Some(group) = group_index.and_then(|index| self.groups.get_mut(index)) {
            group.sessions.push(session_name.to_string());
        }
    }

    pub fn group_for_session(&self, session_name: &str) -> Option<usize> {
        self.groups
            .iter()
            .position(|group| group.sessions.iter().any(|name| name == session_name))
    }

    fn normalize(&mut self) -> Result<(), String> {
        let mut group_names = HashSet::new();
        let mut session_names = HashSet::new();
        for group in &mut self.groups {
            group.name = group.name.trim().to_string();
            let normalized_name = group.name.to_lowercase();
            if group.name.is_empty()
                || normalized_name == UNGROUPED_NAME.to_lowercase()
                || !group_names.insert(normalized_name)
            {
                return Err(format!("invalid or duplicate group name: {}", group.name));
            }
            group
                .sessions
                .retain(|session_name| session_names.insert(session_name.clone()));
        }
        Ok(())
    }
}

fn current_version() -> u8 {
    1
}

fn valid_name<'a>(
    name: &str,
    mut existing_names: impl Iterator<Item = &'a str>,
) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("group name cannot be empty".to_string());
    }
    if name.eq_ignore_ascii_case(UNGROUPED_NAME) {
        return Err(format!("{UNGROUPED_NAME} is reserved"));
    }
    if existing_names.any(|existing_name| existing_name.eq_ignore_ascii_case(name)) {
        return Err(format!("group already exists: {name}"));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Group, GroupState};

    #[test]
    fn group_state_round_trips_as_toml() {
        let state = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: true,
                sessions: vec!["api".to_string(), "database".to_string()],
            }],
        };

        let encoded = state.to_toml().unwrap();
        let decoded = GroupState::from_toml(&encoded).unwrap();

        assert_eq!(decoded, state);
        assert!(encoded.contains("[[groups]]"));
    }

    #[test]
    fn moving_a_session_removes_it_from_other_groups() {
        let mut state = GroupState {
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
                    sessions: Vec::new(),
                },
            ],
        };

        state.move_session("api", Some(1));

        assert!(state.groups[0].sessions.is_empty());
        assert_eq!(state.groups[1].sessions, ["api"]);
    }

    #[test]
    fn moving_a_session_to_ungrouped_forgets_membership() {
        let mut state = GroupState {
            version: 1,
            groups: vec![Group {
                name: "Work".to_string(),
                collapsed: false,
                sessions: vec!["api".to_string(), "stale".to_string()],
            }],
        };

        state.move_session("api", None);

        assert_eq!(state.groups[0].sessions, ["stale"]);
        assert_eq!(state.group_for_session("api"), None);
    }

    #[test]
    fn group_names_are_trimmed_and_unique_case_insensitively() {
        let mut state = GroupState::default();

        assert_eq!(state.add_group(" Work ").unwrap(), 0);
        assert!(state.add_group("work").is_err());
        assert!(state.add_group("Ungrouped").is_err());
        assert!(state.add_group("  ").is_err());
    }

    #[test]
    fn deleting_a_group_keeps_other_groups_and_drops_membership() {
        let mut state = GroupState {
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

        state.delete_group(0).unwrap();

        assert_eq!(state.groups.len(), 1);
        assert_eq!(state.groups[0].name, "Personal");
        assert_eq!(state.group_for_session("api"), None);
    }
}
