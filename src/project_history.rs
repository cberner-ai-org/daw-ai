#[cfg(test)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Project, Studio};
use crate::storage::{
    MAX_PROJECT_DOCUMENT_BYTES, ProjectStore, quarantine_invalid_file, read_bounded_text,
};

const MAX_HISTORY_SNAPSHOTS: usize = 10_000;

#[derive(Clone)]
pub(crate) struct ProjectHistory {
    pub(crate) snapshots: Vec<Project>,
    parents: Vec<Option<usize>>,
    pub(crate) current: usize,
}

impl ProjectHistory {
    pub(crate) fn new(project: Project) -> Self {
        Self {
            snapshots: vec![project],
            parents: vec![None],
            current: 0,
        }
    }

    pub(crate) fn push(&mut self, project: Project) -> io::Result<()> {
        self.push_with_limits(project, MAX_HISTORY_SNAPSHOTS, MAX_PROJECT_DOCUMENT_BYTES)
    }

    fn push_with_limits(
        &mut self,
        project: Project,
        maximum_snapshots: usize,
        maximum_document_bytes: u64,
    ) -> io::Result<()> {
        if self.snapshots.len() >= maximum_snapshots {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("project history is limited to {maximum_snapshots} snapshots"),
            ));
        }
        let parent = self.current;
        self.snapshots.push(project);
        self.parents.push(Some(parent));
        self.current = self.snapshots.len() - 1;
        if project_document(&self.snapshots[self.current], self).len() as u64
            > maximum_document_bytes
        {
            self.snapshots.pop();
            self.parents.pop();
            self.current = parent;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("project history exceeds the {maximum_document_bytes}-byte document limit"),
            ));
        }
        Ok(())
    }

    pub(crate) fn parent(&self, index: usize) -> Option<usize> {
        self.parents.get(index).copied().flatten()
    }
}

pub(crate) fn load_project_history(path: &Path, project: &Project) -> io::Result<ProjectHistory> {
    let source = read_bounded_text(path, MAX_PROJECT_DOCUMENT_BYTES, "project history")?;
    let value: serde_json::Value = serde_json::from_str(&source)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(history) = value.get("history") else {
        return Ok(ProjectHistory::new(project.clone()));
    };
    let snapshots = history
        .get("snapshots")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "history snapshots are required")
        })?;
    if snapshots.is_empty() || snapshots.len() > MAX_HISTORY_SNAPSHOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("history must contain between 1 and {MAX_HISTORY_SNAPSHOTS} snapshots"),
        ));
    }
    let parents = history
        .get("parents")
        .and_then(serde_json::Value::as_array)
        .filter(|parents| parents.len() == snapshots.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "history parents are required"))?
        .iter()
        .enumerate()
        .map(|(index, parent)| {
            if parent.is_null() {
                return (index == 0).then_some(None);
            }
            parent
                .as_u64()
                .and_then(|parent| usize::try_from(parent).ok())
                .filter(|parent| *parent < index)
                .map(Some)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "history parents are invalid"))?;
    let encoding = history
        .get("encoding")
        .map(|encoding| {
            encoding.as_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "history encoding is invalid")
            })
        })
        .transpose()?;
    if encoding.is_some_and(|encoding| encoding != "delta-v1") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "history encoding is unsupported",
        ));
    }
    let current_source = project.to_json();
    let snapshots = snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| {
            if snapshot.is_null() {
                return Ok((index, None));
            }
            let source = if encoding == Some("delta-v1") {
                let delta =
                    serde_json::from_value::<SnapshotDelta>(snapshot.clone()).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("history snapshot delta is invalid: {error}"),
                        )
                    })?;
                apply_snapshot_delta(&current_source, &delta)?
            } else {
                snapshot.to_string()
            };
            Project::from_json(&source)
                .map(|snapshot| (index, Some(snapshot)))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let current = history
        .get("current")
        .and_then(serde_json::Value::as_u64)
        .and_then(|current| usize::try_from(current).ok())
        .filter(|current| *current < snapshots.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "history current is invalid"))?;
    if snapshots
        .iter()
        .filter(|(_, snapshot)| snapshot.is_none())
        .count()
        > 1
        || snapshots
            .iter()
            .any(|(index, snapshot)| snapshot.is_none() && *index != current)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "only the current history snapshot may be omitted",
        ));
    }
    let snapshots = snapshots
        .into_iter()
        .map(|(_, snapshot)| snapshot.unwrap_or_else(|| project.clone()))
        .collect::<Vec<_>>();
    if snapshots[current].to_json() != project.to_json() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "history current does not match the saved project",
        ));
    }
    Ok(ProjectHistory {
        snapshots,
        parents,
        current,
    })
}

pub(crate) fn project_document(project: &Project, history: &ProjectHistory) -> String {
    let mut document = serde_json::from_str::<serde_json::Value>(&project.to_json())
        .expect("validated project serializes to a JSON object");
    document
        .as_object_mut()
        .expect("a project serializes to an object")
        .insert("history".to_owned(), history_value(history));
    format!("{document}\n")
}

fn history_value(history: &ProjectHistory) -> serde_json::Value {
    #[derive(Serialize)]
    struct PersistedHistory {
        encoding: &'static str,
        current: usize,
        snapshots: Vec<serde_json::Value>,
        parents: Vec<Option<usize>>,
    }

    let current_source = history.snapshots[history.current].to_json();
    let snapshots = history
        .snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| {
            if index == history.current {
                serde_json::Value::Null
            } else {
                serde_json::to_value(snapshot_delta(&current_source, &snapshot.to_json()))
                    .expect("snapshot delta serializes to JSON")
            }
        })
        .collect();
    serde_json::to_value(PersistedHistory {
        encoding: "delta-v1",
        current: history.current,
        snapshots,
        parents: history.parents.clone(),
    })
    .expect("project history serializes to JSON")
}

#[derive(Deserialize, Serialize)]
struct SnapshotDelta {
    prefix: usize,
    suffix: usize,
    replacement: String,
}

fn snapshot_delta(base: &str, target: &str) -> SnapshotDelta {
    let mut prefix = 0;
    for ((base_index, base_character), (target_index, target_character)) in
        base.char_indices().zip(target.char_indices())
    {
        if base_character != target_character {
            break;
        }
        prefix = (base_index + base_character.len_utf8())
            .min(target_index + target_character.len_utf8());
    }
    let base_tail = &base[prefix..];
    let target_tail = &target[prefix..];
    let mut suffix = 0;
    for (base_character, target_character) in base_tail.chars().rev().zip(target_tail.chars().rev())
    {
        if base_character != target_character {
            break;
        }
        suffix += base_character.len_utf8();
    }
    SnapshotDelta {
        prefix,
        suffix,
        replacement: target[prefix..target.len() - suffix].to_owned(),
    }
}

fn apply_snapshot_delta(base: &str, delta: &SnapshotDelta) -> io::Result<String> {
    let suffix_start = base
        .len()
        .checked_sub(delta.suffix)
        .filter(|suffix_start| delta.prefix <= *suffix_start)
        .filter(|suffix_start| {
            base.is_char_boundary(delta.prefix) && base.is_char_boundary(*suffix_start)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "history snapshot delta is out of bounds",
            )
        })?;
    let mut snapshot = String::with_capacity(delta.prefix + delta.replacement.len() + delta.suffix);
    snapshot.push_str(&base[..delta.prefix]);
    snapshot.push_str(&delta.replacement);
    snapshot.push_str(&base[suffix_start..]);
    Ok(snapshot)
}

pub(crate) fn save_project_state(
    store: &ProjectStore,
    project: &Project,
    history: &ProjectHistory,
) -> io::Result<()> {
    store.save_source(&project_document(project, history))
}

pub(crate) fn open_project_with_history(
    path: PathBuf,
) -> io::Result<(ProjectStore, Studio, ProjectHistory)> {
    let (store, studio) = match ProjectStore::open(path.clone()) {
        Ok(opened) => opened,
        Err(error) if error.kind() == io::ErrorKind::InvalidData && path.is_file() => {
            let quarantine = quarantine_invalid_file(&path)?;
            eprintln!(
                "warning: quarantined invalid sound graph {} as {}: {error}",
                path.display(),
                quarantine.display()
            );
            ProjectStore::open(path.clone())?
        }
        Err(error) => return Err(error),
    };
    let source = store.read_source()?;
    let has_embedded_history = serde_json::from_str::<serde_json::Value>(&source)
        .ok()
        .and_then(|value| value.get("history").cloned())
        .is_some();
    let loaded_history = load_project_history(store.path(), studio.project());
    let history = match loaded_history {
        Ok(history) => history,
        Err(error) if error.kind() == io::ErrorKind::InvalidData && has_embedded_history => {
            eprintln!(
                "warning: discarded invalid embedded project history in {}: {error}",
                store.path().display()
            );
            ProjectHistory::new(studio.project().clone())
        }
        Err(error) => return Err(error),
    };
    save_project_state(&store, studio.project(), &history)?;
    Ok((store, studio, history))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_and_history_publish_as_one_revision() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-project-state-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("state test directory");
        let path = root.join("sound-graph.json");
        let (store, studio) = ProjectStore::open(path.clone()).expect("initial project");
        let mut project = studio.project().clone();
        project.name = "Atomic revision".to_owned();
        project.version += 1;
        let mut history = ProjectHistory::new(studio.project().clone());
        history.push(project.clone()).expect("append history");

        save_project_state(&store, &project, &history).expect("single-file state commit");

        assert_eq!(
            store.read().expect("committed project").to_json(),
            project.to_json()
        );
        let loaded = load_project_history(&path, &project).expect("embedded history");
        assert_eq!(loaded.current, 1);
        assert_eq!(loaded.snapshots.len(), 2);
        assert_eq!(loaded.parent(1), Some(0));
        assert!(
            store
                .read_source()
                .expect("project document")
                .contains("\"history\"")
        );
        fs::remove_dir_all(root).expect("remove state test directory");
    }

    #[test]
    fn invalid_embedded_history_recovers_the_current_project() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-invalid-embedded-history-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("history test directory");
        let path = root.join("sound-graph.json");
        let project = Project::demo();
        let mut document =
            serde_json::from_str::<serde_json::Value>(&project.to_json()).expect("project JSON");
        document["history"] = serde_json::json!({"current":0,"snapshots":[]});
        fs::write(&path, format!("{document}\n")).expect("invalid embedded history");

        let (store, studio, history) =
            open_project_with_history(path).expect("recover embedded history");
        assert_eq!(studio.project().to_json(), project.to_json());
        assert_eq!(history.snapshots.len(), 1);
        assert_eq!(history.current, 0);
        load_project_history(store.path(), studio.project()).expect("repaired embedded history");
        fs::remove_dir_all(root).expect("remove history test directory");
    }

    #[test]
    fn editing_an_older_state_preserves_forward_history() {
        let mut history = ProjectHistory::new(Project::initial());
        let mut second = history.snapshots[0].clone();
        second.version += 1;
        history.push(second).expect("append forward history");
        let forward = history.snapshots[1].to_json();

        history.current = 0;
        let mut branch = history.snapshots[0].clone();
        branch.version += 2;
        history.push(branch).expect("append branch history");

        assert_eq!(history.snapshots.len(), 3);
        assert_eq!(history.snapshots[1].to_json(), forward);
        assert_eq!(history.parent(2), Some(0));
    }

    #[test]
    fn compact_history_round_trips_branched_unicode_projects() {
        let current = Project::demo();
        let mut history = ProjectHistory::new(current.clone());
        let mut second = current.clone();
        second.name = "Mix \u{2603}".to_owned();
        second.version += 1;
        history.push(second).expect("append unicode snapshot");
        history.current = 0;
        let mut branch = current.clone();
        branch.name = "Branch".to_owned();
        branch.version += 2;
        history.push(branch.clone()).expect("append branch");

        let root = std::env::temp_dir().join(format!(
            "daw-ai-compact-history-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("history test directory");
        let path = root.join("sound-graph.json");
        fs::write(&path, project_document(&branch, &history)).expect("compact history");

        let loaded = load_project_history(&path, &branch).expect("load compact history");
        assert_eq!(loaded.current, 2);
        assert_eq!(loaded.parent(1), Some(0));
        assert_eq!(loaded.parent(2), Some(0));
        assert_eq!(loaded.snapshots[1].name, "Mix \u{2603}");
        fs::remove_dir_all(root).expect("remove history test directory");
    }

    #[test]
    fn history_limits_are_enforced_before_mutating_state() {
        let project = Project::initial();
        let mut count_limited = ProjectHistory::new(project.clone());
        let error = count_limited
            .push_with_limits(project.clone(), 1, u64::MAX)
            .expect_err("snapshot limit");
        assert!(error.to_string().contains("1 snapshots"));
        assert_eq!(count_limited.snapshots.len(), 1);
        assert_eq!(count_limited.current, 0);

        let mut bytes_limited = ProjectHistory::new(project.clone());
        let error = bytes_limited
            .push_with_limits(project, 2, 1)
            .expect_err("document limit");
        assert!(error.to_string().contains("document limit"));
        assert_eq!(bytes_limited.snapshots.len(), 1);
        assert_eq!(bytes_limited.current, 0);
        assert_eq!(bytes_limited.parents, vec![None]);
    }

    #[test]
    fn compact_history_does_not_duplicate_large_unchanged_graphs() {
        let mut project = Project::demo();
        for index in 0..512 {
            project.edits.push(crate::model::Edit {
                id: 10_000 + index,
                start: 0.0,
                end: 1.0,
                prompt: "x".repeat(crate::model::MAX_PROMPT_CHARACTERS),
                summary: "Large history fixture".to_owned(),
            });
        }
        Project::from_json(&project.to_json()).expect("large valid project");
        let mut history = ProjectHistory::new(project.clone());
        project.version += 1;
        history.push(project.clone()).expect("append large project");

        let document = project_document(&project, &history);
        assert!(document.len() < project.to_json().len() + 2_048);
    }
}
