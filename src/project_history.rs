#[cfg(test)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Project, Studio};
use crate::storage::{
    MAX_PROJECT_BYTES, MAX_PROJECT_DOCUMENT_BYTES, ProjectStore, quarantine_invalid_file,
    read_bounded_text,
};

const MAX_HISTORY_SNAPSHOTS: usize = 10_000;
const MAX_CHECKOUT_PEAK_SOURCE_BYTES: usize = MAX_PROJECT_BYTES * 3;

#[derive(Clone)]
pub(crate) struct ProjectHistory {
    snapshots: Vec<CompactSnapshot>,
    parents: Vec<Option<usize>>,
    pub(crate) current: usize,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct HistoryEntry {
    pub(crate) version: u64,
    pub(crate) edit_count: usize,
    pub(crate) summary: String,
    pub(crate) source: String,
    pub(crate) prompt: Option<String>,
    pub(crate) start: Option<f32>,
    pub(crate) end: Option<f32>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CompactSnapshot {
    base: Option<usize>,
    delta: Option<SnapshotDelta>,
    source_bytes: usize,
    checksum: u64,
    entry: HistoryEntry,
}

impl ProjectHistory {
    pub(crate) fn new(project: Project) -> Self {
        let source = history_source(&project);
        Self {
            snapshots: vec![CompactSnapshot {
                base: None,
                delta: None,
                source_bytes: source.len(),
                checksum: source_checksum(&source),
                entry: history_entry(&project, 0, 0),
            }],
            parents: vec![None],
            current: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub(crate) fn entry(&self, index: usize) -> Option<&HistoryEntry> {
        self.snapshots.get(index).map(|snapshot| &snapshot.entry)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.snapshots.iter().map(|snapshot| &snapshot.entry)
    }

    pub(crate) fn update_current_metadata(&mut self, project: &Project) {
        let entry = &mut self.snapshots[self.current].entry;
        entry.version = project.version;
        entry.edit_count = project.edits.len();
    }

    pub(crate) fn push(&mut self, current: &Project, project: &Project) -> io::Result<()> {
        self.push_with_limits(
            current,
            project,
            MAX_HISTORY_SNAPSHOTS,
            MAX_PROJECT_DOCUMENT_BYTES,
        )
    }

    fn push_with_limits(
        &mut self,
        current: &Project,
        project: &Project,
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
        let previous_current = self.snapshots[parent].clone();
        if previous_current.base.is_some() || previous_current.delta.is_some() {
            return Err(invalid_data("project history current snapshot is invalid"));
        }
        let next = self.snapshots.len();
        let current_source = history_source(current);
        let next_source = history_source(project);
        if current_source.len() > MAX_PROJECT_BYTES || next_source.len() > MAX_PROJECT_BYTES {
            return Err(invalid_data(
                "project history snapshot exceeds the graph limit",
            ));
        }
        validate_snapshot_source(&previous_current, &current_source)?;
        self.snapshots[parent].base = Some(next);
        self.snapshots[parent].delta = Some(snapshot_delta(&next_source, &current_source));
        self.snapshots.push(CompactSnapshot {
            base: None,
            delta: None,
            source_bytes: next_source.len(),
            checksum: source_checksum(&next_source),
            entry: history_entry(project, next, previous_current.entry.edit_count),
        });
        self.parents.push(Some(parent));
        self.current = next;
        let maximum_persisted_bytes = match self.maximum_persisted_bytes(project) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.snapshots.pop();
                self.parents.pop();
                self.snapshots[parent] = previous_current;
                self.current = parent;
                return Err(error);
            }
        };
        if maximum_persisted_bytes as u64 > maximum_document_bytes {
            self.snapshots.pop();
            self.parents.pop();
            self.snapshots[parent] = previous_current;
            self.current = parent;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "project history can exceed the {maximum_document_bytes}-byte document limit after checkout"
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn checkout(
        mut self,
        index: usize,
        current: &Project,
    ) -> io::Result<(Self, Project)> {
        if index >= self.snapshots.len() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "history snapshot does not exist",
            ));
        }
        if index == self.current {
            return Ok((self, history_project(current)));
        }
        let project =
            self.reroot_with_peak_limit(index, current, MAX_CHECKOUT_PEAK_SOURCE_BYTES)?;
        Ok((self, project))
    }

    fn reroot_with_peak_limit(
        &mut self,
        index: usize,
        current: &Project,
        maximum_peak_source_bytes: usize,
    ) -> io::Result<Project> {
        let mut path = Vec::new();
        let mut cursor = index;
        while cursor != self.current {
            if path.len() >= self.snapshots.len() {
                return Err(invalid_data("project history delta tree contains a cycle"));
            }
            path.push(cursor);
            cursor = self.snapshots[cursor]
                .base
                .ok_or_else(|| invalid_data("project history snapshot is disconnected"))?;
        }
        path.reverse();

        let mut base_index = self.current;
        let mut base_source = history_source(current);
        validate_snapshot_source(&self.snapshots[base_index], &base_source)?;
        for target_index in path {
            let target_bytes = self.snapshots[target_index].source_bytes;
            let delta = self.snapshots[target_index]
                .delta
                .as_ref()
                .ok_or_else(|| invalid_data("project history snapshot delta is missing"))?;
            let reverse_replacement_bytes = base_source
                .len()
                .checked_sub(delta.prefix)
                .and_then(|bytes| bytes.checked_sub(delta.suffix))
                .ok_or_else(|| invalid_data("project history snapshot delta is out of bounds"))?;
            base_source
                .len()
                .checked_add(target_bytes)
                .and_then(|bytes| bytes.checked_add(reverse_replacement_bytes))
                .filter(|bytes| *bytes <= maximum_peak_source_bytes)
                .ok_or_else(|| {
                    invalid_data(format!(
                        "history checkout exceeds the {maximum_peak_source_bytes}-byte peak source limit"
                    ))
                })?;
            let prefix = delta.prefix;
            let suffix = delta.suffix;
            let delta = self.snapshots[target_index]
                .delta
                .take()
                .expect("checked history snapshot delta exists");
            let target_source = apply_snapshot_delta(&base_source, &delta)?;
            validate_snapshot_source(&self.snapshots[target_index], &target_source)?;
            let suffix_start = base_source
                .len()
                .checked_sub(suffix)
                .filter(|suffix_start| prefix <= *suffix_start)
                .ok_or_else(|| invalid_data("project history snapshot delta is out of bounds"))?;
            let reverse_replacement = &base_source[prefix..suffix_start];
            if serialized_string_bytes(reverse_replacement) != delta.reverse_replacement_bytes {
                return Err(invalid_data(
                    "project history reverse delta size is invalid",
                ));
            }
            let forward_replacement_bytes = serialized_string_bytes(&delta.replacement);
            drop(delta);
            let reverse_delta = SnapshotDelta {
                prefix,
                suffix,
                reverse_replacement_bytes: forward_replacement_bytes,
                replacement: reverse_replacement.to_owned(),
            };
            self.snapshots[base_index].base = Some(target_index);
            self.snapshots[base_index].delta = Some(reverse_delta);
            self.snapshots[target_index].base = None;
            base_source = target_source;
            base_index = target_index;
        }
        self.current = index;
        Project::from_json(&base_source).map_err(|error| invalid_data(error.to_string()))
    }

    pub(crate) fn parent(&self, index: usize) -> Option<usize> {
        self.parents.get(index).copied().flatten()
    }

    #[cfg(test)]
    fn stored_delta_bytes(&self) -> usize {
        self.snapshots
            .iter()
            .filter_map(|snapshot| snapshot.delta.as_ref())
            .map(|delta| delta.replacement.len())
            .sum()
    }

    fn maximum_persisted_bytes(&self, project: &Project) -> io::Result<usize> {
        let mut children = vec![Vec::new(); self.snapshots.len()];
        for (index, snapshot) in self.snapshots.iter().enumerate() {
            if let Some(base) = snapshot.base {
                children
                    .get_mut(base)
                    .ok_or_else(|| invalid_data("project history snapshot base is invalid"))?
                    .push(index);
            }
        }
        let current_bytes = project_document(project, self).len();
        let mut root_bytes = vec![None; self.snapshots.len()];
        root_bytes[self.current] = Some(current_bytes as i128);
        let mut pending = vec![self.current];
        let mut maximum = current_bytes as i128;
        while let Some(base_index) = pending.pop() {
            let base_bytes = root_bytes[base_index]
                .ok_or_else(|| invalid_data("project history snapshot is disconnected"))?;
            for &target_index in &children[base_index] {
                let base = &self.snapshots[base_index];
                let target = &self.snapshots[target_index];
                let delta = target
                    .delta
                    .as_ref()
                    .ok_or_else(|| invalid_data("project history snapshot delta is missing"))?;
                let replacement_bytes = serialized_string_bytes(&delta.replacement);
                let index_bytes =
                    decimal_bytes(target_index) as i128 - decimal_bytes(base_index) as i128;
                let next_bytes = base_bytes + target.source_bytes as i128
                    - base.source_bytes as i128
                    + delta.reverse_replacement_bytes as i128
                    - replacement_bytes as i128
                    + decimal_bytes(replacement_bytes) as i128
                    - decimal_bytes(delta.reverse_replacement_bytes) as i128
                    + index_bytes * 2;
                if next_bytes <= 0 {
                    return Err(invalid_data("project history document size is invalid"));
                }
                root_bytes[target_index] = Some(next_bytes);
                maximum = maximum.max(next_bytes);
                pending.push(target_index);
            }
        }
        if root_bytes.iter().any(Option::is_none) {
            return Err(invalid_data("project history snapshot is disconnected"));
        }
        usize::try_from(maximum)
            .ok()
            .and_then(|bytes| bytes.checked_add(self.navigation_metadata_reserve(project)))
            .ok_or_else(|| invalid_data("project history document size is invalid"))
    }

    fn navigation_metadata_reserve(&self, project: &Project) -> usize {
        const MAX_VERSION_BYTES: usize = 20;
        const MAX_EDIT_COUNT_BYTES: usize = 5;

        MAX_VERSION_BYTES.saturating_sub(decimal_bytes_u64(project.version))
            + self
                .snapshots
                .iter()
                .map(|snapshot| {
                    MAX_VERSION_BYTES.saturating_sub(decimal_bytes_u64(snapshot.entry.version))
                        + MAX_EDIT_COUNT_BYTES
                            .saturating_sub(decimal_bytes(snapshot.entry.edit_count))
                })
                .sum::<usize>()
    }
}

pub(crate) fn load_project_history(path: &Path, project: &Project) -> io::Result<ProjectHistory> {
    let source = read_bounded_text(path, MAX_PROJECT_DOCUMENT_BYTES, "project history")?;
    let value: serde_json::Value = serde_json::from_str(&source)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(history) = value.get("history") else {
        return Ok(ProjectHistory::new(project.clone()));
    };
    let snapshot_values = history
        .get("snapshots")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "history snapshots are required")
        })?;
    if snapshot_values.is_empty() || snapshot_values.len() > MAX_HISTORY_SNAPSHOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("history must contain between 1 and {MAX_HISTORY_SNAPSHOTS} snapshots"),
        ));
    }
    let parents = history
        .get("parents")
        .and_then(serde_json::Value::as_array)
        .filter(|parents| parents.len() == snapshot_values.len())
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
    let current = history
        .get("current")
        .and_then(serde_json::Value::as_u64)
        .and_then(|current| usize::try_from(current).ok())
        .filter(|current| *current < snapshot_values.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "history current is invalid"))?;
    let encoding = history
        .get("encoding")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_data("history encoding is required"))?;
    if encoding != "delta-tree-v3" {
        return Err(invalid_data("history encoding is unsupported"));
    }
    let snapshots = snapshot_values
        .iter()
        .cloned()
        .map(|snapshot| {
            serde_json::from_value::<CompactSnapshot>(snapshot)
                .map_err(|error| invalid_data(format!("history snapshot is invalid: {error}")))
        })
        .collect::<io::Result<Vec<_>>>()?;
    validate_compact_snapshots(&snapshots, current)?;
    validate_snapshot_source(&snapshots[current], &history_source(project))?;
    let mut loaded = ProjectHistory {
        snapshots,
        parents,
        current,
    };
    loaded.update_current_metadata(project);
    if loaded.maximum_persisted_bytes(project)? as u64 > MAX_PROJECT_DOCUMENT_BYTES {
        return Err(invalid_data(
            "project history can exceed the document limit after checkout",
        ));
    }
    Ok(loaded)
}

pub(crate) fn project_document(project: &Project, history: &ProjectHistory) -> String {
    #[derive(Serialize)]
    struct PersistedHistory<'a> {
        encoding: &'static str,
        current: usize,
        snapshots: &'a [CompactSnapshot],
        parents: &'a [Option<usize>],
    }

    let mut document = project.to_json().into_bytes();
    assert_eq!(
        document.pop(),
        Some(b'}'),
        "validated project serializes to a JSON object"
    );
    document.extend_from_slice(b",\"history\":");
    serde_json::to_writer(
        &mut document,
        &PersistedHistory {
            encoding: "delta-tree-v3",
            current: history.current,
            snapshots: &history.snapshots,
            parents: &history.parents,
        },
    )
    .expect("project history serializes to JSON");
    document.extend_from_slice(b"}\n");
    String::from_utf8(document).expect("project history serialization is UTF-8")
}

#[derive(Clone, Deserialize, Serialize)]
struct SnapshotDelta {
    prefix: usize,
    suffix: usize,
    replacement: String,
    reverse_replacement_bytes: usize,
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
        reverse_replacement_bytes: serialized_string_bytes(&base[prefix..base.len() - suffix]),
    }
}

fn serialized_string_bytes(value: &str) -> usize {
    serde_json::to_string(value)
        .expect("a project source slice serializes as a JSON string")
        .len()
}

fn decimal_bytes(value: usize) -> usize {
    decimal_bytes_u64(value as u64)
}

fn decimal_bytes_u64(mut value: u64) -> usize {
    let mut bytes = 1;
    while value >= 10 {
        value /= 10;
        bytes += 1;
    }
    bytes
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

fn history_project(project: &Project) -> Project {
    let mut snapshot = project.clone();
    snapshot.version = 1;
    snapshot.edit_operations.clear();
    snapshot
}

fn history_source(project: &Project) -> String {
    history_project(project).to_json()
}

fn history_entry(project: &Project, index: usize, previous_edit_count: usize) -> HistoryEntry {
    let edit = (project.edits.len() > previous_edit_count)
        .then(|| project.edits.last())
        .flatten();
    if index == 0 {
        return HistoryEntry {
            version: project.version,
            edit_count: project.edits.len(),
            summary: "Initial project".to_owned(),
            source: "Project".to_owned(),
            prompt: None,
            start: None,
            end: None,
        };
    }
    if let Some(edit) = edit {
        let source = project
            .edit_operations
            .iter()
            .find(|operation| operation.project_version == project.version)
            .map_or("Gemini", |operation| operation.source.as_str());
        return HistoryEntry {
            version: project.version,
            edit_count: project.edits.len(),
            summary: edit.summary.clone(),
            source: source.to_owned(),
            prompt: Some(edit.prompt.clone()),
            start: Some(edit.start),
            end: Some(edit.end),
        };
    }
    HistoryEntry {
        version: project.version,
        edit_count: project.edits.len(),
        summary: "Manual project change".to_owned(),
        source: "Manual".to_owned(),
        prompt: None,
        start: None,
        end: None,
    }
}

fn validate_compact_snapshots(snapshots: &[CompactSnapshot], current: usize) -> io::Result<()> {
    for (index, snapshot) in snapshots.iter().enumerate() {
        if snapshot.source_bytes > MAX_PROJECT_BYTES {
            return Err(invalid_data(
                "project history snapshot exceeds the graph limit",
            ));
        }
        if index == current {
            if snapshot.base.is_some() || snapshot.delta.is_some() {
                return Err(invalid_data(
                    "project history current snapshot must be the delta-tree root",
                ));
            }
            continue;
        }
        if snapshot.base.is_none() || snapshot.delta.is_none() {
            return Err(invalid_data("project history snapshot is disconnected"));
        }
        let base = snapshot
            .base
            .and_then(|base| snapshots.get(base))
            .ok_or_else(|| invalid_data("project history snapshot base is invalid"))?;
        let delta = snapshot
            .delta
            .as_ref()
            .expect("connected snapshot has a delta");
        base.source_bytes
            .checked_sub(delta.prefix)
            .and_then(|bytes| bytes.checked_sub(delta.suffix))
            .ok_or_else(|| invalid_data("project history snapshot delta is out of bounds"))?;
        let target_bytes = delta
            .prefix
            .checked_add(delta.replacement.len())
            .and_then(|bytes| bytes.checked_add(delta.suffix))
            .ok_or_else(|| invalid_data("project history snapshot delta is out of bounds"))?;
        if target_bytes != snapshot.source_bytes {
            return Err(invalid_data(
                "project history snapshot delta length is invalid",
            ));
        }
        let reverse_replacement_bytes = base
            .source_bytes
            .checked_sub(delta.prefix)
            .and_then(|bytes| bytes.checked_sub(delta.suffix))
            .ok_or_else(|| invalid_data("project history snapshot delta is out of bounds"))?;
        if !(reverse_replacement_bytes + 2..=reverse_replacement_bytes * 2 + 2)
            .contains(&delta.reverse_replacement_bytes)
        {
            return Err(invalid_data(
                "project history reverse delta size is invalid",
            ));
        }
        let mut cursor = index;
        for _ in 0..snapshots.len() {
            if cursor == current {
                break;
            }
            cursor = snapshots
                .get(cursor)
                .and_then(|snapshot| snapshot.base)
                .filter(|base| *base < snapshots.len())
                .ok_or_else(|| invalid_data("project history snapshot base is invalid"))?;
        }
        if cursor != current {
            return Err(invalid_data("project history delta tree contains a cycle"));
        }
    }
    Ok(())
}

fn validate_snapshot_source(snapshot: &CompactSnapshot, source: &str) -> io::Result<()> {
    if source.len() != snapshot.source_bytes || source_checksum(source) != snapshot.checksum {
        return Err(invalid_data(
            "project history snapshot does not match its compact delta",
        ));
    }
    Ok(())
}

fn source_checksum(source: &str) -> u64 {
    source.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(crate) fn save_project_state(
    store: &ProjectStore,
    project: &Project,
    history: &ProjectHistory,
) -> io::Result<()> {
    if history.maximum_persisted_bytes(project)? as u64 > MAX_PROJECT_DOCUMENT_BYTES {
        return Err(invalid_data(
            "project history can exceed the document limit after checkout",
        ));
    }
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

    fn large_project(edit_count: usize) -> Project {
        let mut project = Project::demo();
        for index in 0..edit_count {
            project.edits.push(crate::model::Edit {
                id: 10_000 + index as u64,
                start: 0.0,
                end: 1.0,
                prompt: "x".repeat(crate::model::MAX_PROMPT_CHARACTERS),
                summary: "Large history fixture".to_owned(),
            });
        }
        Project::from_json(&project.to_json()).expect("large valid project")
    }

    fn escape_heavy_project(edit_count: usize) -> Project {
        let mut project = Project::demo();
        let prompt = "\\".repeat(crate::model::MAX_PROMPT_CHARACTERS);
        for index in 0..edit_count {
            project.edits.push(crate::model::Edit {
                id: 20_000 + index as u64,
                start: 0.0,
                end: 1.0,
                prompt: prompt.clone(),
                summary: "Escape-heavy history fixture".to_owned(),
            });
        }
        Project::from_json(&project.to_json()).expect("escape-heavy valid project")
    }

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
        let initial = studio.project().clone();
        let mut project = studio.project().clone();
        project.name = "Atomic revision".to_owned();
        project.version += 1;
        let mut history = ProjectHistory::new(initial.clone());
        history.push(&initial, &project).expect("append history");

        save_project_state(&store, &project, &history).expect("single-file state commit");

        assert_eq!(
            store.read().expect("committed project").to_json(),
            project.to_json()
        );
        let loaded = load_project_history(&path, &project).expect("embedded history");
        assert_eq!(loaded.current, 1);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.parent(1), Some(0));
        assert_eq!(
            loaded
                .checkout(0, &project)
                .expect("materialized initial state")
                .1
                .to_json(),
            initial.to_json()
        );
        assert!(
            store
                .read_source()
                .expect("project document")
                .contains("\"encoding\":\"delta-tree-v3\"")
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
        assert_eq!(history.len(), 1);
        assert_eq!(history.current, 0);
        load_project_history(store.path(), studio.project()).expect("repaired embedded history");
        fs::remove_dir_all(root).expect("remove history test directory");
    }

    #[test]
    fn editing_an_older_state_preserves_forward_history() {
        let initial = Project::initial();
        let mut history = ProjectHistory::new(initial.clone());
        let mut second = initial.clone();
        second.name = "Forward".to_owned();
        second.version += 1;
        history
            .push(&initial, &second)
            .expect("append forward history");

        let (selected_history, restored) = history
            .checkout(0, &second)
            .expect("select initial history");
        history = selected_history;
        let mut branch = restored.clone();
        branch.name = "Branch".to_owned();
        branch.version += 2;
        history
            .push(&restored, &branch)
            .expect("append branch history");

        assert_eq!(history.len(), 3);
        assert_eq!(history.parent(2), Some(0));
        assert_eq!(
            history
                .clone()
                .checkout(1, &branch)
                .expect("retained forward state")
                .1
                .name,
            "Forward"
        );
    }

    #[test]
    fn compact_history_round_trips_branched_unicode_projects() {
        let current = Project::demo();
        let mut history = ProjectHistory::new(current.clone());
        let mut second = current.clone();
        second.name = "Mix \u{2603}".to_owned();
        second.version += 1;
        history
            .push(&current, &second)
            .expect("append unicode snapshot");
        let (selected_history, restored) = history.checkout(0, &second).expect("select initial");
        history = selected_history;
        let mut branch = restored.clone();
        branch.name = "Branch".to_owned();
        branch.version += 2;
        history.push(&restored, &branch).expect("append branch");

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
        assert_eq!(
            loaded
                .checkout(1, &branch)
                .expect("load forward branch")
                .1
                .name,
            "Mix \u{2603}"
        );
        fs::remove_dir_all(root).expect("remove history test directory");
    }

    #[test]
    fn history_limits_are_enforced_before_mutating_state() {
        let project = Project::initial();
        let mut next = project.clone();
        next.version += 1;
        let mut count_limited = ProjectHistory::new(project.clone());
        let error = count_limited
            .push_with_limits(&project, &next, 1, u64::MAX)
            .expect_err("snapshot limit");
        assert!(error.to_string().contains("1 snapshots"));
        assert_eq!(count_limited.len(), 1);
        assert_eq!(count_limited.current, 0);

        let mut bytes_limited = ProjectHistory::new(project.clone());
        let error = bytes_limited
            .push_with_limits(&project, &next, 2, 1)
            .expect_err("document limit");
        assert!(error.to_string().contains("document limit"));
        assert_eq!(bytes_limited.len(), 1);
        assert_eq!(bytes_limited.current, 0);
        assert_eq!(bytes_limited.parents, vec![None]);
        assert!(bytes_limited.snapshots[0].base.is_none());

        let mut mismatched = ProjectHistory::new(project.clone());
        let mut wrong_current = project.clone();
        wrong_current.name = "Wrong current".to_owned();
        let error = mismatched
            .push(&wrong_current, &next)
            .expect_err("live project identity");
        assert!(error.to_string().contains("does not match"));
        assert_eq!(mismatched.len(), 1);
    }

    #[test]
    fn history_rejects_an_orientation_that_checkout_cannot_persist() {
        let initial = Project::initial();
        let large = escape_heavy_project(3_500);
        let mut unbounded = ProjectHistory::new(initial.clone());
        unbounded
            .push_with_limits(&initial, &large, 2, u64::MAX)
            .expect("construct near-limit history fixture");
        let current_bytes = project_document(&large, &unbounded).len() as u64;
        assert!(current_bytes < MAX_PROJECT_DOCUMENT_BYTES);
        let metadata_reserve = unbounded.navigation_metadata_reserve(&large);
        let maximum_bytes = unbounded
            .maximum_persisted_bytes(&large)
            .expect("maximum persisted size");
        assert!(maximum_bytes as u64 > MAX_PROJECT_DOCUMENT_BYTES);
        let root = std::env::temp_dir().join(format!(
            "daw-ai-root-independent-history-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("root-independent history test directory");
        let (store, _) =
            ProjectStore::open(root.join("sound-graph.json")).expect("history test store");
        let error = save_project_state(&store, &large, &unbounded)
            .expect_err("common save must reject an unsafe retained root");
        assert!(error.to_string().contains("after checkout"));

        let (rerooted, restored) = unbounded
            .checkout(0, &large)
            .expect("materialize oversized orientation");
        let rerooted_bytes = project_document(&restored, &rerooted).len();
        assert_eq!(maximum_bytes - metadata_reserve, rerooted_bytes);
        assert!(rerooted_bytes as u64 > MAX_PROJECT_DOCUMENT_BYTES);

        let mut bounded = ProjectHistory::new(initial.clone());
        let error = bounded
            .push(&initial, &large)
            .expect_err("unselectable history must be rejected");
        assert!(error.to_string().contains("after checkout"));
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded.current, 0);
        fs::remove_dir_all(root).expect("remove root-independent history test directory");
    }

    #[test]
    fn compact_history_does_not_duplicate_large_unchanged_graphs() {
        let project = large_project(512);
        let mut next = project.clone();
        next.version += 1;
        let mut history = ProjectHistory::new(project.clone());
        history.push(&project, &next).expect("append large project");

        let document = project_document(&next, &history);
        assert!(document.len() < next.to_json().len() + 2_048);
        assert_eq!(history.stored_delta_bytes(), 0);
    }

    #[test]
    fn many_large_snapshots_stay_compact_in_memory_and_on_disk() {
        let mut current = large_project(512);
        let initial_source = history_source(&current);
        let mut history = ProjectHistory::new(current.clone());
        for _ in 0..64 {
            let mut next = current.clone();
            next.version += 1;
            next.tracks[0].muted = !next.tracks[0].muted;
            history.push(&current, &next).expect("compact append");
            current = next;
        }

        assert_eq!(history.len(), 65);
        assert!(history.stored_delta_bytes() < 4_096);
        assert!(project_document(&current, &history).len() < current.to_json().len() + 128 * 1024);

        let source_bytes = history_source(&current).len();
        let peak_limit = source_bytes * 2 + 4_096;
        assert!(source_bytes * history.len() > peak_limit);
        let mut direct = history.clone();
        let restored = direct
            .reroot_with_peak_limit(0, &current, peak_limit)
            .expect("direct checkout with bounded peak source memory");
        assert_eq!(history_source(&restored), initial_source);

        for _ in 0..16 {
            let previous = history.parent(history.current).expect("previous snapshot");
            let (selected_history, mut restored) = history
                .checkout(previous, &current)
                .expect("lazy one-step checkout");
            history = selected_history;
            restored.version = current.version + 1;
            history.update_current_metadata(&restored);
            current = restored;
        }
        let retained = history.len();
        let mut branch = current.clone();
        branch.name = "Stress branch".to_owned();
        branch.version += 1;
        history.push(&current, &branch).expect("stress branch");
        assert_eq!(history.len(), retained + 1);
    }

    #[test]
    fn previous_history_encoding_is_rejected() {
        let current = Project::demo();
        let history = ProjectHistory::new(current.clone());
        let mut document =
            serde_json::from_str::<serde_json::Value>(&project_document(&current, &history))
                .expect("current project document");
        document["history"]["encoding"] = "delta-tree-v2".into();
        let root = std::env::temp_dir().join(format!(
            "daw-ai-unsupported-history-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("unsupported history test directory");
        let path = root.join("sound-graph.json");
        fs::write(&path, format!("{document}\n")).expect("unsupported history");

        let error = match load_project_history(&path, &current) {
            Ok(_) => panic!("unsupported history encoding should be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unsupported"));
        fs::remove_dir_all(root).expect("remove unsupported history test directory");
    }
}
