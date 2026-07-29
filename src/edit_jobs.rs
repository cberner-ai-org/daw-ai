use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::gemini::EDIT_TIMEOUT_SECONDS;
use crate::http::{Response, valid_user_id};
use crate::model::json_string;

pub(crate) const MAX_ACTIVE_EDIT_JOBS: usize = 4;
const MAX_RETAINED_EDIT_JOBS: usize = 64;

pub(crate) struct EditJobs {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<u64, EditJob>>,
}

struct EditJob {
    operation_id: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    poll_after_ms: u64,
    applied_steps: usize,
    project_version: Option<u64>,
    state: EditJobState,
    interrupted: bool,
    cancellation: Arc<AtomicBool>,
    worker_active: bool,
}

enum EditJobState {
    Queued,
    Running { phase: &'static str, detail: String },
    Completed { message: String },
    Failed { status: u16, error: String },
}

impl EditJobs {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn has_active(&self) -> bool {
        self.lock().values().any(|job| job.worker_active)
    }

    pub(crate) fn create(
        &self,
        poll_after_ms: u64,
        requested_operation_id: Option<&str>,
    ) -> Result<(u64, String, bool), ()> {
        let mut jobs = self.lock();
        if let Some(operation_id) = requested_operation_id {
            if let Some((id, job)) = jobs
                .iter()
                .find(|(_, job)| job.operation_id == operation_id)
            {
                return Ok((*id, job.operation_id.clone(), false));
            }
        }
        let active_jobs = jobs.values().filter(|job| job.worker_active).count();
        if active_jobs >= MAX_ACTIVE_EDIT_JOBS {
            return Err(());
        }
        while jobs.len() >= MAX_RETAINED_EDIT_JOBS {
            let Some(id) = jobs.iter().find_map(|(id, job)| {
                matches!(
                    &job.state,
                    EditJobState::Completed { .. } | EditJobState::Failed { .. }
                )
                .then_some(*id)
            }) else {
                return Err(());
            };
            jobs.remove(&id);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let operation_id = requested_operation_id
            .map(str::to_owned)
            .unwrap_or_else(|| new_operation_id(id));
        jobs.insert(
            id,
            EditJob {
                operation_id: operation_id.clone(),
                started_at: Instant::now(),
                finished_at: None,
                poll_after_ms,
                applied_steps: 0,
                project_version: None,
                state: EditJobState::Queued,
                interrupted: false,
                cancellation: Arc::new(AtomicBool::new(false)),
                worker_active: true,
            },
        );
        Ok((id, operation_id, true))
    }

    pub(crate) fn response_for_operation(&self, operation_id: &str) -> Option<Response> {
        let id = self
            .lock()
            .iter()
            .find(|(_, job)| job.operation_id == operation_id)
            .map(|(id, _)| *id)?;
        self.response(id)
    }

    pub(crate) fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    pub(crate) fn set_running(&self, id: u64, phase: &'static str, detail: impl Into<String>) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.state = EditJobState::Running {
                phase,
                detail: detail.into(),
            };
        }
    }

    pub(crate) fn publish_update(&self, id: u64, project_version: u64, summary: &str) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.applied_steps += 1;
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "editing",
                detail: format!("Applied step {}: {summary}", job.applied_steps),
            };
        }
    }

    pub(crate) fn finalize_updates(&self, id: u64, project_version: u64) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "finalizing",
                detail: "Gemini finished the sound graph edit".to_owned(),
            };
        }
    }

    pub(crate) fn complete(&self, id: u64, message: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            if job.interrupted {
                return;
            }
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Completed { message };
            job.worker_active = false;
        }
    }

    pub(crate) fn fail(&self, id: u64, status: u16, error: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            if job.interrupted {
                return;
            }
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Failed { status, error };
            job.worker_active = false;
        }
    }

    pub(crate) fn worker_finished(&self, id: u64) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.worker_active = false;
        }
    }

    pub(crate) fn interrupt(&self, id: u64) -> bool {
        let mut jobs = self.lock();
        let Some(job) = jobs.get_mut(&id) else {
            return false;
        };
        if !matches!(
            job.state,
            EditJobState::Queued | EditJobState::Running { .. }
        ) {
            return false;
        }
        job.interrupted = true;
        job.cancellation.store(true, Ordering::SeqCst);
        job.finished_at = Some(Instant::now());
        job.state = EditJobState::Failed {
            status: 409,
            error: "Edit interrupted by the user.".to_owned(),
        };
        true
    }

    pub(crate) fn is_interrupted(&self, id: u64) -> bool {
        self.lock().get(&id).is_some_and(|job| job.interrupted)
    }

    pub(crate) fn cancellation(&self, id: u64) -> Arc<AtomicBool> {
        self.lock()
            .get(&id)
            .map(|job| Arc::clone(&job.cancellation))
            .expect("edit job must exist while its worker is running")
    }

    pub(crate) fn response(&self, id: u64) -> Option<Response> {
        self.lock()
            .get(&id)
            .map(|job| Response::json(200, edit_job_json(id, job)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, EditJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) fn new_operation_id(id: u64) -> String {
    let mut random = [0_u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .is_ok()
    {
        let mut token = String::with_capacity(32);
        for byte in random {
            write!(token, "{byte:02x}").expect("writing to a string cannot fail");
        }
        return token;
    }
    if let Ok(uuid) = fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let token = uuid
            .bytes()
            .filter(|byte| byte.is_ascii_hexdigit())
            .map(char::from)
            .collect::<String>();
        if valid_user_id(&token) {
            return token;
        }
    }
    fallback_operation_id(id)
}

pub(crate) fn fallback_operation_id(id: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let hash = |domain: u8| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        domain.hash(&mut hasher);
        nanos.hash(&mut hasher);
        std::process::id().hash(&mut hasher);
        std::thread::current().id().hash(&mut hasher);
        id.hash(&mut hasher);
        hasher.finish()
    };
    format!("{:016x}{:016x}", hash(0), hash(1))
}

pub(crate) fn accepted_edit_job_json(id: u64, operation_id: &str, poll_after_ms: u64) -> String {
    format!(
        concat!(
            "{{\"id\":\"{}\",\"operationId\":{},\"status\":\"queued\",\"phase\":\"queued\",",
            "\"detail\":\"Waiting for the edit worker\",\"elapsedSeconds\":0,",
            "\"appliedSteps\":0,\"projectVersion\":null,",
            "\"timeoutSeconds\":{},\"pollAfterMs\":{}}}"
        ),
        id,
        json_string(operation_id),
        EDIT_TIMEOUT_SECONDS,
        poll_after_ms
    )
}

fn edit_job_json(id: u64, job: &EditJob) -> String {
    let ended_at = job.finished_at.unwrap_or_else(Instant::now);
    let elapsed = ended_at.saturating_duration_since(job.started_at).as_secs();
    let project_version = job
        .project_version
        .map_or_else(|| "null".to_owned(), |version| version.to_string());
    let common = format!(
        concat!(
            "\"id\":\"{}\",\"operationId\":{},\"elapsedSeconds\":{},",
            "\"timeoutSeconds\":{},\"appliedSteps\":{},\"projectVersion\":{}"
        ),
        id,
        json_string(&job.operation_id),
        elapsed,
        EDIT_TIMEOUT_SECONDS,
        job.applied_steps,
        project_version
    );
    match &job.state {
        EditJobState::Queued => format!(
            concat!(
                "{{{},\"status\":\"queued\",\"phase\":\"queued\",",
                "\"detail\":\"Waiting for the edit worker\",",
                "\"pollAfterMs\":{}}}"
            ),
            common, job.poll_after_ms
        ),
        EditJobState::Running { phase, detail } => format!(
            concat!(
                "{{{},\"status\":\"running\",\"phase\":{},\"detail\":{},",
                "\"pollAfterMs\":{}}}"
            ),
            common,
            json_string(phase),
            json_string(detail),
            job.poll_after_ms
        ),
        EditJobState::Completed { message } => format!(
            "{{{},\"status\":\"completed\",\"phase\":\"completed\",\"message\":{}}}",
            common,
            json_string(message)
        ),
        EditJobState::Failed { status, error } => format!(
            "{{{},\"status\":\"failed\",\"phase\":\"failed\",\"errorStatus\":{},\"error\":{}}}",
            common,
            status,
            json_string(error)
        ),
    }
}
