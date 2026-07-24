use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{Project, Studio};
use crate::storage::{ProjectStore, quarantine_invalid_file};

pub(crate) const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HISTORY_SNAPSHOTS: usize = 128;

#[derive(Clone)]
pub(crate) struct ProjectHistory {
    pub(crate) snapshots: Vec<Project>,
    pub(crate) current: usize,
}

impl ProjectHistory {
    pub(crate) fn new(project: Project) -> Self {
        Self {
            snapshots: vec![project],
            current: 0,
        }
    }

    pub(crate) fn push(&mut self, project: Project) {
        self.snapshots.truncate(self.current + 1);
        self.snapshots.push(project);
        self.current = self.snapshots.len() - 1;
        if self.snapshots.len() > MAX_HISTORY_SNAPSHOTS {
            self.snapshots.remove(0);
            self.current -= 1;
        }
    }
}

pub(crate) fn load_project_history(path: &Path, project: &Project) -> io::Result<ProjectHistory> {
    let source = fs::read_to_string(path)?;
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
            "history must contain between 1 and 128 snapshots",
        ));
    }
    let snapshots = snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| {
            if snapshot.is_null() {
                return Ok((index, None));
            }
            Project::from_json(&snapshot.to_string())
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
    Ok(ProjectHistory { snapshots, current })
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
        current: usize,
        snapshots: Vec<serde_json::Value>,
    }

    let snapshots = history
        .snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| {
            if index == history.current {
                Ok(serde_json::Value::Null)
            } else {
                serde_json::from_str(&snapshot.to_json())
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("validated project snapshots serialize to JSON values");
    serde_json::to_value(PersistedHistory {
        current: history.current,
        snapshots,
    })
    .expect("project history serializes to JSON")
}

pub(crate) fn history_path(project_path: &Path) -> PathBuf {
    let file_name = project_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sound-graph.json");
    project_path.with_file_name(format!("{file_name}.history.json"))
}

pub(crate) fn save_project_state(
    store: &ProjectStore,
    project: &Project,
    history: &ProjectHistory,
) -> io::Result<()> {
    store.save_source(&project_document(project, history))?;
    let legacy_history = history_path(store.path());
    if legacy_history.is_file() {
        let _ = fs::remove_file(legacy_history);
    }
    Ok(())
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
    let separate_history = history_path(store.path());
    let source = store.read_source()?;
    let has_embedded_history = serde_json::from_str::<serde_json::Value>(&source)
        .ok()
        .and_then(|value| value.get("history").cloned())
        .is_some();
    let loaded_history = if has_embedded_history {
        load_project_history(store.path(), studio.project())
    } else if separate_history.is_file() {
        load_project_history(&separate_history, studio.project())
    } else {
        load_project_history(store.path(), studio.project())
    };
    let history = match loaded_history {
        Ok(history) => history,
        Err(error) if error.kind() == io::ErrorKind::InvalidData && separate_history.is_file() => {
            let quarantine = quarantine_invalid_file(&separate_history)?;
            eprintln!(
                "warning: quarantined invalid project history {} as {}: {error}",
                separate_history.display(),
                quarantine.display()
            );
            ProjectHistory::new(studio.project().clone())
        }
        Err(error) => return Err(error),
    };
    save_project_state(&store, studio.project(), &history)?;
    Ok((store, studio, history))
}

pub(crate) fn trim_project_history(project: &Project, history: &mut ProjectHistory) {
    let maximum = project.to_json().len() + MAX_HISTORY_BYTES;
    while history.snapshots.len() > 1 && project_document(project, history).len() > maximum {
        if history.current > 0 {
            history.snapshots.remove(0);
            history.current -= 1;
        } else {
            history.snapshots.pop();
        }
    }
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
        history.push(project.clone());

        fs::create_dir(history_path(&path)).expect("unwritable legacy history destination");
        save_project_state(&store, &project, &history).expect("single-file state commit");

        assert_eq!(
            store.read().expect("committed project").to_json(),
            project.to_json()
        );
        let loaded = load_project_history(&path, &project).expect("embedded history");
        assert_eq!(loaded.current, 1);
        assert_eq!(loaded.snapshots.len(), 2);
        assert!(
            store
                .read_source()
                .expect("project document")
                .contains("\"history\"")
        );
        fs::remove_dir_all(root).expect("remove state test directory");
    }
}
