use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value as JsonValue;

use crate::model::Project;
use crate::prompt::EditPlan;
use crate::storage::{MAX_PROJECT_BYTES, read_bounded_text, replace_text_file};

pub(crate) const GRAPH_FILE: &str = "sound-graph.json";
pub(crate) const REQUEST_FILE: &str = "request.json";
pub(crate) const SESSION_FILE: &str = "session.json";
const PROGRESS_DIRECTORY: &str = "edit-progress";
pub(crate) const PENDING_PROGRESS_DIRECTORY: &str = ".edit-progress.pending";
pub(crate) const PROGRESS_PLAN_FILE: &str = "plan.json";
pub(crate) const PROGRESS_GRAPH_FILE: &str = "project.json";
pub(crate) const UNDO_GRAPH_FILE: &str = "undo-sound-graph.json";
pub(crate) const UNDO_REQUEST_FILE: &str = "undo-request.json";
pub(crate) const MAX_SOUND_GRAPH_BYTES: u64 = MAX_PROJECT_BYTES as u64;
pub(crate) const MAX_SESSION_JSON_BYTES: u64 = 64 * 1024;
const MAX_PROGRESS_PLAN_BYTES: u64 = 64 * 1024;
const DEFAULT_SESSION_RETENTION_DAYS: u64 = 30;
const DEFAULT_SESSION_RETENTION_COUNT: usize = 100;
const DEFAULT_SESSION_RETENTION_BYTES: u64 = 512 * 1024 * 1024;
const RUNNING_SESSION_LEASE: Duration = Duration::from_secs(25 * 60);
pub(crate) static SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(crate) struct SessionRetention {
    pub(crate) maximum_age: Duration,
    pub(crate) maximum_count: usize,
    pub(crate) maximum_bytes: u64,
}

impl SessionRetention {
    fn configured() -> Self {
        Self {
            maximum_age: Duration::from_secs(
                configured_u64(
                    "DAW_AI_GEMINI_SESSION_RETENTION_DAYS",
                    DEFAULT_SESSION_RETENTION_DAYS,
                )
                .saturating_mul(24 * 60 * 60),
            ),
            maximum_count: configured_u64(
                "DAW_AI_GEMINI_SESSION_RETENTION_COUNT",
                DEFAULT_SESSION_RETENTION_COUNT as u64,
            ) as usize,
            maximum_bytes: configured_u64(
                "DAW_AI_GEMINI_SESSION_RETENTION_BYTES",
                DEFAULT_SESSION_RETENTION_BYTES,
            ),
        }
    }
}

pub(crate) struct EditSession {
    path: PathBuf,
    persistent: bool,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SessionVariants {
    pub(crate) new_prompt: bool,
}

impl EditSession {
    #[cfg(test)]
    pub(crate) fn create(
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
    ) -> io::Result<Self> {
        Self::create_in(
            &session_root(),
            project,
            prompt,
            start,
            end,
            SessionVariants::default(),
        )
    }

    pub(crate) fn create_in(
        root: &Path,
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
        variants: SessionVariants,
    ) -> io::Result<Self> {
        apply_session_retention_with(root, SessionRetention::configured())?;
        let path = reserve_session_directory(root)?;
        let result = (|| {
            write_new(&path.join(GRAPH_FILE), &project.to_json())?;
            write_new(
                &path.join(REQUEST_FILE),
                &serde_json::json!({
                    "start": start,
                    "end": end,
                    "prompt": prompt
                })
                .to_string(),
            )?;
            write_new(
                &path.join(SESSION_FILE),
                &serde_json::json!({
                    "id": path.file_name().unwrap_or_default().to_string_lossy(),
                    "createdAt": unix_milliseconds(),
                    "updatedAt": unix_milliseconds(),
                    "status": "running",
                    "model": crate::gemini::GEMINI_MODEL,
                    "prompt": prompt,
                    "start": start,
                    "end": end,
                    "appliedSteps": 0,
                    "audioListens": 0,
                    "newPrompt": variants.new_prompt,
                    "detail": "Gemini session started"
                })
                .to_string(),
            )?;
            Ok(Self {
                path: path.clone(),
                persistent: !cfg!(test),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&path);
        }
        result
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn synchronize_project(&self, project: &Project) -> Result<(), String> {
        write_replace(
            &self.path.join(GRAPH_FILE),
            &format!("{}\n", project.to_json()),
        )
        .map_err(|error| format!("could not synchronize committed sound graph: {error}"))
    }

    pub(crate) fn record_exchange(
        &self,
        name: &str,
        request: &JsonValue,
        response: &str,
    ) -> io::Result<()> {
        write_new(
            &self.path.join(format!("{name}-request.json")),
            &request.to_string(),
        )?;
        write_new(&self.path.join(format!("{name}-response.json")), response)
    }

    pub(crate) fn record_audio(&self, sequence: usize, wav: &[u8]) -> io::Result<String> {
        let name = format!("audio-{sequence:03}.wav");
        write_new_with(&self.path.join(&name), |file| file.write_all(wav))?;
        Ok(name)
    }

    pub(crate) fn update_status(
        &self,
        status: &str,
        detail: &str,
        applied_steps: usize,
        audio_listens: usize,
    ) -> io::Result<()> {
        let source = self.metadata_source()?;
        self.update_status_from(&source, status, detail, applied_steps, audio_listens)
    }

    pub(crate) fn update_status_from(
        &self,
        source: &str,
        status: &str,
        detail: &str,
        applied_steps: usize,
        audio_listens: usize,
    ) -> io::Result<()> {
        let path = self.path.join(SESSION_FILE);
        let mut value = serde_json::from_str::<JsonValue>(source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid session record"))?;
        object.insert("status".to_owned(), JsonValue::String(status.to_owned()));
        object.insert("detail".to_owned(), JsonValue::String(detail.to_owned()));
        object.insert("updatedAt".to_owned(), unix_milliseconds().into());
        object.insert("appliedSteps".to_owned(), applied_steps.into());
        object.insert("audioListens".to_owned(), audio_listens.into());
        write_replace(&path, &value.to_string())
    }

    pub(crate) fn metadata_source(&self) -> io::Result<String> {
        read_bounded_text(
            &self.path.join(SESSION_FILE),
            MAX_SESSION_JSON_BYTES,
            "Gemini session metadata",
        )
    }

    pub(crate) fn update_metrics(&self, metrics: &JsonValue) -> io::Result<()> {
        let source = self.metadata_source()?;
        let mut value = serde_json::from_str::<JsonValue>(&source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        value
            .as_object_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid session record"))?
            .insert("metrics".to_owned(), metrics.clone());
        write_replace(&self.path.join(SESSION_FILE), &value.to_string())
    }

    pub(crate) fn stats(&self) -> io::Result<(usize, usize)> {
        let source = self.metadata_source()?;
        let value = serde_json::from_str::<JsonValue>(&source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let applied_steps = value
            .get("appliedSteps")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        let audio_listens = value
            .get("audioListens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        Ok((applied_steps, audio_listens))
    }

    pub(crate) fn finish(&self, plans: Vec<EditPlan>) -> Result<(EditPlan, Project), String> {
        let mut summary = None;
        for plan in plans {
            summary = Some(plan.summary);
        }
        if summary.is_none() {
            return Err("Gemini did not use a registered graph mutation tool".to_owned());
        }
        let graph = read_bounded_text(
            &self.path.join(GRAPH_FILE),
            MAX_SOUND_GRAPH_BYTES,
            "Gemini sound graph",
        )
        .map_err(|error| format!("could not read Gemini sound graph: {error}"))?;
        let project = Project::from_json(&graph)
            .map_err(|error| format!("Gemini left an invalid sound graph: {error}"))?;
        Ok((
            EditPlan {
                summary: summary.expect("plans were nonempty"),
            },
            project,
        ))
    }

    pub(crate) fn take_update(&self) -> Result<Option<(EditPlan, Project)>, String> {
        let path = progress_path(&self.path);
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("could not inspect Gemini edit progress: {error}"));
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Gemini edit progress handoff is not a regular directory".to_owned());
        }
        let plan_source = read_bounded_text(
            &path.join(PROGRESS_PLAN_FILE),
            MAX_PROGRESS_PLAN_BYTES,
            "Gemini edit plan progress",
        )
        .map_err(|error| format!("could not read Gemini edit plan progress: {error}"))?;
        let graph_source = read_bounded_text(
            &path.join(PROGRESS_GRAPH_FILE),
            MAX_SOUND_GRAPH_BYTES,
            "Gemini sound graph progress",
        )
        .map_err(|error| format!("could not read Gemini sound graph progress: {error}"))?;
        let plan = if let Some(summary) = serde_json::from_str::<JsonValue>(&plan_source)
            .ok()
            .filter(|value| value.get("graphMutation") == Some(&JsonValue::Bool(true)))
            .and_then(|value| {
                value
                    .get("summary")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
            }) {
            EditPlan { summary }
        } else {
            return Err("Gemini edit progress did not contain a graph mutation".to_owned());
        };
        let project = Project::from_json(&graph_source)
            .map_err(|error| format!("Gemini edit progress is invalid: {error}"))?;
        fs::remove_dir_all(&path)
            .map_err(|error| format!("could not consume Gemini edit progress: {error}"))?;
        Ok(Some((plan, project)))
    }
}

impl Drop for EditSession {
    fn drop(&mut self) {
        if self.persistent {
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("warning: could not remove Gemini test session: {error}");
        }
    }
}

pub(crate) fn wait_for_progress_handoff(session_path: &Path) {
    let path = progress_path(session_path);
    while path.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn publish_progress(
    session_path: &Path,
    plan: &str,
    project: &Project,
) -> Result<(), String> {
    let pending = session_path.join(PENDING_PROGRESS_DIRECTORY);
    let published = progress_path(session_path);
    let result = (|| {
        fs::create_dir(&pending)
            .map_err(|error| format!("could not prepare Gemini edit progress: {error}"))?;
        write_new(&pending.join(PROGRESS_PLAN_FILE), plan)
            .map_err(|error| format!("could not record Gemini edit plan progress: {error}"))?;
        write_new(&pending.join(PROGRESS_GRAPH_FILE), &project.to_json())
            .map_err(|error| format!("could not record Gemini sound graph progress: {error}"))?;
        fs::rename(&pending, &published)
            .map_err(|error| format!("could not publish Gemini edit progress: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&pending);
    }
    result
}

pub(crate) fn progress_path(session_path: &Path) -> PathBuf {
    session_path.join(PROGRESS_DIRECTORY)
}

fn reserve_session_directory(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    set_private_directory(root)?;
    for _ in 0..64 {
        let id = SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            "{}-{}-{id}",
            unix_milliseconds(),
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(error) = set_private_directory(&path) {
                    let _ = fs::remove_dir(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a Gemini edit session",
    ))
}

#[cfg(test)]
pub(crate) fn session_root() -> PathBuf {
    if let Some(path) =
        std::env::var_os("DAW_AI_GEMINI_SESSION_DIR").filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("DAW_AI_PROJECT_PATH").filter(|path| !path.is_empty())
        && let Some(parent) = Path::new(&path).parent()
    {
        return parent.join("gemini-sessions");
    }
    std::env::temp_dir().join(format!("daw-ai-gemini-tests-{}", std::process::id()))
}

pub(crate) fn session_root_for_project(project_path: &Path) -> PathBuf {
    let root = std::env::var_os("DAW_AI_GEMINI_SESSION_DIR").filter(|path| !path.is_empty());
    session_root_for_project_with_override(project_path, root.as_deref())
}

fn session_root_for_project_with_override(
    project_path: &Path,
    override_root: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(root) = override_root {
        let project_directory = project_path.parent().unwrap_or_else(|| Path::new("."));
        let namespace = if project_directory
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "users")
        {
            project_directory.file_name().unwrap_or_default()
        } else {
            std::ffi::OsStr::new("default")
        };
        return PathBuf::from(root).join(namespace);
    }
    project_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("gemini-sessions")
}

#[cfg(test)]
pub(crate) fn session_summaries() -> io::Result<Vec<JsonValue>> {
    session_summaries_in(&session_root())
}

pub(crate) fn session_summaries_in(root: &Path) -> io::Result<Vec<JsonValue>> {
    // Visible session state must always reflect lease reconciliation and retention.
    apply_session_retention(root)?;
    let Some(entries) = tolerate_missing(fs::read_dir(root))? else {
        return Ok(Vec::new());
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let Some(entry) = tolerate_missing(entry)? else {
            continue;
        };
        let path = entry.path().join(SESSION_FILE);
        if !path.is_file() {
            continue;
        }
        let Ok(source) =
            read_bounded_text(&path, MAX_SESSION_JSON_BYTES, "Gemini session metadata")
        else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<JsonValue>(&source) else {
            continue;
        };
        sessions.push(value);
    }
    sessions.sort_by_key(|session| {
        std::cmp::Reverse(
            session
                .get("createdAt")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
        )
    });
    sessions.truncate(100);
    Ok(sessions)
}

pub(crate) fn apply_session_retention(root: &Path) -> io::Result<()> {
    apply_session_retention_with(root, SessionRetention::configured())
}

struct RetainedSession {
    path: PathBuf,
    updated: SystemTime,
    running: bool,
    bytes: u64,
}

pub(crate) fn apply_session_retention_with(
    root: &Path,
    policy: SessionRetention,
) -> io::Result<()> {
    let Some(entries) = tolerate_missing(fs::read_dir(root))? else {
        return Ok(());
    };
    let mut sessions = Vec::new();
    let now = SystemTime::now();
    for entry in entries {
        let Some(entry) = tolerate_missing(entry)? else {
            continue;
        };
        let Some(file_type) = tolerate_missing(entry.file_type())? else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let metadata_path = entry.path().join(SESSION_FILE);
        let Some(mut metadata) = read_bounded_text(
            &metadata_path,
            MAX_SESSION_JSON_BYTES,
            "Gemini session metadata",
        )
        .ok()
        .and_then(|source| serde_json::from_str::<JsonValue>(&source).ok())
        .filter(|metadata| valid_session_metadata(&entry.path(), metadata)) else {
            continue;
        };
        let mut running = metadata.get("status").and_then(JsonValue::as_str) == Some("running");
        let mut updated = metadata
            .get("updatedAt")
            .and_then(JsonValue::as_u64)
            .map(|milliseconds| UNIX_EPOCH + Duration::from_millis(milliseconds))
            .unwrap_or(UNIX_EPOCH);
        if running && now.duration_since(updated).unwrap_or_default() > RUNNING_SESSION_LEASE {
            let object = metadata
                .as_object_mut()
                .expect("validated session metadata is an object");
            object.insert("status".to_owned(), JsonValue::String("failed".to_owned()));
            object.insert(
                "detail".to_owned(),
                JsonValue::String("Session abandoned after the edit worker stopped".to_owned()),
            );
            object.insert("updatedAt".to_owned(), unix_milliseconds().into());
            write_replace(&metadata_path, &metadata.to_string())?;
            running = false;
            updated = now;
        }
        sessions.push(RetainedSession {
            bytes: directory_bytes(&entry.path())?,
            path: entry.path(),
            updated,
            running,
        });
    }
    sessions.sort_by_key(|session| session.updated);
    let mut total_bytes = sessions.iter().map(|session| session.bytes).sum::<u64>();

    for session in sessions.iter_mut().filter(|session| !session.running) {
        let expired = now.duration_since(session.updated).unwrap_or_default() > policy.maximum_age;
        let over_budget = total_bytes > policy.maximum_bytes;
        if !expired && !over_budget {
            continue;
        }
        let Some(entries) = tolerate_missing(fs::read_dir(&session.path))? else {
            continue;
        };
        for entry in entries {
            let Some(entry) = tolerate_missing(entry)? else {
                continue;
            };
            let Some(file_type) = tolerate_missing(entry.file_type())? else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let is_audio = entry.path().extension().and_then(|value| value.to_str()) == Some("wav");
            if is_audio {
                let Some(metadata) = tolerate_missing(entry.metadata())? else {
                    continue;
                };
                let bytes = metadata.len();
                let _ = tolerate_missing(fs::remove_file(entry.path()))?;
                session.bytes = session.bytes.saturating_sub(bytes);
                total_bytes = total_bytes.saturating_sub(bytes);
            }
        }
    }

    let mut retained_count = sessions.len();
    for session in sessions.iter().filter(|session| !session.running) {
        let expired = now.duration_since(session.updated).unwrap_or_default() > policy.maximum_age;
        if !expired && retained_count <= policy.maximum_count && total_bytes <= policy.maximum_bytes
        {
            continue;
        }
        let _ = tolerate_missing(fs::remove_dir_all(&session.path))?;
        retained_count = retained_count.saturating_sub(1);
        total_bytes = total_bytes.saturating_sub(session.bytes);
    }
    Ok(())
}

fn valid_session_metadata(path: &Path, metadata: &JsonValue) -> bool {
    let directory_id = path.file_name().and_then(|name| name.to_str());
    metadata.get("id").and_then(JsonValue::as_str) == directory_id
        && metadata
            .get("createdAt")
            .and_then(JsonValue::as_u64)
            .is_some()
        && metadata
            .get("updatedAt")
            .and_then(JsonValue::as_u64)
            .is_some()
        && matches!(
            metadata.get("status").and_then(JsonValue::as_str),
            Some("running" | "completed" | "failed")
        )
        && path.join(GRAPH_FILE).is_file()
        && path.join(REQUEST_FILE).is_file()
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    let Some(entries) = tolerate_missing(fs::read_dir(path))? else {
        return Ok(0);
    };
    let mut bytes = 0_u64;
    for entry in entries {
        let Some(entry) = tolerate_missing(entry)? else {
            continue;
        };
        bytes = bytes.saturating_add(directory_entry_bytes(&entry)?);
    }
    Ok(bytes)
}

fn directory_entry_bytes(entry: &fs::DirEntry) -> io::Result<u64> {
    let Some(metadata) = tolerate_missing(entry.path().symlink_metadata())? else {
        return Ok(0);
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_dir() {
        directory_bytes(&entry.path())
    } else {
        Ok(metadata.len())
    }
}

fn tolerate_missing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn configured_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn unix_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn write_new(path: &Path, source: &str) -> io::Result<()> {
    write_new_with(path, |file| {
        file.write_all(source.as_bytes())?;
        file.write_all(b"\n")
    })
}

fn write_new_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = write(&mut file).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

pub(crate) fn write_replace(path: &Path, source: &str) -> io::Result<()> {
    replace_text_file(path, &format!("{}\n", source.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_metadata_records_current_prompt_variant() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-session-variants-{}-{}",
            std::process::id(),
            SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let session = EditSession::create_in(
            &root,
            &Project::initial(),
            "test prompt variant",
            0.0,
            1.0,
            SessionVariants { new_prompt: true },
        )
        .expect("session with prompt variant");

        let metadata: JsonValue =
            serde_json::from_str(&session.metadata_source().expect("session metadata"))
                .expect("session metadata JSON");
        assert_eq!(metadata["newPrompt"], true);
        assert!(metadata.get("slimPrompt").is_none());
        assert!(metadata.get("dynamicToolLoading").is_none());
        assert!(metadata.get("requireAnalysis").is_none());
        assert!(metadata.get("batchParameterTools").is_none());
        fs::remove_dir_all(root).expect("remove session test directory");
    }

    #[test]
    fn directory_sizing_ignores_disappearing_entries() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-disappearing-session-artifact-{}-{}",
            std::process::id(),
            SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("artifact test directory");
        let artifact = root.join("edit-progress");
        fs::create_dir(&artifact).expect("temporary artifact directory");
        fs::write(artifact.join("plan.json"), b"temporary").expect("temporary artifact");
        let entry = fs::read_dir(&root)
            .expect("artifact directory")
            .next()
            .expect("artifact entry")
            .expect("read artifact entry");
        fs::remove_dir_all(&artifact).expect("remove temporary artifact");

        assert_eq!(directory_entry_bytes(&entry).expect("disappeared entry"), 0);
        assert_eq!(
            directory_bytes(&artifact).expect("disappeared directory"),
            0
        );
        fs::remove_dir_all(root).expect("remove artifact test directory");
    }

    #[test]
    fn listing_reconciles_an_abandoned_running_session() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-listed-abandoned-session-{}-{}",
            std::process::id(),
            SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let abandoned = root.join("abandoned");
        fs::create_dir_all(&abandoned).expect("abandoned session");
        write_new(
            &abandoned.join(SESSION_FILE),
            r#"{"id":"abandoned","status":"running","createdAt":1,"updatedAt":1}"#,
        )
        .expect("abandoned metadata");
        write_new(&abandoned.join(GRAPH_FILE), "{}").expect("session graph marker");
        write_new(&abandoned.join(REQUEST_FILE), "{}").expect("session request marker");

        let sessions = session_summaries_in(&root).expect("reconciled session summaries");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["status"], "failed");
        assert!(
            sessions[0]["detail"]
                .as_str()
                .is_some_and(|value| value.contains("abandoned"))
        );
        let saved: JsonValue = serde_json::from_str(
            &fs::read_to_string(abandoned.join(SESSION_FILE)).expect("saved session metadata"),
        )
        .expect("saved session JSON");
        assert_eq!(saved["status"], "failed");
        fs::remove_dir_all(root).expect("remove session test directory");
    }

    #[test]
    fn configured_session_roots_are_namespaced_per_user() {
        let configured = std::ffi::OsStr::new("/sessions");
        assert_eq!(
            session_root_for_project_with_override(
                Path::new("/state/users/user-one/sound-graph.json"),
                Some(configured),
            ),
            Path::new("/sessions/user-one")
        );
        assert_eq!(
            session_root_for_project_with_override(
                Path::new("/state/users/user-two/sound-graph.json"),
                Some(configured),
            ),
            Path::new("/sessions/user-two")
        );
        assert_eq!(
            session_root_for_project_with_override(
                Path::new("/state/sound-graph.json"),
                Some(configured),
            ),
            Path::new("/sessions/default")
        );
    }
}
