use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio_analysis;
use crate::audio_renderer::{AudioRenderError, AudioRenderPriority, AudioRenderer};
use crate::audio_stream::{
    ByteRange, WAV_HEADER_BYTES, bounded_audio_byte_range, wait_for_playback_window,
};
use crate::concurrency::{Limiter, Permit};
#[cfg(test)]
use crate::edit_jobs::fallback_operation_id;
use crate::edit_jobs::{EditJobs, accepted_edit_job_json, new_operation_id};
use crate::gemini::{EDIT_TIMEOUT_SECONDS, GeminiEdit, GeminiPlanner, PlannerError};
#[cfg(test)]
use crate::gemini_session::session_root;
use crate::gemini_session::{session_root_for_project, session_summaries_in};
use crate::gemini_tools::render_audio_request_cancellable;
#[cfg(test)]
use crate::http::valid_user_id;
#[cfg(test)]
use crate::http::{AUDIO_REQUEST_HEADER, MAX_REQUEST_HEADER_BYTES, parse_authority};
use crate::http::{Request, Response, write_response_head};
use crate::model::{Project, Studio, StudioError, json_string, valid_operation_id};
use crate::project_history::{
    ProjectHistory, open_project_with_history, project_document, save_project_state,
};
#[cfg(debug_assertions)]
use crate::prompt::EditPlan;
use crate::storage::{ProjectStore, replace_file, replace_text_file};

const MAX_ACTIVE_CONNECTIONS: usize = 64;
const MAX_ACTIVE_EDIT_JOBS: usize = 4;
const AUDIO_RANGE_SAMPLES: usize =
    (audio_analysis::MAX_REGION_SECONDS * audio_analysis::SAMPLE_RATE as f32) as usize;
const PLAYBACK_CHUNK_SAMPLES: usize = audio_analysis::SAMPLE_RATE as usize * 2;
const AUDIO_STREAM_LOOKAHEAD_SAMPLES: usize = PLAYBACK_CHUNK_SAMPLES * 2;
const TRACK_SPECTRUM_MAGIC: &[u8; 8] = b"DAWSPEC1";
const SPECTRUM_FFT_SAMPLES: usize = 1024;
const SPECTRUM_FRAME_SAMPLES: usize = audio_analysis::SAMPLE_RATE as usize / 30;
const SPECTRUM_BANDS: usize = 8;
const MAX_TRACK_SPECTRUM_WINDOW_MS: u64 = 64_000;
// Full render regions avoid replaying Surge's DSP preroll for every spectrum frame batch.
const SPECTRUM_RENDER_CHUNK_SAMPLES: usize = AUDIO_RANGE_SAMPLES;
const GEMINI_POLL_INTERVAL_MS: u64 = 1_000;
#[cfg(debug_assertions)]
const TEST_AI_POLL_INTERVAL_MS: u64 = 25;

#[derive(Clone, Copy)]
enum PlaybackPacing {
    RealTime,
    #[cfg(test)]
    Unpaced,
}

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const AUDIO_ENGINE_JS: &str = include_str!("../web/audio-engine.js");
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
                    PlaybackPacing::RealTime,
                );
            }
            if request.path.starts_with("/api/track-spectrum/") {
                let cancellation_stream = stream.try_clone()?;
                return scoped.write_track_spectrum_with_cancel(
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
    bounded_audio_byte_range(value, total_length, AUDIO_RANGE_SAMPLES * 4)
}

#[derive(Clone)]
struct Router {
    studio: Arc<Mutex<Studio>>,
    store: Option<ProjectStore>,
    ai: Ai,
    edit_jobs: Arc<EditJobs>,
    edit_limiter: Arc<Limiter>,
    audio_renderer: Arc<AudioRenderer>,
    spectrum_cache: Arc<Mutex<()>>,
    audio_token: Arc<String>,
    users: Option<Arc<UserRegistry>>,
    history: Arc<Mutex<ProjectHistory>>,
}

struct UserRegistry {
    root: PathBuf,
    ai: Ai,
    edit_limiter: Arc<Limiter>,
    audio_renderer: Arc<AudioRenderer>,
    users: Mutex<HashMap<String, CachedUser>>,
}

struct CachedUser {
    router: Router,
    last_used: Instant,
}

const MAX_CACHED_USERS: usize = 64;
const MAX_PERSISTED_USERS: usize = 256;
const USER_CACHE_IDLE: Duration = Duration::from_secs(60 * 60);

fn request_needs_user_scope(request: &Request) -> bool {
    matches!(
        request.path.as_str(),
        "/api/project"
            | "/api/gemini-sessions"
            | "/api/history"
            | "/api/edits"
            | "/api/duration"
            | "/api/mix"
            | "/api/undo"
            | "/api/reset"
            | "/api/audio-access"
            | "/api/export.wav"
    ) || request.path.starts_with("/api/edits/")
        || request.path.starts_with("/api/edit-operations/")
        || (request.path.starts_with("/api/audio-stream/") && request.user_id().is_some())
        || (request.path.starts_with("/api/track-spectrum/") && request.user_id().is_some())
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
enum Ai {
    Gemini,
    #[cfg(debug_assertions)]
    Deterministic(Duration),
    #[cfg(test)]
    GatedDeterministic(Arc<PlannerGate>),
}

#[cfg(test)]
struct PlannerGate {
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
impl PlannerGate {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, false)),
            changed: std::sync::Condvar::new(),
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

struct EditRequest {
    operation_id: String,
    prompt: String,
    start: f32,
    end: f32,
    project: crate::model::Project,
    batch_parameter_tools: bool,
    slim_prompt: bool,
    dynamic_tools: bool,
}

struct EditFailure {
    status: u16,
    message: String,
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
    fn write_track_spectrum_with_cancel(
        &self,
        request: &Request,
        output: &mut impl Write,
        is_cancelled: impl Fn() -> bool,
        set_cookie: Option<&str>,
    ) -> io::Result<()> {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host")).write(output);
        };
        let Some((token, version, start_milliseconds, window_milliseconds)) =
            track_spectrum_stream(&request.path)
        else {
            return Response::json(404, error_json("track spectrum not found")).write(output);
        };
        if request.method != "GET" {
            return Response::json(405, error_json("method not allowed"))
                .with_header("Allow", "GET")
                .write(output);
        }
        if token != self.audio_token.as_str() || !request.is_trusted_request(public_host) {
            return Response::json(403, error_json("cross-origin audio request rejected"))
                .write(output);
        }
        let project = self.lock_studio().project().clone();
        if version != project.version {
            return Response::json(
                409,
                error_json("project changed before spectrum was rendered"),
            )
            .write(output);
        }
        let request_cancelled =
            || is_cancelled() || self.lock_studio().project().version != version;
        let start_sample = audio_analysis::playback_start_sample_milliseconds(start_milliseconds);
        let project_end_sample = audio_analysis::playback_sample_count(0.0, project.duration);
        if start_sample >= project_end_sample {
            return Response::json(
                422,
                error_json("track spectrum start is outside the project"),
            )
            .write(output);
        }
        let window_samples = audio_analysis::playback_start_sample_milliseconds(
            window_milliseconds
                .unwrap_or(MAX_TRACK_SPECTRUM_WINDOW_MS)
                .clamp(1, MAX_TRACK_SPECTRUM_WINDOW_MS),
        );
        let end_sample = (start_sample + window_samples).min(project_end_sample);
        let frame_count = (end_sample - start_sample).div_ceil(SPECTRUM_FRAME_SAMPLES);
        let cache_path = self.spectrum_cache_path(start_milliseconds, window_samples as u64);
        if let Some(path) = &cache_path {
            let cached = {
                let _cache_guard = self
                    .spectrum_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                fs::read(path)
            };
            if let Ok(cached) = cached {
                let expected_length = 8usize
                    .saturating_add(32)
                    .saturating_add(project.tracks.len().saturating_mul(8))
                    .saturating_add(
                        frame_count
                            .saturating_mul(project.tracks.len())
                            .saturating_mul(SPECTRUM_BANDS),
                    );
                if cached.len() == expected_length
                    && cached[..8] == version.to_le_bytes()
                    && cached[8..16] == *TRACK_SPECTRUM_MAGIC
                {
                    write_response_head(
                        output,
                        200,
                        "application/vnd.daw-ai.track-spectrum",
                        cached.len() - 8,
                        &[("Cache-Control", "private, max-age=31536000, immutable")],
                        set_cookie,
                    )?;
                    return output.write_all(&cached[8..]);
                }
            }
        }
        let mut body = Vec::with_capacity(
            32 + project.tracks.len() * 8 + frame_count * project.tracks.len() * SPECTRUM_BANDS,
        );
        body.extend_from_slice(TRACK_SPECTRUM_MAGIC);
        body.extend_from_slice(&(project.tracks.len() as u32).to_le_bytes());
        body.extend_from_slice(&(frame_count as u32).to_le_bytes());
        body.extend_from_slice(&start_milliseconds.to_le_bytes());
        body.extend_from_slice(&(SPECTRUM_FRAME_SAMPLES as u32).to_le_bytes());
        body.extend_from_slice(&audio_analysis::SAMPLE_RATE.to_le_bytes());
        for track in &project.tracks {
            body.extend_from_slice(&track.id.to_le_bytes());
        }
        let mut cursor = start_sample;
        while cursor < end_sample {
            if request_cancelled() {
                return Ok(());
            }
            let chunk_end = (cursor + SPECTRUM_RENDER_CHUNK_SAMPLES).min(end_sample);
            let render_start = cursor.saturating_sub(SPECTRUM_FFT_SAMPLES / 2);
            let render_end = (chunk_end + SPECTRUM_FFT_SAMPLES / 2).min(project_end_sample);
            let stems = match self.audio_renderer.stream_stems_sample_range(
                &project,
                render_start,
                render_end,
                &request_cancelled,
            ) {
                Ok(stems) => stems,
                Err(AudioRenderError::Render(error)) => {
                    eprintln!("error: could not render track spectrum: {error}");
                    return Err(io::Error::other("could not render track spectrum"));
                }
                Err(AudioRenderError::Cancelled) => return Ok(()),
            };
            let chunk_frames = (chunk_end - cursor).div_ceil(SPECTRUM_FRAME_SAMPLES);
            for frame in 0..chunk_frames {
                let center = cursor - render_start + frame * SPECTRUM_FRAME_SAMPLES;
                for (expected, (track_id, region)) in project.tracks.iter().zip(&stems) {
                    if expected.id != *track_id {
                        return Err(io::Error::other(
                            "track spectrum order changed during render",
                        ));
                    }
                    body.extend_from_slice(&spectrum_levels(&region.samples, center));
                }
            }
            cursor = chunk_end;
        }
        if let Some(path) = &cache_path {
            let _cache_guard = self
                .spectrum_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut cached = Vec::with_capacity(8 + body.len());
            cached.extend_from_slice(&version.to_le_bytes());
            cached.extend_from_slice(&body);
            if let Err(error) = replace_file(path, &cached) {
                eprintln!("warning: could not persist track spectrum cache: {error}");
            }
        }
        write_response_head(
            output,
            200,
            "application/vnd.daw-ai.track-spectrum",
            body.len(),
            &[("Cache-Control", "private, max-age=31536000, immutable")],
            set_cookie,
        )?;
        output.write_all(&body)
    }

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
        let sample_count = audio_analysis::playback_sample_count(0.0, project.duration);
        let total_length = WAV_HEADER_BYTES.saturating_add(sample_count.saturating_mul(4));
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
            let region =
                match self
                    .audio_renderer
                    .stream_sample_range(&project, cursor, end, &|| false)
                {
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
        #[cfg(debug_assertions)]
        let ai = match std::env::var("DAW_AI_TEST_AI") {
            Ok(value) if value == "deterministic" => Ai::Deterministic(Duration::from_secs(2)),
            Ok(value) if value == "deterministic-fast" => Ai::Deterministic(Duration::ZERO),
            _ => Ai::Gemini,
        };
        #[cfg(not(debug_assertions))]
        let ai = Ai::Gemini;
        let project_path = std::env::var_os(crate::storage::PROJECT_PATH_ENV)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?.join("sound-graph.json"));
        let root = project_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("users");
        fs::create_dir_all(&root)?;
        let (store, studio, history) = open_project_with_history(project_path)?;
        save_project_state(&store, studio.project(), &history)?;
        let edit_limiter = Limiter::new(MAX_ACTIVE_EDIT_JOBS);
        let audio_renderer = Arc::new(AudioRenderer::default());
        let users = Arc::new(UserRegistry {
            root,
            ai: ai.clone(),
            edit_limiter: Arc::clone(&edit_limiter),
            audio_renderer: Arc::clone(&audio_renderer),
            users: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            history: Arc::new(Mutex::new(history)),
            studio: Arc::new(Mutex::new(studio)),
            store: Some(store),
            ai,
            edit_jobs: Arc::new(EditJobs::new()),
            edit_limiter,
            audio_renderer,
            spectrum_cache: Arc::new(Mutex::new(())),
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
            studio: Arc::new(Mutex::new(Studio::new())),
            store: None,
            ai: Ai::Deterministic(Duration::ZERO),
            edit_jobs: Arc::new(EditJobs::new()),
            edit_limiter: Limiter::new(MAX_ACTIVE_EDIT_JOBS),
            audio_renderer: Arc::new(AudioRenderer::default()),
            spectrum_cache: Arc::new(Mutex::new(())),
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
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "persistent user project limit reached; existing projects were preserved",
                ));
            }
        }
        fs::create_dir_all(&directory)?;
        let project_path = directory.join("sound-graph.json");
        if !project_path.exists() {
            let studio = Studio::new();
            let history = ProjectHistory::new(studio.project().clone());
            replace_text_file(&project_path, &project_document(studio.project(), &history))?;
        }
        let (store, studio, history) = open_project_with_history(project_path)?;
        save_project_state(&store, studio.project(), &history)?;
        let router = Self {
            history: Arc::new(Mutex::new(history)),
            studio: Arc::new(Mutex::new(studio)),
            store: Some(store),
            ai: registry.ai.clone(),
            edit_jobs: Arc::new(EditJobs::new()),
            edit_limiter: Arc::clone(&registry.edit_limiter),
            audio_renderer: Arc::clone(&registry.audio_renderer),
            spectrum_cache: Arc::new(Mutex::new(())),
            audio_token: Arc::new(new_operation_id(0)),
            users: None,
        };
        users.insert(
            user_id.clone(),
            CachedUser {
                router: router.clone(),
                last_used: Instant::now(),
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
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => Response::static_asset("text/html; charset=utf-8", INDEX_HTML),
            ("GET", "/app.css") => Response::static_asset("text/css; charset=utf-8", APP_CSS),
            ("GET", "/audio-engine.js") => {
                Response::static_asset("text/javascript; charset=utf-8", AUDIO_ENGINE_JS)
            }
            ("GET", "/app.js") => Response::static_asset("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/api/health") => Response::json(200, "{\"status\":\"ok\"}".to_owned()),
            ("GET", "/api/project") => {
                let studio = self.lock_studio();
                self.project_response(&studio)
            }
            ("GET", "/api/gemini-sessions") => self.gemini_sessions(),
            ("GET", "/api/history") => self.history_response(),
            ("POST", "/api/edits") => self.start_edit(&request.body),
            ("POST", "/api/duration") => self.change_duration(&request.body),
            ("POST", "/api/mix") => self.change_mix(&request.body),
            ("POST", "/api/logs") => Self::client_log(&request.body),
            ("POST", "/api/undo") => self.undo(),
            ("POST", "/api/reset") => self.reset(),
            ("POST", "/api/history") => self.select_history(&request.body),
            (
                _,
                "/api/edits" | "/api/duration" | "/api/mix" | "/api/logs" | "/api/undo"
                | "/api/reset",
            ) => Response::json(405, error_json("method not allowed")).with_header("Allow", "POST"),
            (_, "/api/project" | "/api/health" | "/api/gemini-sessions") => {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            }
            (_, "/api/history") => Response::json(405, error_json("method not allowed"))
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
        let batch_parameter_tools = match parse_optional_boolean(&form, "batch_parameter_tools") {
            Ok(value) => value,
            Err(message) => return Response::json(422, error_json(message)),
        };
        let slim_prompt = match parse_optional_boolean(&form, "slim_prompt") {
            Ok(value) => value,
            Err(message) => return Response::json(422, error_json(message)),
        };
        let dynamic_tools = match parse_optional_boolean(&form, "dynamic_tools") {
            Ok(value) => value,
            Err(message) => return Response::json(422, error_json(message)),
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
        let poll_after_ms = match &self.ai {
            Ai::Gemini => GEMINI_POLL_INTERVAL_MS,
            #[cfg(debug_assertions)]
            Ai::Deterministic(_) => TEST_AI_POLL_INTERVAL_MS,
            #[cfg(test)]
            Ai::GatedDeterministic(_) => TEST_AI_POLL_INTERVAL_MS,
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
        let Some(edit_permit) = self.edit_limiter.acquire() else {
            self.edit_jobs.remove(job_id);
            return Response::json(
                503,
                error_json("the server edit queue is full; retry shortly"),
            );
        };
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: prompt.to_owned(),
            start,
            end,
            project,
            batch_parameter_tools,
            slim_prompt,
            dynamic_tools,
        };
        let worker = self.clone();
        let spawn = thread::Builder::new()
            .name(format!("daw-ai-edit-{job_id}"))
            .spawn(move || worker.run_edit_job(job_id, edit, edit_permit));
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
        self.write_playback_stream_with_cancel(
            request,
            output,
            || false,
            None,
            PlaybackPacing::Unpaced,
        )
    }

    fn write_playback_stream_with_cancel(
        &self,
        request: &Request,
        output: &mut impl Write,
        is_cancelled: impl Fn() -> bool,
        set_cookie: Option<&str>,
        pacing: PlaybackPacing,
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
        let total_length = WAV_HEADER_BYTES.saturating_add(sample_count.saturating_mul(4));
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
            if matches!(pacing, PlaybackPacing::RealTime)
                && !wait_for_playback_window(
                    generated_samples,
                    AUDIO_STREAM_LOOKAHEAD_SAMPLES,
                    audio_analysis::SAMPLE_RATE,
                    stream_started,
                    &is_cancelled,
                )
            {
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
            let count = region
                .samples
                .len()
                .div_euclid(audio_analysis::CHANNEL_COUNT)
                .min(remaining);
            if is_cancelled() {
                return Ok(());
            }
            let pcm =
                audio_analysis::pcm_bytes(&region.samples[..count * audio_analysis::CHANNEL_COUNT]);
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
        let first_sample = pcm_start / 4;
        let end_sample = pcm_end.div_ceil(4);
        let region = match self.audio_renderer.stream_sample_range(
            project,
            stream_start_sample + first_sample,
            stream_start_sample + end_sample,
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
        let first_sample_byte = first_sample * 4;
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

    fn run_edit_job(&self, job_id: u64, edit: EditRequest, _edit_permit: Permit) {
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
            "The AI producer is planning, editing, and listening to the sound graph",
        );
        match &self.ai {
            Ai::Gemini => self.perform_gemini_edit(job_id, edit),
            #[cfg(debug_assertions)]
            Ai::Deterministic(delay) => self.perform_deterministic_edit(job_id, edit, *delay),
            #[cfg(test)]
            Ai::GatedDeterministic(gate) => {
                gate.wait_until_released();
                self.perform_deterministic_edit(job_id, edit, Duration::ZERO)
            }
        }
    }

    fn perform_gemini_edit(&self, job_id: u64, edit: EditRequest) -> Result<String, EditFailure> {
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        let cancellation = self.edit_jobs.cancellation(job_id);
        let render_cancellation = Arc::clone(&cancellation);
        let completed = GeminiPlanner::interpret_with_updates(
            &self.gemini_session_root(),
            &edit.prompt,
            edit.start,
            edit.end,
            &edit.project,
            edit.batch_parameter_tools,
            edit.slim_prompt,
            edit.dynamic_tools,
            cancellation,
            |request| {
                self.edit_jobs.set_running(
                    job_id,
                    "rendering",
                    "Rendering the current sound graph with Surge XT",
                );
                let cancelled = || render_cancellation.load(Ordering::SeqCst);
                let result = self
                    .audio_renderer
                    .render_with(AudioRenderPriority::Foreground, &cancelled, || {
                        render_audio_request_cancellable(request, || {
                            render_cancellation.load(Ordering::SeqCst)
                        })
                    })
                    .map_err(|error| match error {
                        AudioRenderError::Render(error) => error,
                        AudioRenderError::Cancelled => "audio render interrupted".to_owned(),
                    });
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

    #[cfg(debug_assertions)]
    fn perform_deterministic_edit(
        &self,
        job_id: u64,
        edit: EditRequest,
        delay: Duration,
    ) -> Result<String, EditFailure> {
        thread::sleep(delay);
        let summary = "Applied deterministic test edit".to_owned();
        let mut project = edit.project.clone();
        let track = project
            .tracks
            .first_mut()
            .ok_or_else(|| EditFailure::new(422, "track not found"))?;
        track.muted = !track.muted;
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        self.commit_gemini_update(
            job_id,
            &edit,
            &mut expected_version,
            &mut published_update,
            GeminiEdit {
                plan: EditPlan {
                    summary: summary.clone(),
                },
                project,
            },
        )
        .map_err(planner_failure)?;
        self.complete_gemini_operation(job_id, &edit, &mut expected_version, &summary)
            .map_err(planner_failure)?;
        Ok(summary)
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
        if !candidate.record_operation_step(&edit.operation_id, "Gemini", &summary) {
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

    fn change_duration(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(duration) = form
            .get("duration")
            .and_then(|value| value.parse::<f32>().ok())
        else {
            return Response::json(422, error_json("duration is required"));
        };
        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        match candidate.set_duration(duration) {
            Ok(()) => match self.commit(&mut studio, candidate) {
                Ok(()) => self.project_response(&studio),
                Err(response) => response,
            },
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn change_mix(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track ID is required"));
        };
        let muted = match form.get("muted").map(String::as_str) {
            Some("true") => true,
            Some("false") => false,
            _ => return Response::json(422, error_json("muted must be true or false")),
        };
        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        match candidate.set_mix(track_id, None, Some(muted)) {
            Ok(()) => match self.commit(&mut studio, candidate) {
                Ok(()) => self.project_response(&studio),
                Err(response) => response,
            },
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn gemini_sessions(&self) -> Response {
        match session_summaries_in(&self.gemini_session_root()) {
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
        let Some(previous_index) = history.parent(history.current) else {
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
        Response::json(
            200,
            studio.to_json_with_can_undo(history.parent(history.current).is_some()),
        )
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
                let previous_edit_count = history
                    .parent(index)
                    .and_then(|parent| history.snapshots.get(parent))
                    .map_or(0, |previous| previous.edits.len());
                let edit = (project.edits.len() > previous_edit_count)
                    .then(|| project.edits.last())
                    .flatten();
                let (summary, source, prompt, start, end) = if index == 0 {
                    ("Initial project", "Project", None, None, None)
                } else if let Some(edit) = edit {
                    let source = project
                        .edit_operations
                        .iter()
                        .find(|operation| operation.project_version == project.version)
                        .map_or("Gemini", |operation| operation.source.as_str());
                    (
                        edit.summary.as_str(),
                        source,
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
        Response::json(
            200,
            studio.to_json_with_can_undo(history.parent(history.current).is_some()),
        )
    }

    fn project_response(&self, studio: &Studio) -> Response {
        let history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let can_undo = history.parent(history.current).is_some();
        Response::json(200, studio.to_json_with_can_undo(can_undo))
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

    fn spectrum_cache_path(&self, start_milliseconds: u64, window_samples: u64) -> Option<PathBuf> {
        let project_path = self.store.as_ref()?.path();
        let parent = project_path.parent()?;
        let project_name = project_path.file_name()?.to_string_lossy();
        Some(parent.join(format!(
            ".{project_name}.track-spectrum-{start_milliseconds}-{window_samples}.cache"
        )))
    }

    fn gemini_session_root(&self) -> PathBuf {
        if let Some(store) = &self.store {
            return session_root_for_project(store.path());
        }
        #[cfg(test)]
        return session_root();
        #[cfg(not(test))]
        unreachable!("production routers always have project storage")
    }

    fn lock_studio(&self) -> std::sync::MutexGuard<'_, Studio> {
        self.studio
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
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

fn track_spectrum_stream(path: &str) -> Option<(&str, u64, u64, Option<u64>)> {
    let mut parts = path.strip_prefix("/api/track-spectrum/")?.split('/');
    let token = parts.next()?;
    let version = parts.next()?.parse::<u64>().ok()?;
    let start_milliseconds = parts.next()?.parse::<u64>().ok()?;
    let window_milliseconds = parts.next().map(str::parse::<u64>).transpose().ok()?;
    if token.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((token, version, start_milliseconds, window_milliseconds))
}

struct SpectrumFftTables {
    bit_reversed: Vec<usize>,
    window: Vec<f64>,
    cosine: Vec<f64>,
    sine: Vec<f64>,
}

fn spectrum_levels(samples: &[f32], center_frame: usize) -> [u8; SPECTRUM_BANDS] {
    static TABLES: OnceLock<SpectrumFftTables> = OnceLock::new();
    let tables = TABLES.get_or_init(|| {
        let bit_reversed = (0..SPECTRUM_FFT_SAMPLES)
            .map(|index: usize| index.reverse_bits() >> (usize::BITS - 10))
            .collect();
        let window = (0..SPECTRUM_FFT_SAMPLES)
            .map(|index| {
                0.5 - 0.5
                    * (2.0 * std::f64::consts::PI * index as f64
                        / (SPECTRUM_FFT_SAMPLES - 1) as f64)
                        .cos()
            })
            .collect();
        let cosine = (0..SPECTRUM_FFT_SAMPLES / 2)
            .map(|index| {
                (-2.0 * std::f64::consts::PI * index as f64 / SPECTRUM_FFT_SAMPLES as f64).cos()
            })
            .collect();
        let sine = (0..SPECTRUM_FFT_SAMPLES / 2)
            .map(|index| {
                (-2.0 * std::f64::consts::PI * index as f64 / SPECTRUM_FFT_SAMPLES as f64).sin()
            })
            .collect();
        SpectrumFftTables {
            bit_reversed,
            window,
            cosine,
            sine,
        }
    });
    let channel_fft = |channel: usize| {
        let mut real = vec![0.0f64; SPECTRUM_FFT_SAMPLES];
        let mut imaginary = vec![0.0f64; SPECTRUM_FFT_SAMPLES];
        let start = center_frame as isize - (SPECTRUM_FFT_SAMPLES / 2) as isize;
        for index in 0..SPECTRUM_FFT_SAMPLES {
            let sample = start + index as isize;
            if sample >= 0 {
                let source = sample as usize * 2 + channel;
                if source < samples.len() {
                    real[tables.bit_reversed[index]] =
                        samples[source] as f64 * tables.window[index];
                }
            }
        }
        let mut length = 2;
        while length <= SPECTRUM_FFT_SAMPLES {
            let table_step = SPECTRUM_FFT_SAMPLES / length;
            for offset in (0..SPECTRUM_FFT_SAMPLES).step_by(length) {
                for index in 0..length / 2 {
                    let cosine = tables.cosine[index * table_step];
                    let sine = tables.sine[index * table_step];
                    let even = offset + index;
                    let odd = even + length / 2;
                    let odd_real = real[odd] * cosine - imaginary[odd] * sine;
                    let odd_imaginary = real[odd] * sine + imaginary[odd] * cosine;
                    real[odd] = real[even] - odd_real;
                    imaginary[odd] = imaginary[even] - odd_imaginary;
                    real[even] += odd_real;
                    imaginary[even] += odd_imaginary;
                }
            }
            length *= 2;
        }
        (0..SPECTRUM_FFT_SAMPLES / 2)
            .map(|index| real[index].hypot(imaginary[index]) / (SPECTRUM_FFT_SAMPLES / 2) as f64)
            .collect::<Vec<_>>()
    };
    let left = channel_fft(0);
    let right = channel_fft(1);
    let minimum = 40.0f64;
    let maximum = audio_analysis::SAMPLE_RATE as f64 / 2.0;
    std::array::from_fn(|band| {
        let low_hz = minimum * (maximum / minimum).powf(band as f64 / SPECTRUM_BANDS as f64);
        let high_hz = minimum * (maximum / minimum).powf((band + 1) as f64 / SPECTRUM_BANDS as f64);
        let low_bin = ((low_hz / maximum * left.len() as f64).floor() as usize).max(1);
        let high_bin = ((high_hz / maximum * left.len() as f64).ceil() as usize)
            .max(low_bin + 1)
            .min(left.len());
        let sum = (low_bin..high_bin)
            .map(|bin| (left[bin].powi(2) + right[bin].powi(2)) / 2.0)
            .sum::<f64>();
        let magnitude = (sum / (high_bin - low_bin) as f64).sqrt().max(1e-5);
        (((20.0 * magnitude.log10() + 100.0) / 70.0).clamp(0.0, 1.0) * 255.0).round() as u8
    })
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

fn parse_optional_boolean(
    form: &HashMap<String, String>,
    name: &str,
) -> Result<bool, &'static str> {
    match form.get(name).map(String::as_str) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err("boolean setting must be true or false"),
    }
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

#[cfg(test)]
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
        StudioError::InvalidDuration => "duration must be between 1 second and 5 minutes",
        StudioError::InvalidChannel => "invalid channel change",
        StudioError::LastTrack => "create another track before deleting the only track",
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
                studio: Arc::new(Mutex::new(studio)),
                store: Some(store),
                ai: Ai::Deterministic(Duration::ZERO),
                edit_jobs: Arc::new(EditJobs::new()),
                edit_limiter: Limiter::new(MAX_ACTIVE_EDIT_JOBS),
                audio_renderer: Arc::new(AudioRenderer::default()),
                spectrum_cache: Arc::new(Mutex::new(())),
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
        let audio_engine = router.handle(&request("GET", "/audio-engine.js", ""));
        assert_eq!(audio_engine.status, 200);
        assert!(audio_engine.body.contains("createDawAiAudioEngine"));
        let project = router.handle(&request("GET", "/api/project", ""));
        assert_eq!(project.status, 200);
        assert!(project.body.contains("\"tracks\""));
    }

    #[test]
    fn changes_project_duration_with_server_side_bounds() {
        let router = Router::demo();
        let changed = router.handle(&request("POST", "/api/duration", "duration=300"));
        assert_eq!(changed.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&changed.body).expect("duration response")["duration"],
            300.0
        );
        assert_eq!(
            router
                .handle(&request("POST", "/api/duration", "duration=301"))
                .status,
            422
        );
        assert_eq!(
            router
                .handle(&request("POST", "/api/duration", "duration=0.5"))
                .status,
            422
        );
    }

    #[test]
    fn changes_track_mute_with_server_side_validation() {
        let router = Router::demo();
        let project: serde_json::Value =
            serde_json::from_str(&router.handle(&request("GET", "/api/project", "")).body)
                .expect("project response");
        let track_id = project["tracks"][0]["id"].as_u64().expect("track ID");
        let mut hostile = request(
            "POST",
            "/api/mix",
            &format!("track_id={track_id}&muted=true"),
        );
        hostile
            .headers
            .insert("origin".to_owned(), "http://attacker.invalid".to_owned());
        assert_eq!(router.handle(&hostile).status, 403);
        let changed = router.handle(&request(
            "POST",
            "/api/mix",
            &format!("track_id={track_id}&muted=true"),
        ));
        assert_eq!(changed.status, 200);
        let changed: serde_json::Value = serde_json::from_str(&changed.body).expect("mix response");
        assert_eq!(changed["tracks"][0]["muted"], true);
        assert_eq!(
            router
                .handle(&request("POST", "/api/mix", "track_id=0&muted=true"))
                .status,
            422
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    &format!("track_id={track_id}&muted=yes")
                ))
                .status,
            422
        );
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
    fn edit_jobs_allow_one_active_edit_per_project() {
        let jobs = EditJobs::new();
        let active = jobs.create(750, None).expect("active edit job").0;
        assert!(jobs.create(750, None).is_err());

        jobs.fail(active, 503, "planner stopped".to_owned());
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
        assert_eq!(completed["message"], "Applied deterministic test edit");
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
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn repeated_operation_id_reuses_the_active_edit_job() {
        let gate = Arc::new(PlannerGate::new());
        let mut router = Router::demo();
        router.ai = Ai::GatedDeterministic(gate.clone());
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
        assert!(project["edits"][0]["operationId"].is_null());
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
    fn rejects_ambiguous_or_unbounded_http_headers() {
        for raw in [
            "POST /api/logs HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n",
            "POST /api/logs HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "GET http://localhost/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET / HTTP/2\r\nHost: localhost\r\n\r\n",
            "GET / HTTP/1.1\r\nmalformed\r\n\r\n",
        ] {
            assert!(
                Request::read(&mut raw.as_bytes()).is_err(),
                "accepted {raw:?}"
            );
        }

        let oversized = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX-Fill: {}\r\n\r\n",
            "x".repeat(MAX_REQUEST_HEADER_BYTES)
        );
        assert!(Request::read(&mut oversized.as_bytes()).is_err());
    }

    #[test]
    fn validates_optional_batch_parameter_setting() {
        assert!(!parse_optional_boolean(&HashMap::new(), "batch_parameter_tools").unwrap());
        assert!(
            parse_optional_boolean(
                &parse_form("batch_parameter_tools=true"),
                "batch_parameter_tools"
            )
            .unwrap()
        );
        assert_eq!(
            parse_optional_boolean(
                &parse_form("batch_parameter_tools=enabled"),
                "batch_parameter_tools"
            )
            .unwrap_err(),
            "boolean setting must be true or false"
        );
        assert!(parse_optional_boolean(&parse_form("slim_prompt=true"), "slim_prompt").unwrap());
        assert!(
            parse_optional_boolean(&parse_form("dynamic_tools=true"), "dynamic_tools").unwrap()
        );
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
        project.duration = audio_analysis::MAX_REGION_SECONDS + 0.125;
        project.bpm = 113;
        project.tracks.truncate(1);
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
        assert_eq!(body.len(), 44 + expected_samples * 4);
        assert_eq!(&body[..4], b"RIFF");
        assert_eq!(&body[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(body[40..44].try_into().expect("WAV data length")) as usize,
            expected_samples * 4
        );
        let render_boundary = 44
            + (audio_analysis::MAX_REGION_SECONDS * audio_analysis::SAMPLE_RATE as f32) as usize
                * 4;
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
                PlaybackPacing::RealTime,
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
    fn track_spectrum_paths_accept_an_optional_startup_window() {
        assert_eq!(
            track_spectrum_stream("/api/track-spectrum/token/42/1500/2000"),
            Some(("token", 42, 1500, Some(2000)))
        );
        assert!(track_spectrum_stream("/api/track-spectrum/token/42/1500/nope").is_none());
    }

    #[test]
    fn backend_spectrum_reports_audible_tone_energy() {
        let frames = 2048;
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let sample = (2.0 * std::f32::consts::PI * 440.0 * frame as f32
                / audio_analysis::SAMPLE_RATE as f32)
                .sin()
                * 0.25;
            samples.extend_from_slice(&[sample, sample]);
        }
        assert!(
            spectrum_levels(&samples, 1024)
                .into_iter()
                .max()
                .unwrap_or(0)
                > 128
        );
    }

    #[test]
    fn persisted_spectrum_is_served_without_entering_the_renderer() {
        let (router, project_path) = persisted_demo();
        let mut project = router.lock_studio().project().clone();
        project.duration = 0.25;
        *router.lock_studio() = Studio::from_project(project.clone());
        let path = format!("/api/track-spectrum/test-audio-token/{}/0", project.version);
        let mut first = Vec::new();
        router
            .write_track_spectrum_with_cancel(
                &request("GET", &path, ""),
                &mut first,
                || false,
                None,
            )
            .expect("cold spectrum response");
        let cache_path = router
            .spectrum_cache_path(
                0,
                audio_analysis::playback_start_sample_milliseconds(MAX_TRACK_SPECTRUM_WINDOW_MS)
                    as u64,
            )
            .expect("spectrum cache path");
        assert!(cache_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&cache_path)
                    .expect("spectrum cache metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        router.audio_renderer.occupy_for_test();
        let cold_router = router.clone();
        let cold_path = format!(
            "/api/track-spectrum/test-audio-token/{}/100",
            project.version
        );
        let cold = thread::spawn(move || {
            let mut response = Vec::new();
            cold_router.write_track_spectrum_with_cancel(
                &request("GET", &cold_path, ""),
                &mut response,
                || false,
                None,
            )
        });
        router
            .audio_renderer
            .wait_until_queued_for_test(AudioRenderPriority::Background);
        let mut second = Vec::new();
        router
            .write_track_spectrum_with_cancel(
                &request("GET", &path, ""),
                &mut second,
                || false,
                None,
            )
            .expect("cached spectrum response");
        assert_eq!(first, second);

        router.audio_renderer.release_for_test();
        cold.join()
            .expect("cold spectrum worker")
            .expect("unrelated cold spectrum response");

        fs::remove_file(cache_path).expect("remove spectrum cache");
        let cold_cache_path = router
            .spectrum_cache_path(
                100,
                audio_analysis::playback_start_sample_milliseconds(MAX_TRACK_SPECTRUM_WINDOW_MS)
                    as u64,
            )
            .expect("cold spectrum cache path");
        fs::remove_file(cold_cache_path).expect("remove cold spectrum cache");
        fs::remove_file(project_path).expect("remove project fixture");
    }

    #[test]
    fn export_streams_wav_bytes_and_sets_a_new_user_cookie() {
        let router = Router::demo();
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
            WAV_HEADER_BYTES + expected_samples * 4
        );
        assert_eq!(&response[body_start..body_start + 4], b"RIFF");
    }

    #[test]
    fn serves_bounded_byte_ranges_for_maximum_wav_audio() {
        let router = Router::demo();
        let mut project = router.lock_studio().project().clone();
        project.duration = audio_analysis::MAX_WAV_SECONDS;
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
        let total_length = WAV_HEADER_BYTES
            + audio_analysis::playback_sample_count(0.0, audio_analysis::MAX_WAV_SECONDS) * 4;
        assert!(head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(head.contains("Content-Length: 100\r\n"));
        assert!(head.contains("Accept-Ranges: bytes\r\n"));
        assert!(head.contains(&format!("Content-Range: bytes 0-99/{total_length}\r\n")));
        assert_eq!(response.len() - body_start, 100);
        assert_eq!(&response[body_start..body_start + 4], b"RIFF");

        let open_range = audio_byte_range("bytes=44-", total_length).expect("open byte range");
        assert_eq!(open_range.len(), AUDIO_RANGE_SAMPLES * 4);
    }

    #[test]
    fn cancelled_stream_leaves_the_render_queue_without_rendering() {
        let renderer = Arc::new(AudioRenderer::default());
        let project = Project::demo();
        let cancelled = Arc::new(AtomicBool::new(false));
        let checked = Arc::new(std::sync::Barrier::new(2));
        renderer.occupy_for_test();

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
                AudioRenderPriority::Background,
                |_, _, _| -> Result<audio_analysis::AudioRegion, String> {
                    panic!("a cancelled queued stream must not render")
                },
            )
        });

        checked.wait();
        cancelled.store(true, Ordering::SeqCst);
        renderer.release_for_test();
        assert!(matches!(
            second.join().expect("cancelled stream thread"),
            Err(AudioRenderError::Cancelled)
        ));
    }

    #[test]
    fn panicking_render_releases_the_render_queue() {
        let renderer = AudioRenderer::default();
        let project = Project::demo();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = renderer.stream_region_with(
                &project,
                0,
                1,
                &|| false,
                AudioRenderPriority::Foreground,
                |_, _, _| -> Result<(), String> { panic!("simulated renderer panic") },
            );
        }));
        assert!(panic.is_err());

        assert!(matches!(
            renderer.stream_region_with(
                &project,
                0,
                1,
                &|| false,
                AudioRenderPriority::Foreground,
                |_, _, _| Ok(())
            ),
            Ok(())
        ));
    }

    #[test]
    fn panicking_cancellation_check_releases_a_queued_waiter() {
        let renderer = AudioRenderer::default();
        let project = Project::demo();
        renderer.occupy_for_test();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let first_check = AtomicBool::new(true);
            let _ = renderer.stream_region_with(
                &project,
                0,
                1,
                &|| {
                    if first_check.swap(false, Ordering::SeqCst) {
                        false
                    } else {
                        panic!("simulated cancellation panic")
                    }
                },
                AudioRenderPriority::Background,
                |_, _, _| Ok(()),
            );
        }));
        assert!(panic.is_err());

        assert_eq!(renderer.queued_for_test(AudioRenderPriority::Background), 0);
    }

    #[test]
    fn playback_render_precedes_queued_spectrum_work() {
        let renderer = Arc::new(AudioRenderer::default());
        renderer.occupy_for_test();
        let project = Project::demo();
        let (completed, order) = std::sync::mpsc::channel();

        let background_renderer = Arc::clone(&renderer);
        let background_project = project.clone();
        let background_completed = completed.clone();
        let background = thread::spawn(move || {
            background_renderer.stream_region_with(
                &background_project,
                1,
                2,
                &|| false,
                AudioRenderPriority::Background,
                |_, _, _| {
                    background_completed
                        .send("spectrum")
                        .expect("test receiver");
                    Ok(())
                },
            )
        });
        renderer.wait_until_queued_for_test(AudioRenderPriority::Background);

        let foreground_renderer = Arc::clone(&renderer);
        let foreground_completed = completed.clone();
        let foreground = thread::spawn(move || {
            foreground_renderer.stream_region_with(
                &project,
                1,
                2,
                &|| false,
                AudioRenderPriority::Foreground,
                |_, _, _| {
                    foreground_completed
                        .send("playback")
                        .expect("test receiver");
                    Ok(())
                },
            )
        });
        renderer.wait_until_queued_for_test(AudioRenderPriority::Foreground);
        renderer.release_for_test();

        assert_eq!(
            order
                .recv_timeout(Duration::from_secs(2))
                .expect("first render"),
            "playback"
        );
        assert!(matches!(foreground.join(), Ok(Ok(()))));
        assert!(matches!(background.join(), Ok(Ok(()))));
    }

    #[test]
    fn queued_spectrum_render_stops_when_its_project_version_is_stale() {
        let router = Router::demo();
        router.audio_renderer.occupy_for_test();
        let version = router.lock_studio().project().version;
        let request = request(
            "GET",
            &format!("/api/track-spectrum/test-audio-token/{version}/0/1000"),
            "",
        );
        let worker_router = router.clone();
        let worker = thread::spawn(move || {
            let mut response = Vec::new();
            let result = worker_router.write_track_spectrum_with_cancel(
                &request,
                &mut response,
                || false,
                None,
            );
            (result, response)
        });

        router
            .audio_renderer
            .wait_until_queued_for_test(AudioRenderPriority::Background);
        let mut project = router.lock_studio().project().clone();
        project.version += 1;
        *router.lock_studio() = Studio::from_project(project);
        router.audio_renderer.release_for_test();

        let (result, response) = worker.join().expect("spectrum worker");
        assert!(result.is_ok());
        assert!(response.is_empty());
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
    fn history_keeps_every_committed_state() {
        let mut history = ProjectHistory::new(Studio::new().project().clone());
        for index in 0..8 {
            let mut project = Studio::new().project().clone();
            project.name = index.to_string();
            project.version += index + 1;
            history.push(project);
        }
        assert_eq!(history.snapshots.len(), 9);
        assert_eq!(history.current, 8);
    }

    #[test]
    fn static_requests_do_not_create_users_and_user_storage_is_bounded() {
        assert!(!request_needs_user_scope(&request("GET", "/", "")));
        assert!(!request_needs_user_scope(&request("GET", "/app.js", "")));
        assert!(!request_needs_user_scope(&request(
            "GET",
            "/audio-engine.js",
            ""
        )));
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
            },
        );
        for index in 0..MAX_CACHED_USERS {
            cache.insert(
                index.to_string(),
                CachedUser {
                    router: Router::demo(),
                    last_used: Instant::now(),
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
        let edit_limiter = Limiter::new(MAX_ACTIVE_EDIT_JOBS);
        let audio_renderer = Arc::new(AudioRenderer::default());
        let base = Router {
            users: Some(Arc::new(UserRegistry {
                root: root.clone(),
                ai: Ai::Deterministic(Duration::ZERO),
                edit_limiter,
                audio_renderer,
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
        let error = match base.scoped(&request("GET", "/api/project", "")) {
            Ok(_) => panic!("new persistent user must be rejected at the bound"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(scope_error_response(&error).status, 507);
        assert!(root.join(format!("{:032x}", 0)).is_dir());
        fs::remove_dir_all(root).expect("remove user limit root");
    }

    #[test]
    fn user_routers_share_process_wide_edit_and_render_limits() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-shared-resources-{}-{}",
            std::process::id(),
            crate::storage::unique_test_id()
        ));
        fs::create_dir(&root).expect("shared resource root");
        let edit_limiter = Limiter::new(MAX_ACTIVE_EDIT_JOBS);
        let audio_renderer = Arc::new(AudioRenderer::default());
        let base = Router {
            edit_limiter: Arc::clone(&edit_limiter),
            audio_renderer: Arc::clone(&audio_renderer),
            users: Some(Arc::new(UserRegistry {
                root: root.clone(),
                ai: Ai::Deterministic(Duration::ZERO),
                edit_limiter,
                audio_renderer,
                users: Mutex::new(HashMap::new()),
            })),
            ..Router::demo()
        };

        let (first, _) = base
            .scoped(&request("GET", "/api/project", ""))
            .expect("first user");
        let (second, _) = base
            .scoped(&request("GET", "/api/project", ""))
            .expect("second user");

        assert!(Arc::ptr_eq(&first.edit_limiter, &second.edit_limiter));
        assert!(Arc::ptr_eq(&first.audio_renderer, &second.audio_renderer));
        fs::remove_dir_all(root).expect("remove shared resource root");
    }

    #[test]
    fn interrupt_is_terminal_and_blocks_late_completion() {
        let jobs = EditJobs::new();
        let (id, _, _) = jobs.create(100, None).expect("job");
        assert!(jobs.create(100, None).is_err());
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
