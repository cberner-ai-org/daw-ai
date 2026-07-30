use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::concurrency::RecoverPoison;
use crate::gemini::EDIT_TIMEOUT_SECONDS;
use crate::http::Response;

const MAX_ACTIVE_EDIT_JOBS: usize = 1;
const MAX_RETAINED_EDIT_JOBS: usize = 64;

#[derive(Debug)]
pub(crate) enum EditJobCreateError {
    Capacity,
    Entropy(getrandom::Error),
}

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

impl EditJob {
    fn is_active(&self) -> bool {
        matches!(
            &self.state,
            EditJobState::Queued | EditJobState::Running { .. }
        )
    }
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
    ) -> Result<(u64, String, bool), EditJobCreateError> {
        let mut jobs = self.lock();
        if let Some(operation_id) = requested_operation_id
            && let Some((id, job)) = jobs
                .iter()
                .find(|(_, job)| job.operation_id == operation_id)
        {
            return Ok((*id, job.operation_id.clone(), false));
        }
        let active_jobs = jobs.values().filter(|job| job.worker_active).count();
        if active_jobs >= MAX_ACTIVE_EDIT_JOBS {
            return Err(EditJobCreateError::Capacity);
        }
        while jobs.len() >= MAX_RETAINED_EDIT_JOBS {
            let Some(id) = jobs.iter().find_map(|(id, job)| {
                matches!(
                    &job.state,
                    EditJobState::Completed { .. } | EditJobState::Failed { .. }
                )
                .then_some(*id)
            }) else {
                return Err(EditJobCreateError::Capacity);
            };
            jobs.remove(&id);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let operation_id = requested_operation_id.map(str::to_owned).map_or_else(
            || new_operation_id().map_err(EditJobCreateError::Entropy),
            Ok,
        )?;
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

    pub(crate) fn set_running(&self, id: u64, phase: &'static str, detail: impl Into<String>) {
        if let Some(job) = self.lock().get_mut(&id) {
            if !job.is_active() {
                return;
            }
            job.state = EditJobState::Running {
                phase,
                detail: detail.into(),
            };
        }
    }

    pub(crate) fn publish_update(&self, id: u64, project_version: u64, summary: &str) {
        if let Some(job) = self.lock().get_mut(&id) {
            if !job.is_active() {
                return;
            }
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
            if !job.is_active() {
                return;
            }
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "finalizing",
                detail: "Gemini finished the sound graph edit".to_owned(),
            };
        }
    }

    pub(crate) fn complete(&self, id: u64, message: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            if !job.is_active() {
                return;
            }
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Completed { message };
            job.worker_active = false;
        }
    }

    pub(crate) fn fail(&self, id: u64, status: u16, error: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            if !job.is_active() {
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
        self.jobs.lock().recover_poison()
    }
}

pub(crate) fn new_operation_id() -> Result<String, getrandom::Error> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)?;
    let mut token = String::with_capacity(32);
    for byte in random {
        write!(token, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(token)
}

pub(crate) fn accepted_edit_job_json(id: u64, operation_id: &str, poll_after_ms: u64) -> String {
    serde_json::json!({
        "id": id.to_string(),
        "operationId": operation_id,
        "status": "queued",
        "phase": "queued",
        "detail": "Waiting for the edit worker",
        "elapsedSeconds": 0,
        "appliedSteps": 0,
        "projectVersion": null,
        "timeoutSeconds": EDIT_TIMEOUT_SECONDS,
        "pollAfterMs": poll_after_ms
    })
    .to_string()
}

fn edit_job_json(id: u64, job: &EditJob) -> String {
    let ended_at = job.finished_at.unwrap_or_else(Instant::now);
    let elapsed = ended_at.saturating_duration_since(job.started_at).as_secs();
    let mut response = serde_json::json!({
        "id": id.to_string(),
        "operationId": job.operation_id,
        "elapsedSeconds": elapsed,
        "timeoutSeconds": EDIT_TIMEOUT_SECONDS,
        "appliedSteps": job.applied_steps,
        "projectVersion": job.project_version
    });
    match &job.state {
        EditJobState::Queued => {
            response["status"] = "queued".into();
            response["phase"] = "queued".into();
            response["detail"] = "Waiting for the edit worker".into();
            response["pollAfterMs"] = job.poll_after_ms.into();
        }
        EditJobState::Running { phase, detail } => {
            response["status"] = "running".into();
            response["phase"] = (*phase).into();
            response["detail"] = detail.clone().into();
            response["pollAfterMs"] = job.poll_after_ms.into();
        }
        EditJobState::Completed { message } => {
            response["status"] = "completed".into();
            response["phase"] = "completed".into();
            response["message"] = message.clone().into();
        }
        EditJobState::Failed { status, error } => {
            response["status"] = "failed".into();
            response["phase"] = "failed".into();
            response["errorStatus"] = (*status).into();
            response["error"] = error.clone().into();
        }
    }
    response.to_string()
}
