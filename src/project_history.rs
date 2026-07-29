#[cfg(test)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

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

    pub(crate) fn push(&mut self, project: Project) {
        let parent = self.current;
        self.snapshots.push(project);
        self.parents.push(Some(parent));
        self.current = self.snapshots.len() - 1;
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
        current: usize,
        snapshots: Vec<serde_json::Value>,
        parents: Vec<Option<usize>>,
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
        parents: history.parents.clone(),
    })
    .expect("project history serializes to JSON")
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
        history.push(project.clone());

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
        history.push(second);
        let forward = history.snapshots[1].to_json();

        history.current = 0;
        let mut branch = history.snapshots[0].clone();
        branch.version += 2;
        history.push(branch);

        assert_eq!(history.snapshots.len(), 3);
        assert_eq!(history.snapshots[1].to_json(), forward);
        assert_eq!(history.parent(2), Some(0));
    }
}
