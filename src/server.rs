use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio_analysis;
use crate::audio_stream::{
    ByteRange, WAV_HEADER_BYTES, bounded_audio_byte_range, wait_for_playback_window,
};
use crate::concurrency::Limiter;
use crate::gemini::{EDIT_TIMEOUT_SECONDS, GeminiEdit, GeminiPlanner, PlannerError};
use crate::gemini_tools::render_audio_request_with_backend;
use crate::model::{ChannelOperationAction, Project, Studio, StudioError, json_string};
#[cfg(test)]
use crate::project_history::{MAX_HISTORY_BYTES, load_project_history};
use crate::project_history::{
    ProjectHistory, open_project_with_history, project_document, save_project_state,
    trim_project_history,
};
use crate::prompt::{EditPlan, PromptEngine};
use crate::storage::{ProjectStore, replace_text_file};

const MAX_REQUEST_BYTES: usize = 6 * 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;
const MAX_ACTIVE_EDIT_JOBS: usize = 4;
const MAX_RETAINED_EDIT_JOBS: usize = 64;
const AUDIO_REQUEST_HEADER: &str = "x-daw-ai-audio";
const AUDIO_RANGE_SAMPLES: usize =
    (audio_analysis::MAX_REGION_SECONDS * audio_analysis::SAMPLE_RATE as f32) as usize;
const PLAYBACK_CHUNK_SAMPLES: usize = audio_analysis::SAMPLE_RATE as usize * 2;
const AUDIO_STREAM_LOOKAHEAD_SAMPLES: usize = PLAYBACK_CHUNK_SAMPLES * 2;
const GEMINI_POLL_INTERVAL_MS: u64 = 1_000;
const DEMO_POLL_INTERVAL_MS: u64 = 25;
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub fn run(port: u16) -> io::Result<()> {
    install_shutdown_handlers();
    let router = Router::new(port)?;
    let address = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&address)?;
    listener.set_nonblocking(true)?;
    println!("DAW-AI is ready at http://{address}");
    println!("Sound graph: {}", router.project_path().display());
    let connections = Limiter::new(MAX_ACTIVE_CONNECTIONS);

    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let Some(permit) = connections.acquire() else {
                    let _ = Response::json(503, error_json("server is busy; retry shortly"))
                        .write(&mut stream);
                    continue;
                };
                let router = router.clone();
                if let Err(error) =
                    thread::Builder::new()
                        .name("daw-ai-http".to_owned())
                        .spawn(move || {
                            let _permit = permit;
                            if let Err(error) = serve_connection(&mut stream, &router) {
                                eprintln!("request failed: {error}");
                            }
                        })
                {
                    eprintln!("error: outcome=request_thread_rejected error={error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn install_shutdown_handlers() {
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    unsafe extern "C" {
        fn signal(signal: i32, handler: unsafe extern "C" fn(i32)) -> usize;
    }

    unsafe extern "C" fn request_shutdown(_signal: i32) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }

    let _ = unsafe { signal(SIGINT, request_shutdown) };
    let _ = unsafe { signal(SIGTERM, request_shutdown) };
}

#[cfg(not(unix))]
fn install_shutdown_handlers() {}

fn serve_connection(stream: &mut TcpStream, router: &Router) -> io::Result<()> {
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let response = match Request::read(stream) {
        Ok(request) => {
            let (scoped, new_user) = if request_needs_user_scope(&request) {
                match router.scoped(&request) {
                    Ok(scoped) => scoped,
                    Err(error) => return scope_error_response(&error).write(stream),
                }
            } else {
                (router.clone(), None)
            };
            let new_user_cookie = new_user.as_ref().map(|user_id| {
                format!("daw_ai_user={user_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000")
            });
            if request.path == "/api/export.wav" {
                return scoped.write_export(&request, stream, new_user_cookie.as_deref());
            }
            if request.path.starts_with("/api/audio-stream/") {
                let cancellation_stream = stream.try_clone()?;
                return scoped.write_playback_stream_with_cancel(
                    &request,
                    stream,
                    || stream_disconnected(&cancellation_stream),
                    new_user_cookie.as_deref(),
                );
            }
            let mut response = scoped.handle(&request);
            response.set_cookie = new_user_cookie;
            log_http_response(request_id, started, &request, &response);
            response
        }
        Err(error) => {
            eprintln!(
                "warning: http request_id={request_id} outcome=rejected latency_ms={} error={}",
                started.elapsed().as_millis(),
                single_line(&error)
            );
            Response::json(400, error_json(&error))
        }
    };
    response.write(stream)
}

fn scope_error_response(error: &io::Error) -> Response {
    let status = match error.kind() {
        io::ErrorKind::PermissionDenied => 401,
        io::ErrorKind::ResourceBusy => 503,
        io::ErrorKind::StorageFull => 507,
        _ => 500,
    };
    let mut response = Response::json(status, error_json(&error.to_string()));
    if error.kind() == io::ErrorKind::PermissionDenied {
        response.set_cookie =
            Some("daw_ai_user=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_owned());
    }
    response
}

fn stream_disconnected(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0_u8; 1];
    let result = stream.peek(&mut byte);
    if stream.set_nonblocking(false).is_err() {
        return true;
    }
    match result {
        Ok(0) => true,
        Ok(_) => false,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

fn audio_byte_range(value: &str, total_length: usize) -> Result<ByteRange, ()> {
    bounded_audio_byte_range(value, total_length, AUDIO_RANGE_SAMPLES * 2)
}

fn instrument_parameter_path(path: &str) -> Option<(u64, &str)> {
    let suffix = path.strip_prefix("/api/instrument-parameters/")?;
    let (track_id, group) = suffix.split_once('/')?;
    if group.contains('/') {
        return None;
    }
    Some((track_id.parse().ok()?, group))
}

#[derive(Clone)]
struct Router {
    studio: Arc<Mutex<Studio>>,
    store: Option<ProjectStore>,
    planner: Planner,
    edit_jobs: Arc<EditJobs>,
    audio_renderer: Arc<AudioRenderer>,
    audio_token: Arc<String>,
    users: Option<Arc<UserRegistry>>,
    history: Arc<Mutex<ProjectHistory>>,
    builtin_backend: Arc<AtomicBool>,
}

struct UserRegistry {
    root: PathBuf,
    planner: Planner,
    users: Mutex<HashMap<String, CachedUser>>,
}

struct CachedUser {
    router: Router,
    last_used: Instant,
    last_persisted: Instant,
}

const MAX_CACHED_USERS: usize = 64;
const MAX_PERSISTED_USERS: usize = 256;
const USER_CACHE_IDLE: Duration = Duration::from_secs(60 * 60);
const USER_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const USER_USE_PERSIST_INTERVAL: Duration = Duration::from_secs(60 * 60);
const USER_LAST_USED_FILE: &str = ".last-used";

fn persist_user_use(directory: &std::path::Path) -> io::Result<()> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    replace_text_file(
        &directory.join(USER_LAST_USED_FILE),
        &format!("{milliseconds}\n"),
    )
}

fn persisted_user_use(directory: &std::path::Path) -> io::Result<SystemTime> {
    let marker = directory.join(USER_LAST_USED_FILE);
    if let Ok(source) = fs::read_to_string(&marker) {
        if let Ok(milliseconds) = source.trim().parse::<u64>() {
            return Ok(UNIX_EPOCH + Duration::from_millis(milliseconds));
        }
    }
    fs::metadata(directory.join("sound-graph.json").as_path())
        .or_else(|_| fs::metadata(directory))?
        .modified()
}

fn expire_persisted_users(
    root: &std::path::Path,
    cached: &HashMap<String, CachedUser>,
) -> io::Result<()> {
    let now = SystemTime::now();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() || cached.contains_key(&id) {
            continue;
        }
        let last_used = persisted_user_use(&entry.path())?;
        if now.duration_since(last_used).unwrap_or_default() >= USER_RETENTION {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn request_needs_user_scope(request: &Request) -> bool {
    matches!(
        request.path.as_str(),
        "/api/project"
            | "/api/gemini-sessions"
            | "/api/history"
            | "/api/backend"
            | "/api/edits"
            | "/api/channels"
            | "/api/mix"
            | "/api/sound-tools"
            | "/api/undo"
            | "/api/reset"
            | "/api/audio-access"
            | "/api/export.wav"
    ) || request.path.starts_with("/api/edits/")
        || request.path.starts_with("/api/instrument-parameters/")
        || request.path.starts_with("/api/edit-operations/")
        || (request.path.starts_with("/api/audio-stream/") && request.user_id().is_some())
}

fn expire_and_bound_user_cache(users: &mut HashMap<String, CachedUser>) {
    users.retain(|_, user| user.last_used.elapsed() < USER_CACHE_IDLE || !user.can_evict());
    while users.len() >= MAX_CACHED_USERS {
        let Some(oldest) = users
            .iter()
            .filter(|(_, user)| user.can_evict())
            .min_by_key(|(_, user)| user.last_used)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        users.remove(&oldest);
    }
}

impl CachedUser {
    fn can_evict(&self) -> bool {
        Arc::strong_count(&self.router.studio) == 1 && !self.router.edit_jobs.has_active()
    }
}

#[derive(Clone)]
enum Planner {
    Gemini,
    Demo,
    #[cfg(test)]
    GatedDemo(Arc<PlannerGate>),
}

#[cfg(test)]
struct PlannerGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

#[cfg(test)]
impl PlannerGate {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn wait_until_released(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_until_started(&self) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.0, "planner did not reach the test gate");
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 = true;
        self.changed.notify_all();
    }
}

struct EditJobs {
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

struct EditRequest {
    operation_id: String,
    prompt: String,
    start: f32,
    end: f32,
    project: crate::model::Project,
}

struct EditFailure {
    status: u16,
    message: String,
}

#[derive(Default)]
struct AudioRenderState {
    rendering: bool,
}

#[derive(Default)]
struct AudioRenderer {
    state: Mutex<AudioRenderState>,
    completed: Condvar,
}

#[derive(Debug)]
enum AudioRenderError {
    Render(String),
    Cancelled,
}

impl AudioRenderer {
    fn stream_sample_range(
        &self,
        project: &crate::model::Project,
        start_sample: usize,
        end_sample: usize,
        builtin: bool,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<audio_analysis::AudioRegion, AudioRenderError> {
        self.stream_region_with(
            project,
            start_sample,
            end_sample,
            is_cancelled,
            |project, start, end| {
                if builtin {
                    audio_analysis::render_project_sample_range_builtin(project, start, end)
                } else {
                    audio_analysis::render_project_sample_range(project, start, end)
                }
            },
        )
    }

    fn stream_region_with(
        &self,
        project: &crate::model::Project,
        start_sample: usize,
        end_sample: usize,
        is_cancelled: &impl Fn() -> bool,
        render: impl FnOnce(
            &crate::model::Project,
            usize,
            usize,
        ) -> Result<audio_analysis::AudioRegion, String>,
    ) -> Result<audio_analysis::AudioRegion, AudioRenderError> {
        loop {
            if is_cancelled() {
                return Err(AudioRenderError::Cancelled);
            }
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.rendering {
                drop(
                    self.completed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            } else {
                state.rendering = true;
                break;
            }
        }

        if is_cancelled() {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.rendering = false;
            self.completed.notify_all();
            return Err(AudioRenderError::Cancelled);
        }
        let rendered = render(project, start_sample, end_sample);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.rendering = false;
        self.completed.notify_all();
        rendered.map_err(AudioRenderError::Render)
    }
}

impl EditJobs {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    fn has_active(&self) -> bool {
        self.lock().values().any(|job| job.worker_active)
    }

    fn create(
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

    fn response_for_operation(&self, operation_id: &str) -> Option<Response> {
        let id = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(_, job)| job.operation_id == operation_id)
            .map(|(id, _)| *id)?;
        self.response(id)
    }

    fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    fn set_running(&self, id: u64, phase: &'static str, detail: impl Into<String>) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.state = EditJobState::Running {
                phase,
                detail: detail.into(),
            };
        }
    }

    fn publish_update(&self, id: u64, project_version: u64, summary: &str) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.applied_steps += 1;
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "editing",
                detail: format!("Applied step {}: {summary}", job.applied_steps),
            };
        }
    }

    fn finalize_updates(&self, id: u64, project_version: u64) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "finalizing",
                detail: "Gemini finished the sound graph edit".to_owned(),
            };
        }
    }

    fn complete(&self, id: u64, message: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            if job.interrupted {
                return;
            }
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Completed { message };
            job.worker_active = false;
        }
    }

    fn fail(&self, id: u64, status: u16, error: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            if job.interrupted {
                return;
            }
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Failed { status, error };
            job.worker_active = false;
        }
    }

    fn worker_finished(&self, id: u64) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.worker_active = false;
        }
    }

    fn interrupt(&self, id: u64) -> bool {
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

    fn is_interrupted(&self, id: u64) -> bool {
        self.lock().get(&id).is_some_and(|job| job.interrupted)
    }

    fn cancellation(&self, id: u64) -> Arc<AtomicBool> {
        self.lock()
            .get(&id)
            .map(|job| Arc::clone(&job.cancellation))
            .expect("edit job must exist while its worker is running")
    }

    fn response(&self, id: u64) -> Option<Response> {
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

impl EditFailure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

fn planner_failure(error: PlannerError) -> EditFailure {
    let status = match error {
        PlannerError::ProjectChanged => 409,
        PlannerError::SaveFailed => 500,
        _ => 503,
    };
    EditFailure::new(status, error.to_string())
}

impl Router {
    fn write_export(
        &self,
        request: &Request,
        output: &mut impl Write,
        set_cookie: Option<&str>,
    ) -> io::Result<()> {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host")).write(output);
        };
        if request.method != "GET" || !request.is_trusted_request(public_host) {
            return Response::json(405, error_json("method not allowed")).write(output);
        }
        let project = self.lock_studio().project().clone();
        let builtin_backend = self.builtin_backend.load(Ordering::SeqCst);
        let sample_count = audio_analysis::playback_sample_count(0.0, project.duration);
        let total_length = WAV_HEADER_BYTES.saturating_add(sample_count.saturating_mul(2));
        write_response_head(
            output,
            200,
            "audio/wav",
            total_length,
            &[
                ("Cache-Control", "no-store"),
                ("Content-Disposition", "attachment; filename=project.wav"),
            ],
            set_cookie,
        )?;
        output.write_all(&audio_analysis::wav_header(sample_count))?;
        let mut cursor = 0;
        while cursor < sample_count {
            let end = (cursor + AUDIO_RANGE_SAMPLES).min(sample_count);
            let region = match self.audio_renderer.stream_sample_range(
                &project,
                cursor,
                end,
                builtin_backend,
                &|| false,
            ) {
                Ok(region) => region,
                Err(AudioRenderError::Render(error)) => {
                    eprintln!("error: could not render export: {error}");
                    return Err(io::Error::other("could not render export"));
                }
                Err(AudioRenderError::Cancelled) => return Ok(()),
            };
            output.write_all(&audio_analysis::pcm_bytes(&region.samples))?;
            cursor = end;
        }
        Ok(())
    }

    fn new(_port: u16) -> io::Result<Self> {
        let planner = match std::env::var("DAW_AI_PROMPT_ENGINE") {
            Ok(value) if value == "demo" => Planner::Demo,
            _ => Planner::Gemini,
        };
        let project_path = std::env::var_os(crate::storage::PROJECT_PATH_ENV)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?.join("sound-graph.json"));
        let root = project_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("users");
        fs::create_dir_all(&root)?;
        let (store, studio, mut history) = open_project_with_history(project_path)?;
        trim_project_history(studio.project(), &mut history);
        save_project_state(&store, studio.project(), &history)?;
        let users = Arc::new(UserRegistry {
            root,
            planner: planner.clone(),
            users: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            history: Arc::new(Mutex::new(history)),
            builtin_backend: Arc::new(AtomicBool::new(false)),
            studio: Arc::new(Mutex::new(studio)),
            store: Some(store),
            planner,
            edit_jobs: Arc::new(EditJobs::new()),
            audio_renderer: Arc::new(AudioRenderer::default()),
            audio_token: Arc::new(new_operation_id(0)),
            users: Some(users),
        })
    }

    #[cfg(test)]
    fn demo() -> Self {
        Self {
            history: Arc::new(Mutex::new(ProjectHistory::new(
                Studio::new().project().clone(),
            ))),
            builtin_backend: Arc::new(AtomicBool::new(false)),
            studio: Arc::new(Mutex::new(Studio::new())),
            store: None,
            planner: Planner::Demo,
            edit_jobs: Arc::new(EditJobs::new()),
            audio_renderer: Arc::new(AudioRenderer::default()),
            audio_token: Arc::new("test-audio-token".to_owned()),
            users: None,
        }
    }

    fn scoped(&self, request: &Request) -> io::Result<(Self, Option<String>)> {
        let Some(registry) = &self.users else {
            return Ok((self.clone(), None));
        };
        let existing = request.user_id();
        if let Some(existing) = existing {
            let directory = registry.root.join(existing);
            if !directory.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unknown user session; clear the site cookie and try again",
                ));
            }
        }
        let user_id = existing
            .map(str::to_owned)
            .unwrap_or_else(|| new_operation_id(0));
        let mut users = registry
            .users
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        users.retain(|_, user| user.last_used.elapsed() < USER_CACHE_IDLE || !user.can_evict());
        if let Some(user) = users.get_mut(&user_id) {
            user.last_used = Instant::now();
            if user.last_persisted.elapsed() >= USER_USE_PERSIST_INTERVAL {
                persist_user_use(
                    user.router
                        .project_path()
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new(".")),
                )?;
                user.last_persisted = Instant::now();
            }
            return Ok((user.router.clone(), existing.is_none().then_some(user_id)));
        }
        expire_and_bound_user_cache(&mut users);
        if users.len() >= MAX_CACHED_USERS {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "all cached user projects have active edits",
            ));
        }
        let directory = registry.root.join(&user_id);
        fs::create_dir_all(&registry.root)?;
        if !directory.is_dir() {
            let persisted_count = fs::read_dir(&registry.root)?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .take(MAX_PERSISTED_USERS + 1)
                .count();
            if persisted_count >= MAX_PERSISTED_USERS {
                expire_persisted_users(&registry.root, &users)?;
                if fs::read_dir(&registry.root)?
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .take(MAX_PERSISTED_USERS + 1)
                    .count()
                    >= MAX_PERSISTED_USERS
                {
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        "persistent user project limit reached",
                    ));
                }
            }
        }
        fs::create_dir_all(&directory)?;
        let project_path = directory.join("sound-graph.json");
        if !project_path.exists() {
            let studio = Studio::new();
            let history = ProjectHistory::new(studio.project().clone());
            replace_text_file(&project_path, &project_document(studio.project(), &history))?;
        }
        let (store, studio, mut history) = open_project_with_history(project_path)?;
        trim_project_history(studio.project(), &mut history);
        save_project_state(&store, studio.project(), &history)?;
        persist_user_use(&directory)?;
        let router = Self {
            history: Arc::new(Mutex::new(history)),
            builtin_backend: Arc::new(AtomicBool::new(false)),
            studio: Arc::new(Mutex::new(studio)),
            store: Some(store),
            planner: registry.planner.clone(),
            edit_jobs: Arc::new(EditJobs::new()),
            audio_renderer: Arc::new(AudioRenderer::default()),
            audio_token: Arc::new(new_operation_id(0)),
            users: None,
        };
        users.insert(
            user_id.clone(),
            CachedUser {
                router: router.clone(),
                last_used: Instant::now(),
                last_persisted: Instant::now(),
            },
        );
        Ok((router, existing.is_none().then_some(user_id)))
    }

    fn handle(&self, request: &Request) -> Response {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host"));
        };
        if request.is_mutation() && !request.is_trusted_mutation(public_host) {
            return Response::json(403, error_json("cross-origin request rejected"));
        }
        if request.path == "/api/audio-access" {
            if request.method != "GET" {
                return Response::json(405, error_json("method not allowed"))
                    .with_header("Allow", "GET");
            }
            if !request.is_trusted_audio(public_host) {
                return Response::json(403, error_json("cross-origin audio request rejected"));
            }
            return Response::json(
                200,
                format!("{{\"streamToken\":{}}}", json_string(&self.audio_token)),
            );
        }
        if request.path.starts_with("/api/audio") {
            return Response::json(404, error_json("audio endpoint not found"));
        }
        if let Some(operation_id) = edit_operation_id(&request.path) {
            return if request.method == "GET" {
                self.edit_operation_status(operation_id)
            } else {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            };
        }
        if request.path.starts_with("/api/edit-operations/") {
            return Response::json(404, error_json("edit operation not found"));
        }
        if let Some(job_id) = interrupted_edit_job_id(&request.path) {
            return if request.method == "POST" {
                if self.edit_jobs.interrupt(job_id) {
                    self.edit_jobs.response(job_id).expect("interrupted job")
                } else {
                    Response::json(409, error_json("edit job is not interruptible"))
                }
            } else {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "POST")
            };
        }
        if let Some(job_id) = edit_job_id(&request.path) {
            return if request.method == "GET" {
                self.edit_status(job_id)
            } else {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            };
        }
        if request.path.starts_with("/api/edits/") {
            return Response::json(404, error_json("edit job not found"));
        }
        if let Some((track_id, group)) = instrument_parameter_path(&request.path) {
            return if request.method == "GET" {
                self.instrument_parameters(track_id, group)
            } else {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            };
        }

        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => Response::static_asset("text/html; charset=utf-8", INDEX_HTML),
            ("GET", "/app.css") => Response::static_asset("text/css; charset=utf-8", APP_CSS),
            ("GET", "/app.js") => Response::static_asset("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/api/health") => Response::json(200, "{\"status\":\"ok\"}".to_owned()),
            ("GET", "/api/project") => {
                let studio = self.lock_studio();
                self.project_response(&studio)
            }
            ("GET", "/api/gemini-sessions") => self.gemini_sessions(),
            ("GET", "/api/history") => self.history_response(),
            ("GET", "/api/backend") => self.backend_response(),
            ("POST", "/api/edits") => self.start_edit(&request.body),
            ("POST", "/api/channels") => self.change_channel(&request.body),
            ("POST", "/api/mix") => self.change_mix(&request.body),
            ("POST", "/api/sound-tools") => self.change_sound_tool(&request.body),
            ("POST", "/api/logs") => Self::client_log(&request.body),
            ("POST", "/api/undo") => self.undo(),
            ("POST", "/api/reset") => self.reset(),
            ("POST", "/api/history") => self.select_history(&request.body),
            ("POST", "/api/backend") => self.set_backend(&request.body),
            (
                _,
                "/api/edits" | "/api/channels" | "/api/mix" | "/api/sound-tools" | "/api/logs"
                | "/api/undo" | "/api/reset",
            ) => Response::json(405, error_json("method not allowed")).with_header("Allow", "POST"),
            (_, "/api/project" | "/api/health" | "/api/gemini-sessions") => {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            }
            (_, "/api/history") => Response::json(405, error_json("method not allowed"))
                .with_header("Allow", "GET, POST"),
            (_, "/api/backend") => Response::json(405, error_json("method not allowed"))
                .with_header("Allow", "GET, POST"),
            _ => Response::json(404, error_json("not found")),
        }
    }

    fn start_edit(&self, body: &str) -> Response {
        let form = parse_form(body);
        let operation_id = form.get("operation_id").map(String::as_str);
        if operation_id.is_some_and(|operation_id| !valid_operation_id(operation_id)) {
            return Response::json(422, error_json("operation ID is invalid"));
        }
        if let Some(response) = operation_id
            .and_then(|operation_id| self.edit_jobs.response_for_operation(operation_id))
        {
            return response;
        }
        let Some(prompt) = form.get("prompt") else {
            return Response::json(422, error_json("prompt is required"));
        };
        let Some(start) = form
            .get("start")
            .and_then(|value| value.parse::<f32>().ok())
        else {
            return Response::json(422, error_json("selection start is required"));
        };
        let Some(end) = form.get("end").and_then(|value| value.parse::<f32>().ok()) else {
            return Response::json(422, error_json("selection end is required"));
        };
        let project = {
            let studio = self.lock_studio();
            if let Err(error) = studio.validate_edit(start, end, prompt) {
                return Response::json(422, studio_error(error));
            }
            studio.project().clone()
        };
        if let Some(operation_id) = operation_id {
            if let Some(operation) = project
                .edit_operations
                .iter()
                .find(|operation| operation.operation_id == operation_id)
            {
                return Response::json(200, recovered_operation_json(operation));
            }
        }
        let poll_after_ms = match &self.planner {
            Planner::Gemini => GEMINI_POLL_INTERVAL_MS,
            Planner::Demo => DEMO_POLL_INTERVAL_MS,
            #[cfg(test)]
            Planner::GatedDemo(_) => DEMO_POLL_INTERVAL_MS,
        };
        let Ok((job_id, operation_id, created)) =
            self.edit_jobs.create(poll_after_ms, operation_id)
        else {
            return Response::json(503, error_json("too many edits are already being planned"));
        };
        if !created {
            return self
                .edit_jobs
                .response(job_id)
                .expect("an existing edit job has a response");
        }
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: prompt.to_owned(),
            start,
            end,
            project,
        };
        let worker = self.clone();
        let spawn = thread::Builder::new()
            .name(format!("daw-ai-edit-{job_id}"))
            .spawn(move || worker.run_edit_job(job_id, edit));
        if let Err(error) = spawn {
            self.edit_jobs.remove(job_id);
            eprintln!("error: could not start edit worker: {error}");
            return Response::json(503, error_json("could not start the edit worker"));
        }
        Response::json(
            202,
            accepted_edit_job_json(job_id, &operation_id, poll_after_ms),
        )
    }

    #[cfg(test)]
    fn write_playback_stream(&self, request: &Request, output: &mut impl Write) -> io::Result<()> {
        self.write_playback_stream_with_cancel(request, output, || false, None)
    }

    fn write_playback_stream_with_cancel(
        &self,
        request: &Request,
        output: &mut impl Write,
        is_cancelled: impl Fn() -> bool,
        set_cookie: Option<&str>,
    ) -> io::Result<()> {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host")).write(output);
        };
        if request.method != "GET" {
            return Response::json(405, error_json("method not allowed"))
                .with_header("Allow", "GET")
                .write(output);
        }
        let Some((token, version, start_milliseconds)) = playback_audio_stream(&request.path)
        else {
            return Response::json(404, error_json("audio stream not found")).write(output);
        };
        if token != self.audio_token.as_str() || !request.is_trusted_request(public_host) {
            return Response::json(403, error_json("cross-origin audio request rejected"))
                .write(output);
        }

        let project = self.lock_studio().project().clone();
        if version != project.version {
            return Response::json(409, error_json("project changed before playback started"))
                .write(output);
        }
        let stream_start_sample =
            audio_analysis::playback_start_sample_milliseconds(start_milliseconds);
        let project_end_sample = audio_analysis::playback_sample_count(0.0, project.duration);
        if stream_start_sample >= project_end_sample {
            return Response::json(422, error_json("playback start is outside the project"))
                .write(output);
        }
        if is_cancelled() {
            return Ok(());
        }

        let sample_count = project_end_sample - stream_start_sample;
        let total_length = WAV_HEADER_BYTES.saturating_add(sample_count.saturating_mul(2));
        if let Some(range_value) = request.headers.get("range") {
            let range = match audio_byte_range(range_value, total_length) {
                Ok(range) => range,
                Err(()) => {
                    let content_range = format!("bytes */{total_length}");
                    return write_response_head(
                        output,
                        416,
                        "audio/wav",
                        0,
                        &[
                            ("Cache-Control", "no-store"),
                            ("Accept-Ranges", "bytes"),
                            ("Content-Range", content_range.as_str()),
                        ],
                        set_cookie,
                    );
                }
            };
            let content_range = format!("bytes {}-{}/{total_length}", range.start, range.end);
            write_response_head(
                output,
                206,
                "audio/wav",
                range.len(),
                &[
                    ("Cache-Control", "no-store"),
                    ("Accept-Ranges", "bytes"),
                    ("Content-Range", content_range.as_str()),
                ],
                set_cookie,
            )?;
            return self.write_playback_byte_range(
                &project,
                stream_start_sample,
                sample_count,
                range,
                output,
                &is_cancelled,
            );
        }

        write_response_head(
            output,
            200,
            "audio/wav",
            total_length,
            &[("Cache-Control", "no-store"), ("Accept-Ranges", "bytes")],
            set_cookie,
        )?;
        output.write_all(&audio_analysis::wav_header(sample_count))?;

        let mut remaining = sample_count;
        let mut cursor = stream_start_sample;
        let stream_started = Instant::now();
        let stream_id = new_operation_id(version);
        let mut bytes_written = WAV_HEADER_BYTES;
        while remaining > 0 {
            let next_region_samples = remaining.min(PLAYBACK_CHUNK_SAMPLES);
            let generated_samples = cursor - stream_start_sample + next_region_samples;
            if !wait_for_playback_window(
                generated_samples,
                AUDIO_STREAM_LOOKAHEAD_SAMPLES,
                audio_analysis::SAMPLE_RATE,
                stream_started,
                &is_cancelled,
            ) {
                eprintln!(
                    "audio_stream id={stream_id} outcome=cancelled version={version} bytes_written={bytes_written}"
                );
                return Ok(());
            }
            let end = cursor + next_region_samples;
            let region = match self.audio_renderer.stream_sample_range(
                &project,
                cursor,
                end,
                self.builtin_backend.load(Ordering::SeqCst),
                &is_cancelled,
            ) {
                Ok(rendered) => rendered,
                Err(AudioRenderError::Render(error)) => {
                    eprintln!(
                        "error: audio_stream id={stream_id} outcome=render_failed version={version} bytes_written={bytes_written}: {error}"
                    );
                    return Err(io::Error::other("could not render playback stream"));
                }
                Err(AudioRenderError::Cancelled) => return Ok(()),
            };
            let count = region.samples.len().min(remaining);
            if is_cancelled() {
                return Ok(());
            }
            let pcm = audio_analysis::pcm_bytes(&region.samples[..count]);
            output.write_all(&pcm)?;
            bytes_written += pcm.len();
            remaining -= count;
            cursor = end;
        }
        eprintln!(
            "audio_stream id={stream_id} outcome=completed version={version} bytes_written={bytes_written} elapsed_ms={}",
            stream_started.elapsed().as_millis()
        );
        Ok(())
    }

    fn write_playback_byte_range(
        &self,
        project: &crate::model::Project,
        stream_start_sample: usize,
        sample_count: usize,
        range: ByteRange,
        output: &mut impl Write,
        is_cancelled: &impl Fn() -> bool,
    ) -> io::Result<()> {
        let header = audio_analysis::wav_header(sample_count);
        let mut cursor = range.start;
        if cursor < WAV_HEADER_BYTES {
            let header_end = (range.end + 1).min(WAV_HEADER_BYTES);
            output.write_all(&header[cursor..header_end])?;
            cursor = header_end;
        }
        if cursor > range.end || is_cancelled() {
            return Ok(());
        }

        let pcm_start = cursor - WAV_HEADER_BYTES;
        let pcm_end = range.end + 1 - WAV_HEADER_BYTES;
        let first_sample = pcm_start / 2;
        let end_sample = pcm_end.div_ceil(2);
        let region = match self.audio_renderer.stream_sample_range(
            project,
            stream_start_sample + first_sample,
            stream_start_sample + end_sample,
            self.builtin_backend.load(Ordering::SeqCst),
            is_cancelled,
        ) {
            Ok(region) => region,
            Err(AudioRenderError::Render(error)) => {
                eprintln!("error: could not render playback range: {error}");
                return Err(io::Error::other("could not render playback range"));
            }
            Err(AudioRenderError::Cancelled) => return Ok(()),
        };
        if is_cancelled() {
            return Ok(());
        }
        let pcm = audio_analysis::pcm_bytes(&region.samples);
        let first_sample_byte = first_sample * 2;
        output.write_all(&pcm[pcm_start - first_sample_byte..pcm_end - first_sample_byte])
    }

    fn edit_status(&self, job_id: u64) -> Response {
        self.edit_jobs
            .response(job_id)
            .unwrap_or_else(|| Response::json(404, error_json("edit job not found")))
    }

    fn edit_operation_status(&self, operation_id: &str) -> Response {
        if !valid_operation_id(operation_id) {
            return Response::json(404, error_json("edit operation not found"));
        }
        if let Some(response) = self.edit_jobs.response_for_operation(operation_id) {
            return response;
        }
        self.lock_studio()
            .project()
            .edit_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .map_or_else(
                || Response::json(404, error_json("edit operation not found")),
                |operation| Response::json(200, recovered_operation_json(operation)),
            )
    }

    fn run_edit_job(&self, job_id: u64, edit: EditRequest) {
        let operation_id = edit.operation_id.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.perform_edit(job_id, edit)
        }));
        match result {
            Ok(Ok(message)) => self.edit_jobs.complete(job_id, message),
            Ok(Err(failure)) => {
                eprintln!(
                    "error: edit_job job_id={job_id} operation_id={} status={} outcome=failed error={}",
                    operation_id,
                    failure.status,
                    single_line(&failure.message)
                );
                self.persist_failed_operation(
                    &operation_id,
                    self.edit_jobs.is_interrupted(job_id),
                    &failure.message,
                );
                self.edit_jobs.fail(job_id, failure.status, failure.message);
            }
            Err(_) => {
                let message = "the edit worker stopped unexpectedly".to_owned();
                eprintln!(
                    "error: edit_job job_id={job_id} operation_id={} status=500 outcome=panicked error={message}",
                    operation_id
                );
                self.persist_failed_operation(&operation_id, false, &message);
                self.edit_jobs.fail(job_id, 500, message);
            }
        }
        self.edit_jobs.worker_finished(job_id);
    }

    fn persist_failed_operation(&self, operation_id: &str, interrupted: bool, message: &str) {
        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        if !candidate.mark_operation_failed(operation_id, interrupted, message) {
            return;
        }
        if self.commit_metadata(&mut studio, candidate).is_err() {
            eprintln!(
                "error: operation_id={} could not persist terminal partial-edit state",
                operation_id
            );
        }
    }

    fn perform_edit(&self, job_id: u64, edit: EditRequest) -> Result<String, EditFailure> {
        self.edit_jobs.set_running(
            job_id,
            "planning",
            "Gemini is planning, editing, and listening to the sound graph",
        );
        if matches!(&self.planner, Planner::Gemini) {
            return self.perform_gemini_edit(job_id, edit);
        }
        let plan = self
            .plan_edit(&edit.prompt, edit.start, edit.end, &edit.project)
            .map_err(|message| EditFailure::new(503, message))?;
        if self.edit_jobs.is_interrupted(job_id) {
            return Err(EditFailure::new(409, "edit interrupted by the user"));
        }
        self.edit_jobs.set_running(
            job_id,
            "applying",
            "Validating and saving the updated sound graph",
        );
        let mut studio = self.lock_studio();
        if studio.project().version != edit.project.version {
            return Err(EditFailure::new(
                409,
                "the project changed; submit the edit again",
            ));
        }
        let mut candidate = studio.clone();
        let summary = candidate
            .apply_plan_for_operation(edit.start, edit.end, &edit.prompt, edit.operation_id, plan)
            .map_err(|error| EditFailure::new(422, studio_error_message(error)))?;
        self.commit(&mut studio, candidate)
            .map_err(|_| EditFailure::new(500, "could not save the sound graph"))?;
        Ok(summary)
    }

    fn perform_gemini_edit(&self, job_id: u64, edit: EditRequest) -> Result<String, EditFailure> {
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        let cancellation = self.edit_jobs.cancellation(job_id);
        let completed = GeminiPlanner::interpret_with_audio_renderer_updates(
            &self.gemini_session_root(),
            &edit.prompt,
            edit.start,
            edit.end,
            &edit.project,
            cancellation,
            |request| {
                self.edit_jobs.set_running(
                    job_id,
                    "rendering",
                    "Rendering the current sound graph with the Rust audio engine",
                );
                let result = render_audio_request_with_backend(
                    request,
                    self.builtin_backend.load(Ordering::SeqCst),
                );
                self.edit_jobs.set_running(
                    job_id,
                    "planning",
                    "Gemini is listening to the backend audio render",
                );
                result
            },
            |graph_edit| {
                self.commit_gemini_update(
                    job_id,
                    &edit,
                    &mut expected_version,
                    &mut published_update,
                    graph_edit,
                )
            },
        )
        .map_err(planner_failure)?;
        if !published_update {
            return Err(EditFailure::new(
                503,
                "Gemini completed without publishing a sound graph edit",
            ));
        }
        self.complete_gemini_operation(
            job_id,
            &edit,
            &mut expected_version,
            &completed.plan.summary,
        )
        .map_err(planner_failure)?;
        Ok(completed.plan.summary)
    }

    fn commit_gemini_update(
        &self,
        job_id: u64,
        edit: &EditRequest,
        expected_version: &mut u64,
        published_update: &mut bool,
        graph_edit: GeminiEdit,
    ) -> Result<Project, PlannerError> {
        if self.edit_jobs.is_interrupted(job_id) {
            return Err(PlannerError::Interrupted);
        }
        let summary = graph_edit.plan.summary.clone();
        let mut studio = self.lock_studio();
        if studio.project().version != *expected_version {
            return Err(PlannerError::ProjectChanged);
        }
        let mut candidate = studio.clone();
        candidate
            .replace_graph(
                graph_edit.project,
                edit.start,
                edit.end,
                &edit.prompt,
                graph_edit.plan,
            )
            .map_err(|error| PlannerError::InvalidOutput(studio_error_message(error).to_owned()))?;
        if !candidate.record_operation_step(&edit.operation_id, &summary) {
            return Err(PlannerError::InvalidOutput(
                "could not record the published edit operation".to_owned(),
            ));
        }
        self.commit(&mut studio, candidate)
            .map_err(|_| PlannerError::SaveFailed)?;
        *expected_version = studio.project().version;
        *published_update = true;
        self.edit_jobs
            .publish_update(job_id, *expected_version, &summary);
        Ok(studio.project().clone())
    }

    fn complete_gemini_operation(
        &self,
        job_id: u64,
        edit: &EditRequest,
        expected_version: &mut u64,
        message: &str,
    ) -> Result<(), PlannerError> {
        let mut studio = self.lock_studio();
        if self.edit_jobs.is_interrupted(job_id) {
            return Err(PlannerError::Interrupted);
        }
        if studio.project().version != *expected_version {
            return Err(PlannerError::ProjectChanged);
        }
        let mut candidate = studio.clone();
        if !candidate.mark_operation_complete(&edit.operation_id, message) {
            return Err(PlannerError::InvalidOutput(
                "could not mark the completed edit operation".to_owned(),
            ));
        }
        self.commit_metadata(&mut studio, candidate)
            .map_err(|_| PlannerError::SaveFailed)?;
        *expected_version = studio.project().version;
        self.edit_jobs.finalize_updates(job_id, *expected_version);
        Ok(())
    }

    fn plan_edit(
        &self,
        prompt: &str,
        start: f32,
        end: f32,
        project: &crate::model::Project,
    ) -> Result<EditPlan, String> {
        match &self.planner {
            Planner::Demo => Ok(PromptEngine::interpret_project(prompt, project, start, end)),
            Planner::Gemini => unreachable!("Gemini edits use incremental planning"),
            #[cfg(test)]
            Planner::GatedDemo(gate) => {
                gate.wait_until_released();
                Ok(PromptEngine::interpret_project(prompt, project, start, end))
            }
        }
    }

    fn change_mix(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track_id is required"));
        };
        let volume = match form.get("volume") {
            Some(value) => match value.parse::<f32>() {
                Ok(volume) => Some(volume),
                Err(_) => return Response::json(422, error_json("volume must be a number")),
            },
            None => None,
        };
        let muted = match form.get("muted") {
            Some(value) if value == "true" => Some(true),
            Some(value) if value == "false" => Some(false),
            Some(_) => return Response::json(422, error_json("muted must be true or false")),
            None => None,
        };

        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        match candidate.set_mix(track_id, volume, muted) {
            Ok(()) => match self.commit(&mut studio, candidate) {
                Ok(()) => Response::json(200, studio.to_json()),
                Err(response) => response,
            },
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn change_channel(&self, body: &str) -> Response {
        let form = parse_form(body);
        let operation_id = form.get("operation_id").map(String::as_str);
        if operation_id.is_some_and(|operation_id| !valid_operation_id(operation_id)) {
            return Response::json(422, error_json("operation ID is invalid"));
        }
        #[derive(Clone, Copy)]
        enum Change {
            Add(crate::model::TrackRole),
            Delete(u64),
        }
        let change = match form.get("action").map(String::as_str) {
            Some("add") => form
                .get("role")
                .and_then(|role| crate::model::TrackRole::from_name(role))
                .map(Change::Add),
            Some("delete") => form
                .get("track_id")
                .and_then(|track_id| track_id.parse::<u64>().ok())
                .map(Change::Delete),
            _ => None,
        };
        let Some(change) = change else {
            return Response::json(422, studio_error(StudioError::InvalidChannel));
        };
        let mut studio = self.lock_studio();
        if let Some(operation_id) = operation_id {
            if let Some(recorded) = studio
                .project()
                .channel_operations
                .iter()
                .find(|recorded| recorded.operation_id == operation_id)
            {
                let matches = match change {
                    Change::Add(role) => {
                        recorded.action == ChannelOperationAction::Add
                            && recorded.role == Some(role)
                    }
                    Change::Delete(track_id) => {
                        recorded.action == ChannelOperationAction::Delete
                            && recorded.track_id == track_id
                    }
                };
                return if matches {
                    Response::json(200, studio.to_json())
                } else {
                    Response::json(409, error_json("channel operation ID was already used"))
                };
            }
        }
        let mut candidate = studio.clone();
        let result = match change {
            Change::Add(role) => candidate
                .add_channel(role)
                .map(|track_id| (ChannelOperationAction::Add, track_id, Some(role))),
            Change::Delete(track_id) => candidate
                .delete_channel(track_id)
                .map(|()| (ChannelOperationAction::Delete, track_id, None)),
        };
        match result {
            Ok((action, track_id, role)) => {
                if operation_id.is_some_and(|operation_id| {
                    !candidate.record_channel_operation(operation_id, action, track_id, role)
                }) {
                    return Response::json(
                        409,
                        error_json("channel operation ID was already used"),
                    );
                }
                match self.commit(&mut studio, candidate) {
                    Ok(()) => Response::json(200, studio.to_json()),
                    Err(response) => response,
                }
            }
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn change_sound_tool(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track_id is required"));
        };
        let Some(tool) = form.get("tool") else {
            return Response::json(422, error_json("tool is required"));
        };
        let Some(tool_id) = form
            .get("tool_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("tool_id is required"));
        };
        let clip_id = match form.get("clip_id") {
            Some(value) => match value.parse::<u64>() {
                Ok(value) => Some(value),
                Err(_) => return Response::json(422, error_json("clip_id must be an integer")),
            },
            None => None,
        };
        let Some(parameter) = form.get("parameter") else {
            return Response::json(422, error_json("parameter is required"));
        };
        let Some(value) = form.get("value") else {
            return Response::json(422, error_json("value is required"));
        };

        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        match candidate.configure_sound_tool(track_id, tool, tool_id, clip_id, parameter, value) {
            Ok(()) => match self.commit(&mut studio, candidate) {
                Ok(()) => Response::json(200, studio.to_json()),
                Err(response) => response,
            },
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn gemini_sessions(&self) -> Response {
        match crate::gemini_tools::session_summaries_in(&self.gemini_session_root()) {
            Ok(sessions) => {
                Response::json(200, serde_json::json!({"sessions": sessions}).to_string())
            }
            Err(error) => {
                eprintln!("warning: could not list Gemini sessions: {error}");
                Response::json(500, error_json("could not list Gemini sessions"))
            }
        }
    }

    fn client_log(body: &str) -> Response {
        let form = parse_form(body);
        let Some(level) = form.get("level").map(String::as_str) else {
            return Response::json(422, error_json("log level is required"));
        };
        if !matches!(level, "warning" | "error") {
            return Response::json(422, error_json("log level must be warning or error"));
        }
        let Some(message) = form.get("message").map(|message| message.trim()) else {
            return Response::json(422, error_json("log message is required"));
        };
        if message.is_empty() || message.chars().count() > 4_096 {
            return Response::json(422, error_json("log message length is invalid"));
        }
        let context = form
            .get("context")
            .map(|context| context.trim())
            .filter(|context| !context.is_empty())
            .unwrap_or("browser");
        if context.chars().count() > 160 {
            return Response::json(422, error_json("log context length is invalid"));
        }
        eprintln!(
            "client {level}: {}: {}",
            single_line(context),
            single_line(message)
        );
        Response::json(200, "{\"status\":\"logged\"}".to_owned())
    }

    fn undo(&self) -> Response {
        let mut studio = self.lock_studio();
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(previous_index) = history.current.checked_sub(1) else {
            return Response::json(409, error_json("nothing to undo"));
        };
        let mut project = history.snapshots[previous_index].clone();
        let Some(version) = studio.project().version.checked_add(1) else {
            return Response::json(500, error_json("project revision limit reached"));
        };
        project.version = version;
        let mut candidate_history = history.clone();
        candidate_history.current = previous_index;
        candidate_history.snapshots[previous_index] = project.clone();
        if let Err(error) = self.save_state(&project, &mut candidate_history) {
            eprintln!("error: could not save undone project state: {error}");
            return Response::json(500, error_json("could not undo project change"));
        }
        *history = candidate_history;
        *studio = Studio::from_project(project);
        Response::json(200, studio.to_json_with_can_undo(history.current > 0))
    }

    fn reset(&self) -> Response {
        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        candidate.reset();
        let mut history = ProjectHistory::new(candidate.project().clone());
        if let Err(error) = self.save_state(candidate.project(), &mut history) {
            eprintln!("error: could not reset project and history: {error}");
            return Response::json(500, error_json("could not reset the project"));
        }
        *studio = candidate;
        *self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = history;
        Response::json(200, studio.to_json_with_can_undo(false))
    }

    fn history_response(&self) -> Response {
        let history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = history
            .snapshots
            .iter()
            .enumerate()
            .map(|(index, project)| {
                let previous_edit_count = index
                    .checked_sub(1)
                    .and_then(|previous| history.snapshots.get(previous))
                    .map_or(0, |previous| previous.edits.len());
                let edit = (project.edits.len() > previous_edit_count)
                    .then(|| project.edits.last())
                    .flatten();
                let (summary, source, prompt, start, end) = if index == 0 {
                    ("Initial project", "Project", None, None, None)
                } else if let Some(edit) = edit {
                    (
                        edit.summary.as_str(),
                        "Gemini",
                        Some(edit.prompt.as_str()),
                        Some(edit.start),
                        Some(edit.end),
                    )
                } else {
                    ("Manual project change", "Manual", None, None, None)
                };
                serde_json::json!({
                    "index":index,
                    "version":project.version,
                    "summary":summary,
                    "source":source,
                    "prompt":prompt,
                    "start":start,
                    "end":end
                })
            })
            .collect::<Vec<_>>();
        Response::json(
            200,
            serde_json::json!({
                "current":history.current,
                "currentVersion":history.snapshots[history.current].version,
                "currentEditCount":history.snapshots[history.current].edits.len(),
                "entries":entries
            })
            .to_string(),
        )
    }

    fn backend_response(&self) -> Response {
        let backend = if self.builtin_backend.load(Ordering::SeqCst) {
            "built-in"
        } else {
            "Surge XT"
        };
        Response::json(200, serde_json::json!({"backend":backend}).to_string())
    }

    fn set_backend(&self, body: &str) -> Response {
        let backend = parse_form(body).get("backend").cloned().unwrap_or_default();
        let builtin = match backend.as_str() {
            "Surge XT" => false,
            "built-in" => true,
            _ => return Response::json(422, error_json("unknown sound engine")),
        };
        self.builtin_backend.store(builtin, Ordering::SeqCst);
        self.backend_response()
    }

    fn select_history(&self, body: &str) -> Response {
        let Some(index) = parse_form(body)
            .get("index")
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return Response::json(422, error_json("history index is required"));
        };
        let mut studio = self.lock_studio();
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut project) = history.snapshots.get(index).cloned() else {
            return Response::json(404, error_json("history state not found"));
        };
        let Some(version) = studio.project().version.checked_add(1) else {
            return Response::json(500, error_json("project revision limit reached"));
        };
        project.version = version;
        let mut candidate_history = history.clone();
        candidate_history.current = index;
        candidate_history.snapshots[index] = project.clone();
        if let Err(error) = self.save_state(&project, &mut candidate_history) {
            eprintln!("error: could not save selected history state: {error}");
            return Response::json(500, error_json("could not select history state"));
        }
        *history = candidate_history;
        *studio = Studio::from_project(project);
        Response::json(200, studio.to_json_with_can_undo(history.current > 0))
    }

    fn project_response(&self, studio: &Studio) -> Response {
        let can_undo = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            > 0;
        Response::json(200, studio.to_json_with_can_undo(can_undo))
    }

    fn instrument_parameters(&self, track_id: u64, group: &str) -> Response {
        let studio = self.lock_studio();
        let Some(track) = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == track_id)
        else {
            return Response::json(404, error_json("track not found"));
        };
        let common = match group {
            "common" => true,
            "advanced" => false,
            _ => return Response::json(422, error_json("parameter group is invalid")),
        };
        let mut body = String::from("{\"parameters\":[");
        let mut first = true;
        for parameter in crate::surge::instrument_parameters(&track.instrument.preset)
            .into_iter()
            .filter(|parameter| parameter.common == common)
        {
            if !first {
                body.push(',');
            }
            first = false;
            let legacy_override =
                crate::surge::legacy_instrument_parameter_override(&track.instrument, parameter.id);
            let value = track
                .instrument
                .native_overrides
                .get(&parameter.id)
                .copied()
                .or(legacy_override)
                .unwrap_or(parameter.value);
            let overridden = track
                .instrument
                .native_overrides
                .contains_key(&parameter.id)
                || legacy_override.is_some();
            let graph_parameter =
                crate::surge::instrument_graph_parameter(&track.instrument.preset, parameter.id)
                    .map_or_else(|| "null".to_owned(), json_string);
            write!(
                body,
                concat!(
                    "{{\"parameter\":{},\"graphParameter\":{},\"name\":{},\"value\":{},\"presetValue\":{},",
                    "\"display\":{},\"overridden\":{}}}"
                ),
                json_string(&format!("native:{}", parameter.id)),
                graph_parameter,
                json_string(&parameter.name),
                value,
                parameter.value,
                json_string(&parameter.display),
                overridden
            )
            .expect("writing to a string cannot fail");
        }
        body.push_str("]}");
        Response::json(200, body)
    }

    fn commit(
        &self,
        studio: &mut std::sync::MutexGuard<'_, Studio>,
        candidate: Studio,
    ) -> Result<(), Response> {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        history.push(candidate.project().clone());
        if let Err(error) = self.save_state(candidate.project(), &mut history) {
            eprintln!("error: could not save project history: {error}");
            return Err(Response::json(
                500,
                error_json("could not save project history"),
            ));
        }
        **studio = candidate;
        *self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = history;
        Ok(())
    }

    fn commit_metadata(
        &self,
        studio: &mut std::sync::MutexGuard<'_, Studio>,
        candidate: Studio,
    ) -> Result<(), Response> {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let current = history.current;
        if let Some(snapshot) = history.snapshots.get_mut(current) {
            *snapshot = candidate.project().clone();
        }
        if let Err(error) = self.save_state(candidate.project(), &mut history) {
            eprintln!("error: could not save project history metadata: {error}");
            return Err(Response::json(
                500,
                error_json("could not save project history"),
            ));
        }
        **studio = candidate;
        *self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = history;
        Ok(())
    }

    fn save_state(&self, project: &Project, history: &mut ProjectHistory) -> io::Result<()> {
        trim_project_history(project, history);
        let Some(store) = &self.store else {
            return Ok(());
        };
        save_project_state(store, project, history)
    }

    fn project_path(&self) -> &std::path::Path {
        self.store
            .as_ref()
            .expect("production router has a project store")
            .path()
    }

    fn gemini_session_root(&self) -> PathBuf {
        if let Some(store) = &self.store {
            return store
                .path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("gemini-sessions");
        }
        #[cfg(test)]
        return crate::gemini_tools::session_root();
        #[cfg(not(test))]
        unreachable!("production routers always have project storage")
    }

    fn lock_studio(&self) -> std::sync::MutexGuard<'_, Studio> {
        self.studio
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl Request {
    fn user_id(&self) -> Option<&str> {
        self.headers.get("cookie")?.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "daw_ai_user" && valid_user_id(value)).then_some(value)
        })
    }

    fn read(stream: &mut impl Read) -> Result<Self, String> {
        let mut bytes = Vec::with_capacity(2048);
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_REQUEST_BYTES {
                return Err("request is too large".to_owned());
            }
            if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
                break position + 4;
            }
        };

        let headers = std::str::from_utf8(&bytes[..header_end])
            .map_err(|_| "request headers must be UTF-8".to_owned())?;
        let mut lines = headers.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "missing request line".to_owned())?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts
            .next()
            .ok_or_else(|| "missing method".to_owned())?
            .to_owned();
        let target = request_parts
            .next()
            .ok_or_else(|| "missing path".to_owned())?;
        if request_parts.next().is_none() {
            return Err("missing HTTP version".to_owned());
        }
        let path = target.split('?').next().unwrap_or(target).to_owned();

        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
            .collect();
        let content_length = headers.get("content-length").map_or(Ok(0_usize), |value| {
            value
                .parse::<usize>()
                .map_err(|_| "invalid content length".to_owned())
        })?;
        let body_end = header_end
            .checked_add(content_length)
            .ok_or_else(|| "request is too large".to_owned())?;
        if body_end > MAX_REQUEST_BYTES {
            return Err("request is too large".to_owned());
        }

        while bytes.len() < body_end {
            let remaining = body_end - bytes.len();
            let mut chunk = [0_u8; 2048];
            let count = stream
                .read(&mut chunk[..remaining.min(2048)])
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request body".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
        }

        let body = std::str::from_utf8(&bytes[header_end..body_end])
            .map_err(|_| "request body must be UTF-8".to_owned())?
            .to_owned();
        Ok(Self {
            method,
            path,
            headers,
            body,
        })
    }

    fn is_mutation(&self) -> bool {
        self.method == "POST"
            && (matches!(
                self.path.as_str(),
                "/api/edits"
                    | "/api/mix"
                    | "/api/sound-tools"
                    | "/api/channels"
                    | "/api/logs"
                    | "/api/undo"
                    | "/api/reset"
            ) || self.path.starts_with("/api/edits/"))
    }

    fn public_host(&self) -> Option<&str> {
        let transport_host = self.headers.get("host")?;
        parse_authority(transport_host)?;
        let forwarded = match self.headers.get("x-forwarded-host") {
            Some(value) => Some(forwarded_host(value)?),
            None => None,
        };
        forwarded.or(Some(transport_host))
    }

    fn is_trusted_mutation(&self, host: &str) -> bool {
        self.is_trusted_request(host)
    }

    fn is_trusted_audio(&self, host: &str) -> bool {
        self.headers
            .get(AUDIO_REQUEST_HEADER)
            .is_some_and(|value| value == "1")
            && self.is_trusted_request(host)
    }

    fn is_trusted_request(&self, host: &str) -> bool {
        if self
            .headers
            .get("sec-fetch-site")
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
        {
            return false;
        }

        self.headers
            .get("origin")
            .is_none_or(|origin| origin_matches_host(origin, host))
    }
}

fn valid_user_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, &'static str)>,
    set_cookie: Option<String>,
}

impl Response {
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
            headers: vec![("Cache-Control", "no-store")],
            set_cookie: None,
        }
    }

    fn static_asset(content_type: &'static str, body: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_owned(),
            headers: vec![(
                "Cache-Control",
                "no-store, no-cache, must-revalidate, max-age=0",
            )],
            set_cookie: None,
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    fn write(&self, stream: &mut impl Write) -> io::Result<()> {
        write_response_head(
            stream,
            self.status,
            self.content_type,
            self.body.len(),
            &self.headers,
            self.set_cookie.as_deref(),
        )?;
        stream.write_all(self.body.as_bytes())
    }
}

fn write_response_head(
    stream: &mut impl Write,
    status: u16,
    content_type: &str,
    content_length: usize,
    headers: &[(&str, &str)],
    set_cookie: Option<&str>,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Error",
    };
    let mut head = format!(
        concat!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            "Connection: close\r\nX-Content-Type-Options: nosniff\r\n",
            "Content-Security-Policy: default-src 'self'; script-src 'self'; ",
            "style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; ",
            "media-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none';\r\n",
            "Referrer-Policy: no-referrer\r\n"
        ),
        status, reason, content_type, content_length
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(cookie) = set_cookie {
        head.push_str("Set-Cookie: ");
        head.push_str(cookie);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn new_operation_id(id: u64) -> String {
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

fn fallback_operation_id(id: u64) -> String {
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

fn valid_operation_id(operation_id: &str) -> bool {
    !operation_id.is_empty()
        && operation_id.len() <= 128
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn recovered_operation_json(operation: &crate::model::EditOperation) -> String {
    let common = format!(
        concat!(
            "\"id\":\"recovered\",\"operationId\":{},\"elapsedSeconds\":0,",
            "\"timeoutSeconds\":{},\"appliedSteps\":{},\"initialVersion\":{},",
            "\"projectVersion\":{}"
        ),
        json_string(&operation.operation_id),
        EDIT_TIMEOUT_SECONDS,
        operation.applied_steps,
        operation.initial_version,
        operation.project_version
    );
    if operation.status == crate::model::EditOperationStatus::Completed {
        format!(
            "{{{common},\"status\":\"completed\",\"phase\":\"completed\",\"message\":{}}}",
            json_string(&operation.message)
        )
    } else {
        let recovered_status = match operation.status {
            crate::model::EditOperationStatus::Interrupted => "interrupted_with_changes",
            crate::model::EditOperationStatus::Running
            | crate::model::EditOperationStatus::Failed => "failed_with_changes",
            crate::model::EditOperationStatus::Completed => unreachable!(),
        };
        format!(
            concat!(
                "{{{},\"status\":{},\"phase\":\"failed\",",
                "\"errorStatus\":500,",
                "\"error\":{}}}"
            ),
            common,
            json_string(recovered_status),
            json_string(&operation.message)
        )
    }
}

fn accepted_edit_job_json(id: u64, operation_id: &str, poll_after_ms: u64) -> String {
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

fn edit_job_id(path: &str) -> Option<u64> {
    let id = path.strip_prefix("/api/edits/")?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| id.parse::<u64>().ok())
        .flatten()
}

fn interrupted_edit_job_id(path: &str) -> Option<u64> {
    path.strip_prefix("/api/edits/")?
        .strip_suffix("/interrupt")?
        .parse()
        .ok()
}

fn playback_audio_stream(path: &str) -> Option<(&str, u64, u64)> {
    let mut parts = path.strip_prefix("/api/audio-stream/")?.split('/');
    let token = parts.next()?;
    let version = parts.next()?.parse::<u64>().ok()?;
    let start_milliseconds = parts.next()?.parse::<u64>().ok()?;
    if token.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((token, version, start_milliseconds))
}

fn edit_operation_id(path: &str) -> Option<&str> {
    let operation_id = path.strip_prefix("/api/edit-operations/")?;
    valid_operation_id(operation_id).then_some(operation_id)
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (url_decode(key), url_decode(value))
        })
        .collect()
}

fn forwarded_host(value: &str) -> Option<&str> {
    let host = value.split(',').next()?.trim();
    parse_authority(host).map(|_| host)
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let (authority, default_port) = origin
        .strip_prefix("http://")
        .map(|authority| (authority, 80))
        .or_else(|| {
            origin
                .strip_prefix("https://")
                .map(|authority| (authority, 443))
        })
        .unwrap_or(("", 0));
    if default_port == 0 {
        return false;
    }
    let Some((origin_host, origin_port)) = parse_authority(authority) else {
        return false;
    };
    let Some((request_host, request_port)) = parse_authority(host) else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(request_host)
        && origin_port.unwrap_or(default_port) == request_port.unwrap_or(default_port)
}

fn parse_authority(value: &str) -> Option<(&str, Option<u16>)> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['/', '\\', '?', '#', '@', ','])
    {
        return None;
    }

    if value.starts_with('[') {
        let end = value.find(']')?;
        let hostname = &value[..=end];
        if hostname.len() <= 2 {
            return None;
        }
        let remainder = &value[end + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            Some(parse_port(remainder.strip_prefix(':')?)?)
        };
        return Some((hostname, port));
    }

    let (hostname, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(hostname, port)| (hostname, Some(port)));
    if hostname.is_empty()
        || hostname.contains(':')
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return None;
    }
    let port = match port {
        Some(port) => Some(parse_port(port)?),
        None => None,
    };
    Some((hostname, port))
}

fn parse_port(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|port| *port > 0)
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    output.push(high * 16 + low);
                    index += 2;
                } else {
                    output.push(bytes[index]);
                }
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn error_json(message: &str) -> String {
    format!("{{\"error\":{}}}", json_string(message))
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn log_http_response(request_id: u64, started: Instant, request: &Request, response: &Response) {
    let latency_ms = started.elapsed().as_millis();
    if request.is_mutation() || response.status >= 400 || latency_ms >= 1_000 {
        let level = if response.status >= 500 {
            "error"
        } else if response.status >= 400 {
            "warning"
        } else {
            "info"
        };
        eprintln!(
            "{level}: http request_id={request_id} method={} path={} status={} latency_ms={}",
            request.method, request.path, response.status, latency_ms
        );
    }
}

fn studio_error(error: StudioError) -> String {
    error_json(studio_error_message(error))
}

const fn studio_error_message(error: StudioError) -> &'static str {
    match error {
        StudioError::EmptyPrompt => "describe the change you want",
        StudioError::InvalidPrompt => "prompt is too long",
        StudioError::InvalidSelection => "select a valid part of the track",
        StudioError::UnknownTrack => "track not found",
        StudioError::InvalidMix => "invalid mixer setting",
        StudioError::InvalidChannel => "invalid channel change",
        StudioError::UnknownSoundTool => "sound tool not found",
        StudioError::InvalidSoundTool => "invalid sound tool setting",
        StudioError::EffectCapacity => "Surge XT effect chain is full",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Project;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_FILE_ID: AtomicU64 = AtomicU64::new(1);

    fn request(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: HashMap::from([("host".to_owned(), "127.0.0.1:8888".to_owned())]),
            body: body.to_owned(),
        }
    }

    fn audio_request(path: &str) -> Request {
        let mut request = request("GET", path, "");
        request
            .headers
            .insert(AUDIO_REQUEST_HEADER.to_owned(), "1".to_owned());
        request
    }

    fn wait_for_edit(router: &Router, accepted: &Response) -> serde_json::Value {
        assert_eq!(accepted.status, 202);
        let accepted: serde_json::Value =
            serde_json::from_str(&accepted.body).expect("accepted edit JSON");
        assert_eq!(accepted["status"], "queued");
        assert_eq!(accepted["timeoutSeconds"], EDIT_TIMEOUT_SECONDS);
        assert!(
            accepted["operationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        let path = format!(
            "/api/edits/{}",
            accepted["id"].as_str().expect("edit job ID")
        );
        for _ in 0..200 {
            let response = router.handle(&request("GET", &path, ""));
            assert_eq!(response.status, 200);
            let job: serde_json::Value =
                serde_json::from_str(&response.body).expect("edit status JSON");
            if matches!(job["status"].as_str(), Some("completed" | "failed")) {
                return job;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("edit job did not finish");
    }

    fn persisted_demo() -> (Router, std::path::PathBuf) {
        let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daw-ai-server-test-{}-{id}.json",
            std::process::id()
        ));
        let (store, _) = ProjectStore::open(path.clone()).expect("test project store");
        let studio = Studio::from_project(Project::demo());
        store.save(studio.project()).expect("save demo fixture");
        (
            Router {
                history: Arc::new(Mutex::new(ProjectHistory::new(studio.project().clone()))),
                builtin_backend: Arc::new(AtomicBool::new(false)),
                studio: Arc::new(Mutex::new(studio)),
                store: Some(store),
                planner: Planner::Demo,
                edit_jobs: Arc::new(EditJobs::new()),
                audio_renderer: Arc::new(AudioRenderer::default()),
                audio_token: Arc::new("test-audio-token".to_owned()),
                users: None,
            },
            path,
        )
    }

    #[test]
    fn recovers_an_invalid_persisted_project_and_preserves_the_source() {
        let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daw-ai-server-invalid-{}-{id}.json",
            std::process::id()
        ));
        fs::write(&path, "{not json}\n").expect("invalid graph fixture");

        let (_, studio, history) =
            open_project_with_history(path.clone()).expect("recovered project");

        assert_eq!(history.snapshots.len(), 1);
        assert_eq!(history.snapshots[0].to_json(), studio.project().to_json());
        ProjectStore::open(path.clone()).expect("replacement project is valid");
        let prefix = format!("{}.invalid-", path.file_name().unwrap().to_string_lossy());
        let quarantined = fs::read_dir(path.parent().unwrap())
            .expect("temporary directory")
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .expect("quarantined invalid graph")
            .path();
        assert_eq!(
            fs::read_to_string(&quarantined).expect("quarantined graph"),
            "{not json}\n"
        );
        fs::remove_file(path).expect("remove replacement project");
        fs::remove_file(quarantined).expect("remove quarantine");
    }

    #[test]
    fn serves_the_app_and_project_api() {
        let router = Router::demo();
        let page = router.handle(&request("GET", "/", ""));
        assert_eq!(page.status, 200);
        assert!(page.body.contains("DAW-AI"));
        assert!(page.headers.contains(&(
            "Cache-Control",
            "no-store, no-cache, must-revalidate, max-age=0"
        )));
        let script = router.handle(&request("GET", "/app.js", ""));
        assert_eq!(script.status, 200);
        assert!(script.headers.contains(&(
            "Cache-Control",
            "no-store, no-cache, must-revalidate, max-age=0"
        )));
        let project = router.handle(&request("GET", "/api/project", ""));
        assert_eq!(project.status, 200);
        assert!(project.body.contains("\"tracks\""));
    }

    #[test]
    fn gemini_sessions_are_always_persistent_and_listed_for_debugging() {
        let router = Router::demo();
        let response = router.handle(&request("GET", "/api/gemini-sessions", ""));
        assert_eq!(response.status, 200);
        let body: serde_json::Value =
            serde_json::from_str(&response.body).expect("Gemini session list JSON");
        assert!(body["sessions"].is_array());
        assert_eq!(
            router
                .handle(&request("POST", "/api/gemini-sessions", ""))
                .status,
            405
        );
    }

    #[test]
    fn edit_job_status_reports_phase_progress_and_failures() {
        let jobs = EditJobs::new();
        let (id, operation_id, created) = jobs.create(750, None).expect("edit job");
        assert!(created);
        jobs.set_running(id, "planning", "Gemini is arranging the requested change");
        let running: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("running job response").body)
                .expect("running job JSON");
        assert_eq!(running["status"], "running");
        assert_eq!(running["phase"], "planning");
        assert_eq!(
            running["detail"],
            "Gemini is arranging the requested change"
        );
        assert_eq!(running["pollAfterMs"], 750);
        assert_eq!(running["operationId"], operation_id);
        assert_eq!(running["appliedSteps"], 0);
        assert!(running["projectVersion"].is_null());

        jobs.publish_update(id, 7, "Added a bass layer");
        let updated: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("updated job response").body)
                .expect("updated job JSON");
        assert_eq!(updated["phase"], "editing");
        assert_eq!(updated["detail"], "Applied step 1: Added a bass layer");
        assert_eq!(updated["appliedSteps"], 1);
        assert_eq!(updated["projectVersion"], 7);

        jobs.fail(id, 503, "Gemini timed out".to_owned());
        let failed: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("failed job response").body)
                .expect("failed job JSON");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 503);
        assert_eq!(failed["error"], "Gemini timed out");
    }

    #[test]
    fn edit_jobs_bound_concurrent_project_snapshots() {
        let jobs = EditJobs::new();
        let active = (0..MAX_ACTIVE_EDIT_JOBS)
            .map(|_| jobs.create(750, None).expect("active edit job").0)
            .collect::<Vec<_>>();
        assert!(jobs.create(750, None).is_err());

        jobs.fail(active[0], 503, "planner stopped".to_owned());
        assert!(jobs.create(750, None).is_ok());
    }

    #[test]
    fn edit_api_updates_the_shared_project() {
        let router = Router::demo();
        let response = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=increase+volume",
        ));
        let completed = wait_for_edit(&router, &response);
        assert_eq!(completed["status"], "completed");
        assert!(completed["message"].as_str().unwrap().contains("Lifted"));
        assert!(completed.get("project").is_none());

        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("increase volume"));
        assert_eq!(
            router
                .handle(&request("GET", "/api/edits/999999", ""))
                .status,
            404
        );
        assert_eq!(
            router.handle(&request("POST", "/api/edits/1", "")).status,
            405
        );
    }

    #[test]
    fn sound_tool_api_updates_the_shared_graph() {
        let router = Router::demo();
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=preset&value=Surge+Lead",
        ));
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"preset\":\"Surge Lead\""));

        let invalid = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=attack&value=99",
        ));
        assert_eq!(invalid.status, 422);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"preset\":\"Surge Lead\""));
    }

    #[test]
    fn instrument_parameter_api_reports_legacy_graph_overrides() {
        let router = Router::demo();
        {
            let mut project = Project::demo();
            let instrument = &mut project.tracks[1].instrument;
            instrument.native_overrides.clear();
            instrument.cutoff = 0.123;
            instrument.parameter_overrides.push("cutoff".to_owned());
            *router.lock_studio() = Studio::from_project(project);
        }

        let response = router.handle(&request("GET", "/api/instrument-parameters/2/common", ""));
        assert_eq!(response.status, 200);
        let response: serde_json::Value =
            serde_json::from_str(&response.body).expect("instrument parameters");
        let cutoff = response["parameters"]
            .as_array()
            .expect("parameter list")
            .iter()
            .find(|parameter| parameter["graphParameter"] == "cutoff")
            .expect("cutoff parameter");
        assert!((cutoff["value"].as_f64().expect("cutoff value") - 0.123).abs() < 0.000_001);
        assert_eq!(cutoff["overridden"], true);
    }

    #[test]
    fn channel_api_adds_and_deletes_complete_tracks() {
        let router = Router::demo();
        let added = router.handle(&request(
            "POST",
            "/api/channels",
            "operation_id=add-lead&action=add&role=lead",
        ));
        assert_eq!(added.status, 200);
        let added: serde_json::Value = serde_json::from_str(&added.body).expect("added project");
        let tracks = added["tracks"].as_array().expect("tracks");
        assert_eq!(tracks.len(), 4);
        let lead = tracks.last().expect("lead track");
        assert_eq!(lead["role"], "lead");
        assert_eq!(lead["volume"], 1.0);
        assert_eq!(lead["instrument"]["preset"], "Init");
        assert!(lead["clips"].as_array().expect("clips").is_empty());
        assert!(lead["effects"].as_array().expect("effects").is_empty());
        assert!(
            lead["modulators"]
                .as_array()
                .expect("modulators")
                .is_empty()
        );
        let track_id = lead["id"].as_u64().expect("lead track ID");
        assert_eq!(added["channelOperations"][0]["operationId"], "add-lead");
        assert_eq!(added["channelOperations"][0]["trackId"], track_id);
        let duplicate = router.handle(&request(
            "POST",
            "/api/channels",
            "operation_id=add-lead&action=add&role=lead",
        ));
        assert_eq!(duplicate.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&duplicate.body).expect("duplicate add")
                ["tracks"]
                .as_array()
                .expect("tracks")
                .len(),
            4
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/channels",
                    "operation_id=add-lead&action=add&role=texture",
                ))
                .status,
            409
        );

        let deleted = router.handle(&request(
            "POST",
            "/api/channels",
            &format!("operation_id=delete-lead&action=delete&track_id={track_id}"),
        ));
        assert_eq!(deleted.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&deleted.body)
                .expect("deleted project")["tracks"]
                .as_array()
                .expect("tracks")
                .len(),
            3
        );
        assert_eq!(
            router
                .handle(&request("POST", "/api/channels", "action=add&role=unknown"))
                .status,
            422
        );
    }

    #[test]
    fn gemini_updates_commit_as_incremental_recoverable_steps() {
        let router = Router::demo();
        let (job_id, operation_id, _) = router.edit_jobs.create(750, None).expect("edit job");
        let project = router.lock_studio().project().clone();
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: "shape the bass in two steps".to_owned(),
            start: 4.0,
            end: 8.0,
            project: project.clone(),
        };
        let plan = |preset: &str, summary: &str| EditPlan {
            action: crate::prompt::Action::Configure {
                track_id: 2,
                target: crate::model::TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "preset",
                value: preset.to_owned(),
            },
            summary: summary.to_owned(),
        };
        let mut session = Studio::from_project(project);
        let first_plan = plan("Surge Lead", "Brightened the bass");
        session
            .apply_plan(4.0, 8.0, &edit.prompt, first_plan.clone())
            .expect("first session step");
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan: first_plan,
                    project: session.project().clone(),
                },
            )
            .expect("first live step");
        assert!(
            router
                .lock_studio()
                .project()
                .edits
                .iter()
                .all(|edit| edit.operation_id.is_none())
        );

        let second_plan = plan("Surge Pad", "Softened the bass");
        session
            .apply_plan(4.0, 8.0, &edit.prompt, second_plan.clone())
            .expect("second session step");
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan: second_plan,
                    project: session.project().clone(),
                },
            )
            .expect("second live step");
        router
            .complete_gemini_operation(
                job_id,
                &edit,
                &mut expected_version,
                "Finished shaping the bass",
            )
            .expect("terminal operation marker");

        let studio = router.lock_studio();
        assert_eq!(studio.project().version, 4);
        assert_eq!(studio.project().tracks[1].instrument.preset, "Surge Pad");
        assert_eq!(studio.project().edits.len(), 2);
        assert!(
            studio
                .project()
                .edits
                .iter()
                .all(|edit| edit.operation_id.is_none())
        );
        let operation = &studio.project().edit_operations[0];
        assert_eq!(operation.operation_id, operation_id);
        assert_eq!(
            operation.status,
            crate::model::EditOperationStatus::Completed
        );
        assert_eq!(operation.applied_steps, 2);
        assert_eq!(operation.message, "Finished shaping the bass");
        crate::model::Project::from_json(&studio.project().to_json())
            .expect("persistable incremental graph");
        drop(studio);

        let status: serde_json::Value = serde_json::from_str(
            &router
                .edit_jobs
                .response(job_id)
                .expect("incremental job status")
                .body,
        )
        .expect("incremental job JSON");
        assert_eq!(status["appliedSteps"], 2);
        assert_eq!(status["projectVersion"], 4);
        assert_eq!(status["phase"], "finalizing");

        router.edit_jobs.remove(job_id);
        let recovered = router.handle(&request(
            "POST",
            "/api/edits",
            &format!(
                "operation_id={operation_id}&start=4&end=8&prompt=shape+the+bass+in+two+steps"
            ),
        ));
        assert_eq!(recovered.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered.body)
                .expect("recovered terminal operation")["status"],
            "completed"
        );
    }

    #[test]
    fn partial_gemini_update_is_not_recovered_as_a_completed_operation() {
        let router = Router::demo();
        let (job_id, operation_id, _) = router.edit_jobs.create(750, None).expect("edit job");
        let project = router.lock_studio().project().clone();
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: "shape the bass in two steps".to_owned(),
            start: 4.0,
            end: 8.0,
            project: project.clone(),
        };
        let first_plan = EditPlan {
            action: crate::prompt::Action::Configure {
                track_id: 2,
                target: crate::model::TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "preset",
                value: "Surge Lead".to_owned(),
            },
            summary: "Brightened the bass".to_owned(),
        };
        let mut session = Studio::from_project(project);
        session
            .apply_plan(4.0, 8.0, &edit.prompt, first_plan.clone())
            .expect("first of two session steps");
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan: first_plan,
                    project: session.project().clone(),
                },
            )
            .expect("first live step");
        assert!(
            router
                .lock_studio()
                .project()
                .edits
                .iter()
                .all(|edit| edit.operation_id.as_deref() != Some(operation_id.as_str()))
        );

        assert!(router.edit_jobs.interrupt(job_id));
        assert!(matches!(
            router.complete_gemini_operation(
                job_id,
                &edit,
                &mut expected_version,
                "Finished shaping the bass",
            ),
            Err(PlannerError::Interrupted)
        ));
        assert!(
            router.lock_studio().project().edit_operations[0].status
                != crate::model::EditOperationStatus::Completed,
            "an interrupted job must remain recoverably partial"
        );

        router.edit_jobs.remove(job_id);
        let persisted = crate::model::Project::from_json(&router.lock_studio().project().to_json())
            .expect("persisted partial operation");
        let restarted = Router {
            history: Arc::new(Mutex::new(ProjectHistory::new(persisted.clone()))),
            builtin_backend: Arc::new(AtomicBool::new(false)),
            studio: Arc::new(Mutex::new(Studio::from_project(persisted))),
            store: None,
            planner: Planner::Demo,
            edit_jobs: Arc::new(EditJobs::new()),
            audio_renderer: Arc::new(AudioRenderer::default()),
            audio_token: Arc::new("test-audio-token".to_owned()),
            users: None,
        };
        let retried = restarted.handle(&request(
            "POST",
            "/api/edits",
            &format!(
                "operation_id={operation_id}&start=4&end=8&prompt=shape+the+bass+in+two+steps"
            ),
        ));
        assert_eq!(retried.status, 200);
        let retried_json: serde_json::Value =
            serde_json::from_str(&retried.body).expect("retried edit job");
        assert_eq!(retried_json["status"], "failed_with_changes");
        assert_eq!(retried_json["operationId"], operation_id);
        assert_eq!(retried_json["appliedSteps"], 1);
        assert_eq!(retried_json["projectVersion"], expected_version);
        let recovered = restarted.handle(&request(
            "GET",
            &format!("/api/edit-operations/{operation_id}"),
            "",
        ));
        assert_eq!(recovered.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered.body)
                .expect("recovered partial operation")["status"],
            "failed_with_changes"
        );
    }

    #[test]
    fn gemini_completion_is_independent_of_edit_log_compaction() {
        let router = Router::demo();
        let mut populated = Studio::new();
        let regional_plan = || EditPlan {
            action: crate::prompt::Action::Gain {
                amount: 1.0,
                target: Some(crate::model::TrackRole::Bass),
            },
            summary: "Retained regional state".to_owned(),
        };
        for _ in 1..crate::model::EDIT_LOG_LIMIT {
            populated
                .apply_plan(4.0, 8.0, "retain regional state", regional_plan())
                .expect("regional edit");
        }
        populated
            .apply_plan_for_operation(
                4.0,
                8.0,
                "retain prior operation identity",
                "previous-operation".to_owned(),
                regional_plan(),
            )
            .expect("prior operation");
        let project = populated.project().clone();
        *router.lock_studio() = Studio::from_project(project.clone());

        let (job_id, operation_id, _) = router.edit_jobs.create(750, None).expect("edit job");
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: "change the bass patch".to_owned(),
            start: 4.0,
            end: 8.0,
            project: project.clone(),
        };
        let plan = EditPlan {
            action: crate::prompt::Action::Configure {
                track_id: 2,
                target: crate::model::TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "preset",
                value: "Surge Lead".to_owned(),
            },
            summary: "Changed the bass patch".to_owned(),
        };
        let mut session = Studio::from_project(project);
        session
            .apply_plan(4.0, 8.0, &edit.prompt, plan.clone())
            .expect("nonregional Gemini edit");
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan,
                    project: session.project().clone(),
                },
            )
            .expect("compacted Gemini update");
        router
            .complete_gemini_operation(
                job_id,
                &edit,
                &mut expected_version,
                "Changed the bass patch",
            )
            .expect("independent completion record");

        let studio = router.lock_studio();
        assert_eq!(studio.project().edits.len(), crate::model::EDIT_LOG_LIMIT);
        assert_eq!(
            studio
                .project()
                .edits
                .last()
                .and_then(|edit| edit.operation_id.as_deref()),
            Some("previous-operation")
        );
        let completed = studio
            .project()
            .edit_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("Gemini operation record");
        assert_eq!(
            completed.status,
            crate::model::EditOperationStatus::Completed
        );
        assert_eq!(completed.message, "Changed the bass patch");
        assert!(studio.project().edit_operations.iter().any(|operation| {
            operation.operation_id == "previous-operation"
                && operation.status == crate::model::EditOperationStatus::Completed
        }));
    }

    #[test]
    fn accepts_bounded_client_error_and_warning_logs() {
        let router = Router::demo();
        let error = router.handle(&request(
            "POST",
            "/api/logs",
            "level=error&context=starting+audio&message=Media+playback+failed",
        ));
        assert_eq!(error.status, 200);
        assert_eq!(error.body, "{\"status\":\"logged\"}");

        let warning = router.handle(&request(
            "POST",
            "/api/logs",
            "level=warning&message=Recovered+from+an+invalid+node",
        ));
        assert_eq!(warning.status, 200);
        assert_eq!(
            router
                .handle(&request("POST", "/api/logs", "level=info&message=no"))
                .status,
            422
        );
        assert_eq!(router.handle(&request("GET", "/api/logs", "")).status, 405);
    }

    #[test]
    fn successful_api_mutations_persist_the_sound_graph() {
        let (router, path) = persisted_demo();
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=preset&value=Surge+Lead",
        ));
        assert_eq!(response.status, 200);
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=modulator&tool_id=250&parameter=target&value=instrument.resonance",
        ));
        assert_eq!(response.status, 200);
        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert_eq!(saved.project().tracks[1].instrument.preset, "Surge Lead");
        assert_eq!(
            saved.project().tracks[1].modulators[0].target,
            "instrument.resonance"
        );
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn deleting_an_automated_channel_persists_without_its_envelope() {
        let (router, path) = persisted_demo();
        let bass_id = router.lock_studio().project().tracks[1].id;
        {
            let mut studio = router.lock_studio();
            let mut candidate = studio.clone();
            candidate
                .apply_plan(
                    0.0,
                    4.0,
                    "automate bass volume",
                    crate::prompt::EditPlan {
                        summary: "Automated bass volume".to_owned(),
                        action: crate::prompt::Action::Automation {
                            track_id: bass_id,
                            parameter: "track.volume".to_owned(),
                            curve: "linear",
                            points: vec![
                                crate::prompt::AutomationPoint {
                                    time: 0.0,
                                    value: 0.2,
                                },
                                crate::prompt::AutomationPoint {
                                    time: 1.0,
                                    value: 1.2,
                                },
                            ],
                            target: crate::model::TrackRole::Bass,
                        },
                    },
                )
                .expect("valid automation");
            assert!(router.commit(&mut studio, candidate).is_ok());
        }

        let deleted = router.handle(&request(
            "POST",
            "/api/channels",
            &format!("action=delete&track_id={bass_id}"),
        ));
        assert_eq!(deleted.status, 200, "{}", deleted.body);
        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert!(
            saved
                .project()
                .tracks
                .iter()
                .all(|track| track.id != bass_id)
        );
        assert!(
            !saved
                .project()
                .to_json()
                .contains("\"type\":\"automation\"")
        );
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn completed_async_edits_persist_the_sound_graph() {
        let (router, path) = persisted_demo();
        let accepted = router.handle(&request(
            "POST",
            "/api/edits",
            "operation_id=persisted-operation&start=4&end=8&prompt=increase+volume",
        ));
        let operation_id = serde_json::from_str::<serde_json::Value>(&accepted.body)
            .expect("accepted edit JSON")["operationId"]
            .as_str()
            .expect("operation ID")
            .to_owned();
        let completed = wait_for_edit(&router, &accepted);
        assert_eq!(completed["status"], "completed");

        let recovered = router.handle(&request(
            "POST",
            "/api/edits",
            "operation_id=persisted-operation&start=4&end=8&prompt=increase+volume",
        ));
        assert_eq!(recovered.status, 200);
        let recovered: serde_json::Value =
            serde_json::from_str(&recovered.body).expect("recovered edit JSON");
        assert_eq!(recovered["status"], "completed");
        assert_eq!(recovered["operationId"], operation_id);

        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert_eq!(saved.project().edits.len(), 1);
        assert_eq!(saved.project().edits[0].prompt, "increase volume");
        assert_eq!(
            saved.project().edits[0].operation_id.as_deref(),
            Some(operation_id.as_str())
        );
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn repeated_operation_id_reuses_the_active_edit_job() {
        let gate = Arc::new(PlannerGate::new());
        let mut router = Router::demo();
        router.planner = Planner::GatedDemo(gate.clone());
        let body = "operation_id=client-operation&start=4&end=8&prompt=increase+volume";
        let accepted = router.handle(&request("POST", "/api/edits", body));
        gate.wait_until_started();

        let duplicate = router.handle(&request("POST", "/api/edits", body));
        assert_eq!(duplicate.status, 200);
        let accepted_json: serde_json::Value =
            serde_json::from_str(&accepted.body).expect("accepted edit JSON");
        let duplicate_json: serde_json::Value =
            serde_json::from_str(&duplicate.body).expect("duplicate edit JSON");
        assert_eq!(duplicate_json["id"], accepted_json["id"]);
        assert_eq!(duplicate_json["operationId"], "client-operation");
        assert_eq!(duplicate_json["status"], "running");

        gate.release();
        let completed = wait_for_edit(&router, &accepted);
        assert_eq!(completed["status"], "completed");
        let project: serde_json::Value =
            serde_json::from_str(&router.handle(&request("GET", "/api/project", "")).body)
                .expect("project JSON");
        assert_eq!(project["edits"].as_array().expect("project edits").len(), 1);
        assert_eq!(project["edits"][0]["operationId"], "client-operation");
    }

    #[test]
    fn async_edit_rejects_a_result_when_the_project_changed_while_planning() {
        let gate = Arc::new(PlannerGate::new());
        let mut router = Router::demo();
        router.planner = Planner::GatedDemo(gate.clone());
        let accepted = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=increase+volume",
        ));
        gate.wait_until_started();

        let mutation = router.handle(&request("POST", "/api/mix", "track_id=1&volume=0.5"));
        assert_eq!(mutation.status, 200);
        let newer_project = router.handle(&request("GET", "/api/project", "")).body;
        gate.release();

        let failed = wait_for_edit(&router, &accepted);
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 409);
        assert_eq!(
            failed["error"],
            "the project changed; submit the edit again"
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            newer_project
        );
        assert!(!newer_project.contains("increase volume"));
    }

    #[test]
    fn validates_api_requests_and_methods() {
        let router = Router::demo();
        assert_eq!(
            router
                .handle(&request("POST", "/api/edits", "start=1&end=2"))
                .status,
            422
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/edits",
                    "operation_id=not%20valid&start=4&end=8&prompt=increase+volume",
                ))
                .status,
            422
        );
        let before = router.handle(&request("GET", "/api/project", "")).body;
        let accepted = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=make+the+lead+louder",
        ));
        let failed = wait_for_edit(&router, &accepted);
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 422);
        assert_eq!(failed["error"], "track not found");
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=1&volume=bad&muted=true",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=1&volume=0.5&muted=maybe",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(router.handle(&request("GET", "/missing", "")).status, 404);
        assert_eq!(router.handle(&request("GET", "/api/undo", "")).status, 405);
    }

    #[test]
    fn parses_http_request_and_encoded_forms() {
        let body = "prompt=warm+%26+wide&start=0&end=4";
        let raw = format!(
            "POST /api/edits HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let parsed = Request::read(&mut raw.as_bytes()).expect("valid request");
        assert_eq!(parsed.path, "/api/edits");
        assert_eq!(parsed.headers["host"], "localhost");
        assert_eq!(parse_form(&parsed.body)["prompt"], "warm & wide");
    }

    #[test]
    fn rejects_untrusted_audio_requests() {
        let router = Router::demo();
        assert_eq!(
            router
                .handle(&request("GET", "/api/audio-access", ""))
                .status,
            403
        );

        let mut hostile = audio_request("/api/audio-access");
        hostile
            .headers
            .insert("origin".to_owned(), "http://127.0.0.1:18867".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "cross-site".to_owned());
        assert_eq!(router.handle(&hostile).status, 403);
    }

    #[test]
    fn streams_one_continuous_wav_through_the_reusable_media_endpoint() {
        let router = Router::demo();
        let mut project = router.lock_studio().project().clone();
        project.duration = 32.123_13;
        project.bpm = 113;
        *router.lock_studio() = Studio::from_project(project);
        let access = router.handle(&audio_request("/api/audio-access"));
        assert_eq!(access.status, 200);
        let access: serde_json::Value =
            serde_json::from_str(&access.body).expect("audio access JSON");
        assert_eq!(access["streamToken"], "test-audio-token");

        let version = router.lock_studio().project().version;
        let mut stream = Vec::new();
        router
            .write_playback_stream(
                &request(
                    "GET",
                    &format!("/api/audio-stream/test-audio-token/{version}/0"),
                    "",
                ),
                &mut stream,
            )
            .expect("continuous WAV stream");
        let body_start = find_bytes(&stream, b"\r\n\r\n").expect("HTTP response head") + 4;
        let response_head =
            std::str::from_utf8(&stream[..body_start]).expect("UTF-8 response head");
        let body = &stream[body_start..];
        let expected_samples =
            audio_analysis::playback_sample_count(0.0, router.lock_studio().project().duration);

        assert!(stream.starts_with(b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\n"));
        assert!(response_head.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(response_head.contains("Accept-Ranges: bytes\r\n"));
        assert_eq!(body.len(), 44 + expected_samples * 2);
        assert_eq!(&body[..4], b"RIFF");
        assert_eq!(&body[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(body[40..44].try_into().expect("WAV data length")) as usize,
            expected_samples * 2
        );
        let render_boundary = 44
            + (audio_analysis::MAX_REGION_SECONDS * audio_analysis::SAMPLE_RATE as f32) as usize
                * 2;
        assert_ne!(&body[render_boundary..render_boundary + 4], b"RIFF");

        let mut cookie_stream = Vec::new();
        let mut cookie_request = request(
            "GET",
            &format!("/api/audio-stream/test-audio-token/{version}/0"),
            "",
        );
        cookie_request
            .headers
            .insert("range".to_owned(), "bytes=0-43".to_owned());
        router
            .write_playback_stream_with_cancel(
                &cookie_request,
                &mut cookie_stream,
                || false,
                Some("daw_ai_user=0123456789abcdef0123456789abcdef; Path=/"),
            )
            .expect("cookie-bearing WAV stream");
        let cookie_head_end = find_bytes(&cookie_stream, b"\r\n\r\n").expect("cookie head") + 4;
        let cookie_head =
            std::str::from_utf8(&cookie_stream[..cookie_head_end]).expect("cookie UTF-8");
        assert!(
            cookie_head
                .contains("Set-Cookie: daw_ai_user=0123456789abcdef0123456789abcdef; Path=/\r\n")
        );

        let mut rejected = Vec::new();
        router
            .write_playback_stream(
                &request(
                    "GET",
                    &format!("/api/audio-stream/wrong-token/{version}/0"),
                    "",
                ),
                &mut rejected,
            )
            .expect("rejected stream response");
        assert!(rejected.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
    }

    #[test]
    fn export_streams_wav_bytes_and_sets_a_new_user_cookie() {
        let router = Router::demo();
        router.builtin_backend.store(true, Ordering::SeqCst);
        let mut project = router.lock_studio().project().clone();
        project.duration = 0.5;
        *router.lock_studio() = Studio::from_project(project);
        let mut response = Vec::new();

        router
            .write_export(
                &request("GET", "/api/export.wav", ""),
                &mut response,
                Some("daw_ai_user=0123456789abcdef0123456789abcdef; Path=/"),
            )
            .expect("streamed export");

        let body_start = find_bytes(&response, b"\r\n\r\n").expect("export response head") + 4;
        let head = std::str::from_utf8(&response[..body_start]).expect("export UTF-8 head");
        let expected_samples = audio_analysis::playback_sample_count(0.0, 0.5);
        assert!(
            head.contains("Set-Cookie: daw_ai_user=0123456789abcdef0123456789abcdef; Path=/\r\n")
        );
        assert_eq!(
            response.len() - body_start,
            WAV_HEADER_BYTES + expected_samples * 2
        );
        assert_eq!(&response[body_start..body_start + 4], b"RIFF");

        struct BackendFlippingWriter {
            bytes: Vec<u8>,
            backend: Arc<AtomicBool>,
            flipped: bool,
        }

        impl Write for BackendFlippingWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if !self.flipped {
                    self.backend.store(false, Ordering::SeqCst);
                    self.flipped = true;
                }
                self.bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        router.builtin_backend.store(true, Ordering::SeqCst);
        let mut flipping_response = BackendFlippingWriter {
            bytes: Vec::new(),
            backend: Arc::clone(&router.builtin_backend),
            flipped: false,
        };
        router
            .write_export(
                &request("GET", "/api/export.wav", ""),
                &mut flipping_response,
                Some("daw_ai_user=0123456789abcdef0123456789abcdef; Path=/"),
            )
            .expect("export with backend changed after response starts");
        assert_eq!(flipping_response.bytes, response);
    }

    #[test]
    fn serves_bounded_byte_ranges_for_day_long_audio() {
        let router = Router::demo();
        let mut project = router.lock_studio().project().clone();
        project.duration = 24.0 * 60.0 * 60.0;
        *router.lock_studio() = Studio::from_project(project);
        let version = router.lock_studio().project().version;
        let mut range_request = request(
            "GET",
            &format!("/api/audio-stream/test-audio-token/{version}/0"),
            "",
        );
        range_request
            .headers
            .insert("range".to_owned(), "bytes=0-99".to_owned());
        let mut response = Vec::new();

        router
            .write_playback_stream(&range_request, &mut response)
            .expect("partial WAV response");

        let body_start = find_bytes(&response, b"\r\n\r\n").expect("HTTP response head") + 4;
        let head = std::str::from_utf8(&response[..body_start]).expect("UTF-8 response head");
        let total_length =
            WAV_HEADER_BYTES + audio_analysis::playback_sample_count(0.0, 24.0 * 60.0 * 60.0) * 2;
        assert!(head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(head.contains("Content-Length: 100\r\n"));
        assert!(head.contains("Accept-Ranges: bytes\r\n"));
        assert!(head.contains(&format!("Content-Range: bytes 0-99/{total_length}\r\n")));
        assert_eq!(response.len() - body_start, 100);
        assert_eq!(&response[body_start..body_start + 4], b"RIFF");

        let open_range = audio_byte_range("bytes=44-", total_length).expect("open byte range");
        assert_eq!(open_range.len(), AUDIO_RANGE_SAMPLES * 2);
    }

    #[test]
    fn cancelled_stream_leaves_the_render_queue_without_rendering() {
        let renderer = Arc::new(AudioRenderer::default());
        let project = Project::demo();
        let cancelled = Arc::new(AtomicBool::new(false));
        let checked = Arc::new(std::sync::Barrier::new(2));
        let mut state = renderer
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.rendering = true;

        let second_renderer = Arc::clone(&renderer);
        let second_cancelled = Arc::clone(&cancelled);
        let second_checked = Arc::clone(&checked);
        let second = thread::spawn(move || {
            let first_check = AtomicBool::new(true);
            second_renderer.stream_region_with(
                &project,
                1,
                2,
                &|| {
                    if first_check.swap(false, Ordering::SeqCst) {
                        second_checked.wait();
                    }
                    second_cancelled.load(Ordering::SeqCst)
                },
                |_, _, _| panic!("a cancelled queued stream must not render"),
            )
        });

        checked.wait();
        cancelled.store(true, Ordering::SeqCst);
        state.rendering = false;
        drop(state);
        renderer.completed.notify_all();
        assert!(matches!(
            second.join().expect("cancelled stream thread"),
            Err(AudioRenderError::Cancelled)
        ));
    }

    #[test]
    fn rejects_content_lengths_that_overflow_the_request_bound() {
        let raw = format!(
            "POST /api/edits HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
        let error = Request::read(&mut raw.as_bytes())
            .err()
            .expect("oversized request must be rejected");
        assert_eq!(error, "request is too large");
    }

    #[test]
    fn rejects_cross_origin_mutations_without_changing_state() {
        let router = Router::demo();
        let mut hostile = request("POST", "/api/edits", "start=4&end=8&prompt=increase+volume");
        hostile
            .headers
            .insert("origin".to_owned(), "http://127.0.0.1:18867".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "cross-site".to_owned());

        assert_eq!(router.handle(&hostile).status, 403);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(!project.body.contains("increase volume"));

        hostile.path = "/api/sound-tools".to_owned();
        hostile.body =
            "track_id=2&tool=instrument&tool_id=201&parameter=preset&value=Surge+Lead".to_owned();
        assert_eq!(router.handle(&hostile).status, 403);
        let project = router.handle(&request("GET", "/api/project", ""));
        let project: serde_json::Value = serde_json::from_str(&project.body).expect("project JSON");
        assert_eq!(project["tracks"][1]["instrument"]["preset"], "Surge Bass");

        hostile
            .headers
            .insert("x-forwarded-host".to_owned(), "studio.example".to_owned());
        hostile
            .headers
            .insert("origin".to_owned(), "https://attacker.example".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&hostile).status, 403);

        hostile
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        assert_eq!(router.handle(&hostile).status, 200);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"preset\":\"Surge Lead\""));
    }

    #[test]
    fn supports_reverse_proxy_hosts_without_configuration() {
        let router = Router::demo();
        let mut forwarded = request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=preset&value=Surge+Lead",
        );
        forwarded.headers.insert(
            "x-forwarded-host".to_owned(),
            "studio.example:443, proxy.internal".to_owned(),
        );
        forwarded
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        forwarded
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&forwarded).status, 200);
    }

    #[test]
    fn supports_public_hosts_without_configuration() {
        let router = Router::demo();
        let mut public = request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=preset&value=Surge+Lead",
        );
        public
            .headers
            .insert("host".to_owned(), "studio.example".to_owned());
        public
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        public
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());

        assert_eq!(router.handle(&public).status, 200);
        public
            .headers
            .insert("origin".to_owned(), "https://attacker.example".to_owned());
        assert_eq!(router.handle(&public).status, 403);
    }

    #[test]
    fn serves_the_project_for_any_valid_hostname() {
        let router = Router::demo();
        let mut public = request("GET", "/api/project", "");
        public
            .headers
            .insert("host".to_owned(), "music.private.example:8443".to_owned());

        let response = router.handle(&public);
        assert_eq!(response.status, 200);
        assert!(response.body.contains("Neon First Light"));

        public.method = "POST".to_owned();
        public.path = "/api/sound-tools".to_owned();
        public.body =
            "track_id=2&tool=instrument&tool_id=201&parameter=preset&value=Surge+Lead".to_owned();
        public.headers.insert(
            "origin".to_owned(),
            "http://music.private.example:8443".to_owned(),
        );
        public
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&public).status, 200);

        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"preset\":\"Surge Lead\""));
    }

    #[test]
    fn rejects_malformed_host_authorities() {
        let router = Router::demo();
        let mut invalid = request("GET", "/api/project", "");
        invalid
            .headers
            .insert("host".to_owned(), "studio.example/path".to_owned());

        let response = router.handle(&invalid);
        assert_eq!(response.status, 400);
        assert!(!response.body.contains("Neon First Light"));
    }

    #[test]
    fn parses_public_and_ipv6_authorities() {
        assert_eq!(
            parse_authority("studio.example:8443"),
            Some(("studio.example", Some(8443)))
        );
        assert_eq!(parse_authority("[::1]:8888"), Some(("[::1]", Some(8888))));
        assert_eq!(parse_authority("studio.example/path"), None);
    }

    #[test]
    fn response_contains_security_and_length_headers() {
        let response = Response::json(200, "{\"ok\":true}".to_owned());
        let mut bytes = Vec::new();
        response.write(&mut bytes).expect("writable buffer");
        let rendered = String::from_utf8(bytes).expect("UTF-8 response");
        assert!(rendered.contains("Content-Length: 11"));
        assert!(rendered.contains("X-Content-Type-Options: nosniff"));
    }

    #[test]
    fn backend_selection_is_explicit_and_per_router() {
        let router = Router::demo();
        let initial = router.handle(&request("GET", "/api/backend", ""));
        assert_eq!(initial.status, 200);
        assert!(initial.body.contains("Surge XT"));

        let selected = router.handle(&request("POST", "/api/backend", "backend=built-in"));
        assert_eq!(selected.status, 200);
        assert!(router.builtin_backend.load(Ordering::Relaxed));
        assert_eq!(
            router
                .handle(&request("POST", "/api/backend", "backend=unknown"))
                .status,
            422
        );
    }

    #[test]
    fn cookie_scoped_users_have_independent_projects() {
        let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("daw-ai-users-test-{}-{id}", std::process::id()));
        let base = Router::demo();
        assert_eq!(
            base.handle(&request(
                "POST",
                "/api/mix",
                "track_id=2&volume=0.31&muted=false",
            ))
            .status,
            200
        );
        let registry = Arc::new(UserRegistry {
            root: root.clone(),
            planner: Planner::Demo,
            users: Mutex::new(HashMap::new()),
        });
        let base = Router {
            users: Some(registry),
            ..base
        };
        let (first, first_id) = base
            .scoped(&request("GET", "/api/project", ""))
            .expect("first user");
        let (second, second_id) = base
            .scoped(&request("GET", "/api/project", ""))
            .expect("second user");
        assert_ne!(first_id, second_id);
        assert!((first.lock_studio().project().tracks[1].volume - 0.31).abs() > f32::EPSILON);
        assert_ne!(first.gemini_session_root(), second.gemini_session_root());
        let first_session = first.gemini_session_root().join("first-session");
        let second_session = second.gemini_session_root().join("second-session");
        fs::create_dir_all(&first_session).expect("first session directory");
        fs::create_dir_all(&second_session).expect("second session directory");
        replace_text_file(
            &first_session.join("session.json"),
            r#"{"id":"first-private-session","updatedAt":1}"#,
        )
        .expect("first private session");
        replace_text_file(
            &second_session.join("session.json"),
            r#"{"id":"second-private-session","updatedAt":2}"#,
        )
        .expect("second private session");
        let first_sessions = first.handle(&request("GET", "/api/gemini-sessions", ""));
        assert!(first_sessions.body.contains("first-private-session"));
        assert!(!first_sessions.body.contains("second-private-session"));
        assert_eq!(
            first
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=2&volume=0.25&muted=false",
                ))
                .status,
            200
        );
        assert!((first.lock_studio().project().tracks[1].volume - 0.25).abs() < f32::EPSILON);
        assert!((second.lock_studio().project().tracks[1].volume - 0.25).abs() > f32::EPSILON);
        std::fs::remove_dir_all(root).expect("remove user test projects");
    }

    #[test]
    fn history_navigation_preserves_forward_snapshots() {
        let router = Router::demo();
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=2&volume=0.4&muted=false",
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=2&volume=1.1&muted=false",
                ))
                .status,
            200
        );
        let history: serde_json::Value =
            serde_json::from_str(&router.handle(&request("GET", "/api/history", "")).body)
                .expect("history response");
        let latest_version = router.lock_studio().project().version;
        assert_eq!(history["entries"][1]["source"], "Manual");
        assert_eq!(history["entries"][1]["summary"], "Manual project change");
        let restored: serde_json::Value = serde_json::from_str(
            &router
                .handle(&request("POST", "/api/history", "index=0"))
                .body,
        )
        .expect("restored project");
        assert_eq!(restored["version"], latest_version + 1);
        let forward: serde_json::Value = serde_json::from_str(
            &router
                .handle(&request("POST", "/api/history", "index=2"))
                .body,
        )
        .expect("forward project");
        assert_eq!(forward["version"], latest_version + 2);
        assert!((router.lock_studio().project().tracks[1].volume - 1.1).abs() < f32::EPSILON);
        let reloaded: serde_json::Value =
            serde_json::from_str(&router.handle(&request("GET", "/api/project", "")).body)
                .expect("reloaded project");
        assert_eq!(reloaded["canUndo"], true);
        assert_eq!(router.handle(&request("POST", "/api/undo", "")).status, 200);
        assert_eq!(router.handle(&request("POST", "/api/undo", "")).status, 200);
        assert_eq!(router.lock_studio().project().edits.len(), 0);
        assert_eq!(
            router
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .current,
            0
        );
    }

    #[test]
    fn history_is_bounded_by_bytes_and_keeps_the_current_project() {
        let mut snapshots = Vec::new();
        for index in 0..8 {
            let mut project = Studio::new().project().clone();
            project.name = format!("{index}{}", "x".repeat(1024 * 1024));
            snapshots.push(project);
        }
        let current_project = snapshots.last().expect("current snapshot").clone();
        let mut history = ProjectHistory {
            current: snapshots.len() - 1,
            snapshots,
        };

        trim_project_history(&current_project, &mut history);

        assert!(
            project_document(&current_project, &history).len()
                <= current_project.to_json().len() + MAX_HISTORY_BYTES
        );
        assert_eq!(
            history.snapshots[history.current].name,
            current_project.name
        );
        assert!(history.snapshots.len() < 8);
    }

    #[test]
    fn static_requests_do_not_create_users_and_user_storage_is_bounded() {
        assert!(!request_needs_user_scope(&request("GET", "/", "")));
        assert!(!request_needs_user_scope(&request("GET", "/app.js", "")));
        assert!(!request_needs_user_scope(&request("GET", "/missing", "")));
        assert!(!request_needs_user_scope(&request(
            "GET",
            "/api/missing",
            ""
        )));
        let mut audio_stream = request("GET", "/api/audio-stream/token/1/0", "");
        assert!(!request_needs_user_scope(&audio_stream));
        audio_stream.headers.insert(
            "cookie".to_owned(),
            "daw_ai_user=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        assert!(request_needs_user_scope(&audio_stream));
        assert!(request_needs_user_scope(&request(
            "GET",
            "/api/project",
            ""
        )));

        let mut cache = HashMap::new();
        let active_router = Router::demo();
        active_router
            .edit_jobs
            .create(100, None)
            .expect("active cached edit");
        cache.insert(
            "expired".to_owned(),
            CachedUser {
                router: active_router,
                last_used: Instant::now() - USER_CACHE_IDLE - Duration::from_secs(1),
                last_persisted: Instant::now(),
            },
        );
        for index in 0..MAX_CACHED_USERS {
            cache.insert(
                index.to_string(),
                CachedUser {
                    router: Router::demo(),
                    last_used: Instant::now(),
                    last_persisted: Instant::now(),
                },
            );
        }
        expire_and_bound_user_cache(&mut cache);
        assert!(cache.contains_key("expired"));
        assert!(cache.len() < MAX_CACHED_USERS);

        let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "daw-ai-user-limit-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("user limit root");
        for index in 0..MAX_PERSISTED_USERS {
            fs::create_dir(root.join(format!("{index:032x}"))).expect("bounded user directory");
        }
        replace_text_file(
            &root.join(format!("{:032x}", 0)).join(USER_LAST_USED_FILE),
            "0\n",
        )
        .expect("expired user marker");
        let base = Router {
            users: Some(Arc::new(UserRegistry {
                root: root.clone(),
                planner: Planner::Demo,
                users: Mutex::new(HashMap::new()),
            })),
            ..Router::demo()
        };
        let mut forged_user = request("GET", "/api/project", "");
        forged_user.headers.insert(
            "cookie".to_owned(),
            "daw_ai_user=ffffffffffffffffffffffffffffffff".to_owned(),
        );
        let error = match base.scoped(&forged_user) {
            Ok(_) => panic!("a client-chosen user ID must not allocate storage"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let response = scope_error_response(&error);
        assert_eq!(response.status, 401);
        assert!(
            response
                .set_cookie
                .is_some_and(|cookie| cookie.contains("Max-Age=0"))
        );
        let (_, replacement_id) = base
            .scoped(&request("GET", "/api/project", ""))
            .expect("expired persisted user is reclaimed");
        assert!(replacement_id.is_some());
        assert!(!root.join(format!("{:032x}", 0)).exists());
        let error = match base.scoped(&request("GET", "/api/project", "")) {
            Ok(_) => panic!("new persistent user must be rejected at the bound"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(scope_error_response(&error).status, 507);
        fs::remove_dir_all(root).expect("remove user limit root");
    }

    #[test]
    fn project_history_survives_a_server_restart() {
        let (router, project_path) = persisted_demo();
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=2&volume=0.4&muted=false",
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=2&volume=1.1&muted=false",
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .handle(&request("POST", "/api/history", "index=0"))
                .status,
            200
        );

        let reopened = ProjectStore::open(project_path.clone())
            .expect("reopened project")
            .1;
        let history = load_project_history(&project_path, reopened.project())
            .expect("reopened project history");
        assert_eq!(history.current, 0);
        assert_eq!(history.snapshots.len(), 3);
        assert!((history.snapshots[2].tracks[1].volume - 1.1).abs() < f32::EPSILON);
        assert!(
            std::fs::read_to_string(&project_path)
                .expect("project document")
                .contains("\"history\"")
        );
        std::fs::remove_file(project_path).expect("remove project test file");
    }

    #[test]
    fn reset_clears_project_history() {
        let (router, project_path) = persisted_demo();
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=2&volume=0.4&muted=false",
                ))
                .status,
            200
        );
        assert_eq!(
            router.handle(&request("POST", "/api/reset", "")).status,
            200
        );

        let reopened = ProjectStore::open(project_path.clone())
            .expect("reopened reset project")
            .1;
        let history = load_project_history(&project_path, reopened.project())
            .expect("reopened reset history");
        assert_eq!(history.current, 0);
        assert_eq!(history.snapshots.len(), 1);
        std::fs::remove_file(project_path).expect("remove project test file");
    }

    #[test]
    fn interrupt_is_terminal_and_blocks_late_completion() {
        let jobs = EditJobs::new();
        let (id, _, _) = jobs.create(100, None).expect("job");
        let other_jobs = (1..MAX_ACTIVE_EDIT_JOBS)
            .map(|_| jobs.create(100, None).expect("other active job").0)
            .collect::<Vec<_>>();
        let cancellation = jobs.cancellation(id);
        jobs.set_running(id, "editing", "working");
        assert!(jobs.interrupt(id));
        assert!(cancellation.load(Ordering::SeqCst));
        jobs.complete(id, "too late".to_owned());
        let response = jobs.response(id).expect("interrupted response");
        let body: serde_json::Value = serde_json::from_str(&response.body).expect("job JSON");
        assert_eq!(body["status"], "failed");
        assert_eq!(body["errorStatus"], 409);
        assert!(jobs.is_interrupted(id));
        assert!(jobs.create(100, None).is_err());
        jobs.worker_finished(id);
        assert!(jobs.create(100, None).is_ok());
        for other_id in other_jobs {
            jobs.fail(other_id, 500, "test cleanup".to_owned());
        }
    }

    #[test]
    fn fallback_user_ids_preserve_the_cookie_contract() {
        let first = fallback_operation_id(1);
        let second = fallback_operation_id(2);
        assert!(valid_user_id(&first));
        assert!(valid_user_id(&second));
        assert_ne!(first, second);
    }
}
