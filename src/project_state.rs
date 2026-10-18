use std::io;
use std::path::{Path, PathBuf};

use crate::model::{Project, Studio};
use crate::project_history::{
    ProjectHistory, open_project_with_history, project_document, save_project_state,
};
use crate::storage::{ProjectStore, replace_text_file};

// The studio and its history share one lock; candidates become visible only after saving.
pub(crate) struct ProjectState {
    studio: Studio,
    history: ProjectHistory,
    store: Option<ProjectStore>,
}

impl ProjectState {
    pub(crate) fn open_user(path: PathBuf) -> io::Result<Self> {
        if !path.exists() {
            let project = Project::demo();
            let history = ProjectHistory::new(project.clone());
            replace_text_file(&path, &project_document(&project, &history))?;
        }
        Self::open(path)
    }

    pub(crate) fn open(path: PathBuf) -> io::Result<Self> {
        let (store, studio, history) = open_project_with_history(path)?;
        Ok(Self {
            studio,
            history,
            store: Some(store),
        })
    }

    #[cfg(test)]
    pub(crate) fn in_memory(studio: Studio) -> Self {
        Self {
            history: ProjectHistory::new(studio.project().clone()),
            studio,
            store: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_project_for_test(&mut self, project: Project) {
        self.history = ProjectHistory::new(project.clone());
        self.studio = Studio::from_project(project);
    }

    pub(crate) fn studio(&self) -> &Studio {
        &self.studio
    }

    pub(crate) fn project(&self) -> &Project {
        self.studio.project()
    }

    pub(crate) fn history(&self) -> &ProjectHistory {
        &self.history
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.store.as_ref().map(ProjectStore::path)
    }

    pub(crate) fn project_json(&self) -> String {
        self.studio
            .to_json_with_can_undo(self.history.parent(self.history.current).is_some())
    }

    pub(crate) fn commit(&mut self, candidate: Studio) -> io::Result<()> {
        let mut history = self.history.clone();
        history.push(self.project(), candidate.project())?;
        self.publish(candidate, history)
    }

    pub(crate) fn commit_metadata(&mut self, candidate: Studio) -> io::Result<()> {
        let mut history = self.history.clone();
        history.update_current_metadata(candidate.project())?;
        self.publish(candidate, history)
    }

    pub(crate) fn checkout(&mut self, index: usize) -> io::Result<()> {
        let version = self
            .project()
            .version
            .checked_add(1)
            .ok_or_else(|| io::Error::other("project revision limit reached"))?;
        let (mut history, mut project) = self.history.clone().checkout(index, self.project())?;
        project.version = version;
        project.edit_operations = self.project().edit_operations.clone();
        history.update_current_metadata(&project)?;
        self.publish(Studio::from_project(project), history)
    }

    fn publish(&mut self, studio: Studio, history: ProjectHistory) -> io::Result<()> {
        if let Some(store) = &self.store {
            save_project_state(store, studio.project(), &history)?;
        }
        self.studio = studio;
        self.history = history;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn operation_json(state: &ProjectState) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&state.project_json()).unwrap()["editOperations"]
            .clone()
    }

    #[test]
    fn failed_saves_preserve_state_and_history_navigation_preserves_operations() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-project-state-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("test directory");
        let path = root.join("sound-graph.json");
        let mut state = ProjectState::open(path.clone()).expect("project state");
        let initial_duration = state.project().duration;
        let mut changed = state.studio().clone();
        changed.set_duration(80.0).expect("duration change");
        assert!(changed.record_operation_step("state-test", "Gemini", "Changed duration"));
        state.commit(changed).expect("commit graph change");
        let before = project_document(state.project(), state.history());
        let saved = fs::read_to_string(&path).expect("saved document");

        let mut changed = state.studio().clone();
        changed.set_duration(90.0).expect("next duration change");
        assert!(state.commit_metadata(changed.clone()).is_err());
        assert_eq!(project_document(state.project(), state.history()), before);
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);

        // A directory at the destination forces replacement to fail on every platform.
        let backup = root.join("saved.json");
        fs::rename(&path, &backup).expect("preserve document");
        fs::create_dir(&path).expect("block replacement");
        assert!(state.commit(changed).is_err());
        assert_eq!(project_document(state.project(), state.history()), before);
        let mut completed = state.studio().clone();
        assert!(completed.mark_operation_complete("state-test", "Done"));
        assert!(state.commit_metadata(completed.clone()).is_err());
        assert_eq!(project_document(state.project(), state.history()), before);
        assert!(state.checkout(0).is_err());
        assert_eq!(project_document(state.project(), state.history()), before);
        assert_eq!(fs::read_to_string(&backup).unwrap(), saved);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);

        fs::remove_dir(&path).expect("unblock replacement");
        fs::rename(&backup, &path).expect("restore document");
        state
            .commit_metadata(completed)
            .expect("complete operation");
        let version = state.project().version;
        let operations = operation_json(&state);
        state.checkout(0).expect("undo graph change");
        assert_eq!(state.project().duration, initial_duration);
        assert_eq!(state.project().version, version + 1);
        assert_eq!(state.history().len(), 2);
        assert_eq!(operation_json(&state), operations);

        let mut reopened = ProjectState::open(path).expect("reopen selected history");
        assert_eq!(reopened.project_json(), state.project_json());
        reopened.checkout(1).expect("return to graph change");
        assert_eq!(reopened.project().duration, 80.0);
        assert_eq!(operation_json(&reopened), operations);
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
