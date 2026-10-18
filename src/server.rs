mod transport;

pub use transport::run;
use transport::streaming_body;

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use http::Response as HttpResponse;

use crate::audio_analysis;
use crate::audio_renderer::{AudioRenderError, AudioRenderPriority, AudioRenderer};
use crate::audio_stream::{
    ByteRange, WAV_HEADER_BYTES, bounded_audio_byte_range, wait_for_playback_window,
};
use crate::concurrency::{Limiter, Permit, RecoverPoison};
use crate::edit_jobs::{EditJobCreateError, EditJobs, accepted_edit_job_json, new_operation_id};
use crate::gemini::{EDIT_TIMEOUT_SECONDS, GeminiEdit, GeminiPlanner, PlannerError};
#[cfg(test)]
use crate::gemini_session::session_root;
use crate::gemini_session::{session_root_for_project, session_summaries_in};
use crate::gemini_tools::render_audio_request_cancellable;
use crate::http::{HttpBody, Request, Response, empty_body, full_body, response_with_body};
#[cfg(test)]
use crate::model::Studio;
use crate::model::{Project, StudioError, valid_operation_id};
use crate::project_state::ProjectState;
#[cfg(debug_assertions)]
use crate::prompt::EditPlan;
use crate::storage::{read_bounded_file, replace_file};

const MAX_ACTIVE_EDIT_JOBS: usize = 4;
const MAX_ACTIVE_HTTP_STREAMS: usize = 64;
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

fn dispatch_request(
    router: Router,
    request: Request,
    request_id: u64,
    started: Instant,
) -> HttpResponse<HttpBody> {
    let (scoped, new_user) = if request_needs_user_scope(&request) {
        match router.scoped(&request) {
            Ok(scoped) => scoped,
            Err(error) => return scope_error_response(&error).into_http(),
        }
    } else {
        (router, None)
    };
    let new_user_cookie = new_user.as_ref().map(|user_id| {
        format!("daw_ai_user={user_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000")
    });
    if request.path == "/api/export.wav" {
        return scoped.export_response(&request, new_user_cookie.as_deref());
    }
    if request.path.starts_with("/api/audio-stream/") {
        return scoped.playback_response(&request, new_user_cookie.as_deref());
    }
    if request.path.starts_with("/api/track-spectrum/") {
        return scoped.track_spectrum_response(&request, new_user_cookie.as_deref());
    }
    let mut response = scoped.handle(&request);
    response.set_cookie = new_user_cookie;
    log_http_response(request_id, started, &request, &response);
    response.into_http()
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

fn audio_byte_range(value: &str, total_length: usize) -> Result<ByteRange, ()> {
    bounded_audio_byte_range(value, total_length, AUDIO_RANGE_SAMPLES * 4)
}

#[derive(Clone)]
struct Router {
    project: Arc<Mutex<ProjectState>>,
    ai: Ai,
    edit_jobs: Arc<EditJobs>,
    edit_limiter: Arc<Limiter>,
    stream_limiter: Arc<Limiter>,
    audio_renderer: Arc<AudioRenderer>,
    spectrum_cache: Arc<Mutex<()>>,
    audio_token: Arc<String>,
    users: Option<Arc<UserRegistry>>,
}

struct UserRegistry {
    root: PathBuf,
    ai: Ai,
    edit_limiter: Arc<Limiter>,
    stream_limiter: Arc<Limiter>,
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
        Arc::strong_count(&self.router.project) == 1 && !self.router.edit_jobs.has_active()
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
        let mut state = self.state.lock().recover_poison();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).recover_poison();
        }
    }

    fn wait_until_started(&self) {
        let state = self.state.lock().recover_poison();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
            .recover_poison();
        assert!(state.0, "planner did not reach the test gate");
    }

    fn release(&self) {
        let mut state = self.state.lock().recover_poison();
        state.1 = true;
        self.changed.notify_all();
    }
}

struct EditRequest {
    operation_id: String,
    prompt: String,
    start: f32,
    end: f32,
    deadline: Instant,
    project: crate::model::Project,
    new_prompt: bool,
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

struct BinaryResponse {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
enum PlaybackSelection {
    Full,
    Range(ByteRange),
    Unsatisfiable,
}

#[derive(Debug)]
struct PlaybackPlan {
    project: Project,
    version: u64,
    stream_start_sample: usize,
    sample_count: usize,
    total_length: usize,
    selection: PlaybackSelection,
}

struct TrackSpectrumPlan {
    project: Project,
    version: u64,
    start_milliseconds: u64,
    start_sample: usize,
    end_sample: usize,
    frame_count: usize,
    cache_path: Option<PathBuf>,
}

enum TrackSpectrumPreparation {
    Ready(BinaryResponse),
    Render(TrackSpectrumPlan),
}

impl TrackSpectrumPlan {
    fn cache_key(&self) -> [u8; 24] {
        let mut key = [0; 24];
        key[..8].copy_from_slice(&self.version.to_le_bytes());
        key[8..16].copy_from_slice(&self.start_milliseconds.to_le_bytes());
        key[16..].copy_from_slice(&(self.end_sample as u64).to_le_bytes());
        key
    }

    fn response_length(&self) -> usize {
        32 + self.project.tracks.len() * 8
            + self.frame_count * self.project.tracks.len() * SPECTRUM_BANDS
    }
}

impl BinaryResponse {
    fn into_http(self, set_cookie: Option<&str>) -> HttpResponse<HttpBody> {
        let headers = self
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        response_with_body(
            self.status,
            self.content_type,
            self.body.len(),
            &headers,
            set_cookie,
            full_body(self.body),
        )
    }
}

fn stream_overload_response(set_cookie: Option<&str>) -> HttpResponse<HttpBody> {
    let mut response = Response::json(
        503,
        error_json("too many streaming responses are active; retry shortly"),
    );
    response.set_cookie = set_cookie.map(str::to_owned);
    response.into_http()
}

impl Router {
    fn track_spectrum_response(
        &self,
        request: &Request,
        set_cookie: Option<&str>,
    ) -> HttpResponse<HttpBody> {
        match self.prepare_track_spectrum(request) {
            Ok(TrackSpectrumPreparation::Ready(response)) => response.into_http(set_cookie),
            Ok(TrackSpectrumPreparation::Render(plan)) => {
                let Some(stream_permit) = self.stream_limiter.acquire() else {
                    return stream_overload_response(set_cookie);
                };
                let content_length = plan.response_length();
                let worker = self.clone();
                let body = streaming_body(stream_permit, move |output, is_cancelled| match worker
                    .render_track_spectrum(plan, is_cancelled)
                {
                    Ok(Some(response)) => output.write_all(&response.body),
                    Ok(None) => Ok(()),
                    Err(response) => {
                        eprintln!("track spectrum stream failed: {}", response.body);
                        Err(io::Error::other("could not render track spectrum"))
                    }
                });
                response_with_body(
                    200,
                    "application/vnd.daw-ai.track-spectrum",
                    content_length,
                    &[("Cache-Control", "private, max-age=31536000, immutable")],
                    set_cookie,
                    body,
                )
            }
            Err(response) => response.into_http(),
        }
    }

    #[cfg(test)]
    fn track_spectrum_data(
        &self,
        request: &Request,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<Option<BinaryResponse>, Response> {
        match self.prepare_track_spectrum(request)? {
            TrackSpectrumPreparation::Ready(response) => Ok(Some(response)),
            TrackSpectrumPreparation::Render(plan) => {
                self.render_track_spectrum(plan, &is_cancelled)
            }
        }
    }

    fn prepare_track_spectrum(
        &self,
        request: &Request,
    ) -> Result<TrackSpectrumPreparation, Response> {
        let Some(public_host) = request.public_host() else {
            return Err(Response::json(400, error_json("invalid host")));
        };
        let Some((token, version, start_milliseconds, window_milliseconds)) =
            track_spectrum_stream(&request.path)
        else {
            return Err(Response::json(404, error_json("track spectrum not found")));
        };
        if request.method != "GET" {
            return Err(
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            );
        }
        if token != self.audio_token.as_str() || !request.is_trusted_request(public_host) {
            return Err(Response::json(
                403,
                error_json("cross-origin audio request rejected"),
            ));
        }
        let project = self.lock_project().project().clone();
        if version != project.version {
            return Err(Response::json(
                409,
                error_json("project changed before spectrum was rendered"),
            ));
        }
        let start_sample = audio_analysis::playback_start_sample_milliseconds(start_milliseconds);
        let project_end_sample = audio_analysis::playback_sample_count(0.0, project.duration);
        if start_sample >= project_end_sample {
            return Err(Response::json(
                422,
                error_json("track spectrum start is outside the project"),
            ));
        }
        let window_samples = audio_analysis::playback_start_sample_milliseconds(
            window_milliseconds
                .unwrap_or(MAX_TRACK_SPECTRUM_WINDOW_MS)
                .clamp(1, MAX_TRACK_SPECTRUM_WINDOW_MS),
        );
        let end_sample = (start_sample + window_samples).min(project_end_sample);
        let frame_count = (end_sample - start_sample).div_ceil(SPECTRUM_FRAME_SAMPLES);
        let plan = TrackSpectrumPlan {
            project,
            version,
            start_milliseconds,
            start_sample,
            end_sample,
            frame_count,
            cache_path: self.spectrum_cache_path(),
        };
        let cache_key = plan.cache_key();
        if let Some(path) = &plan.cache_path {
            let expected_length = cache_key.len() + plan.response_length();
            let cached = {
                let _cache_guard = self.spectrum_cache.lock().recover_poison();
                read_bounded_file(path, expected_length as u64, "track spectrum cache")
            };
            if let Ok(cached) = cached
                && cached.len() == expected_length
                && cached[..cache_key.len()] == cache_key
                && cached[cache_key.len()..].starts_with(TRACK_SPECTRUM_MAGIC)
            {
                return Ok(TrackSpectrumPreparation::Ready(BinaryResponse {
                    status: 200,
                    content_type: "application/vnd.daw-ai.track-spectrum",
                    headers: vec![(
                        "Cache-Control".to_owned(),
                        "private, max-age=31536000, immutable".to_owned(),
                    )],
                    body: cached[cache_key.len()..].to_vec(),
                }));
            }
        }
        Ok(TrackSpectrumPreparation::Render(plan))
    }

    fn render_track_spectrum(
        &self,
        plan: TrackSpectrumPlan,
        is_cancelled: &(impl Fn() -> bool + ?Sized),
    ) -> Result<Option<BinaryResponse>, Response> {
        let request_cancelled =
            || is_cancelled() || self.lock_project().project().version != plan.version;
        let mut body = Vec::with_capacity(plan.response_length());
        body.extend_from_slice(TRACK_SPECTRUM_MAGIC);
        body.extend_from_slice(&(plan.project.tracks.len() as u32).to_le_bytes());
        body.extend_from_slice(&(plan.frame_count as u32).to_le_bytes());
        body.extend_from_slice(&plan.start_milliseconds.to_le_bytes());
        body.extend_from_slice(&(SPECTRUM_FRAME_SAMPLES as u32).to_le_bytes());
        body.extend_from_slice(&audio_analysis::SAMPLE_RATE.to_le_bytes());
        for track in &plan.project.tracks {
            body.extend_from_slice(&track.id.to_le_bytes());
        }
        let project_end_sample = audio_analysis::playback_sample_count(0.0, plan.project.duration);
        let mut cursor = plan.start_sample;
        while cursor < plan.end_sample {
            if request_cancelled() {
                return Ok(None);
            }
            let chunk_end = (cursor + SPECTRUM_RENDER_CHUNK_SAMPLES).min(plan.end_sample);
            let render_start = cursor.saturating_sub(SPECTRUM_FFT_SAMPLES / 2);
            let render_end = (chunk_end + SPECTRUM_FFT_SAMPLES / 2).min(project_end_sample);
            let stems = match self.audio_renderer.stream_stems_sample_range(
                &plan.project,
                render_start,
                render_end,
                &request_cancelled,
            ) {
                Ok(stems) => stems,
                Err(AudioRenderError::Render(error)) => {
                    eprintln!("error: could not render track spectrum: {error}");
                    return Err(Response::json(
                        500,
                        error_json("could not render track spectrum"),
                    ));
                }
                Err(AudioRenderError::Cancelled) => return Ok(None),
            };
            let chunk_frames = (chunk_end - cursor).div_ceil(SPECTRUM_FRAME_SAMPLES);
            for frame in 0..chunk_frames {
                let center = cursor - render_start + frame * SPECTRUM_FRAME_SAMPLES;
                for (expected, (track_id, region)) in plan.project.tracks.iter().zip(&stems) {
                    if expected.id != *track_id {
                        return Err(Response::json(
                            500,
                            error_json("track spectrum order changed during render"),
                        ));
                    }
                    body.extend_from_slice(&spectrum_levels(&region.samples, center));
                }
            }
            cursor = chunk_end;
        }
        if let Some(path) = &plan.cache_path {
            let _cache_guard = self.spectrum_cache.lock().recover_poison();
            let cache_key = plan.cache_key();
            let mut cached = Vec::with_capacity(cache_key.len() + body.len());
            cached.extend_from_slice(&cache_key);
            cached.extend_from_slice(&body);
            if let Err(error) = replace_file(path, &cached) {
                eprintln!("warning: could not persist track spectrum cache: {error}");
            }
        }
        Ok(Some(BinaryResponse {
            status: 200,
            content_type: "application/vnd.daw-ai.track-spectrum",
            headers: vec![(
                "Cache-Control".to_owned(),
                "private, max-age=31536000, immutable".to_owned(),
            )],
            body,
        }))
    }

    fn export_response(
        &self,
        request: &Request,
        set_cookie: Option<&str>,
    ) -> HttpResponse<HttpBody> {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host")).into_http();
        };
        if request.method != "GET" {
            return Response::json(405, error_json("method not allowed"))
                .with_header("Allow", "GET")
                .into_http();
        }
        if !request.is_trusted_request(public_host) {
            return Response::json(403, error_json("cross-origin export request rejected"))
                .into_http();
        }
        let project = self.lock_project().project().clone();
        let sample_count = audio_analysis::playback_sample_count(0.0, project.duration);
        let total_length = WAV_HEADER_BYTES.saturating_add(sample_count.saturating_mul(4));
        let Some(stream_permit) = self.stream_limiter.acquire() else {
            return stream_overload_response(set_cookie);
        };
        let worker = self.clone();
        let body = streaming_body(stream_permit, move |output, is_cancelled| {
            worker.write_export_body(&project, sample_count, output, is_cancelled)
        });
        response_with_body(
            200,
            "audio/wav",
            total_length,
            &[
                ("Cache-Control", "no-store"),
                ("Content-Disposition", "attachment; filename=project.wav"),
            ],
            set_cookie,
            body,
        )
    }

    fn write_export_body(
        &self,
        project: &Project,
        sample_count: usize,
        output: &mut impl Write,
        is_cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        if is_cancelled() {
            return Ok(());
        }
        output.write_all(&audio_analysis::wav_header(sample_count))?;
        let mut cursor = 0;
        while cursor < sample_count {
            if is_cancelled() {
                return Ok(());
            }
            let end = (cursor + AUDIO_RANGE_SAMPLES).min(sample_count);
            let region =
                match self
                    .audio_renderer
                    .stream_sample_range(project, cursor, end, is_cancelled)
                {
                    Ok(region) => region,
                    Err(AudioRenderError::Render(error)) => {
                        eprintln!("error: could not render export: {error}");
                        return Err(io::Error::other("could not render export"));
                    }
                    Err(AudioRenderError::Cancelled) => return Ok(()),
                };
            if is_cancelled() {
                return Ok(());
            }
            output.write_all(&audio_analysis::pcm_bytes(&region.samples))?;
            cursor = end;
        }
        Ok(())
    }

    fn new() -> io::Result<Self> {
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
        let project = ProjectState::open(project_path.clone())?;
        discard_obsolete_spectrum_caches(&project_path);
        let edit_limiter = Limiter::new(MAX_ACTIVE_EDIT_JOBS);
        let stream_limiter = Limiter::new(MAX_ACTIVE_HTTP_STREAMS);
        let audio_renderer = Arc::new(AudioRenderer::default());
        let users = Arc::new(UserRegistry {
            root,
            ai: ai.clone(),
            edit_limiter: Arc::clone(&edit_limiter),
            stream_limiter: Arc::clone(&stream_limiter),
            audio_renderer: Arc::clone(&audio_renderer),
            users: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            project: Arc::new(Mutex::new(project)),
            ai,
            edit_jobs: Arc::new(EditJobs::new()),
            edit_limiter,
            stream_limiter,
            audio_renderer,
            spectrum_cache: Arc::new(Mutex::new(())),
            audio_token: Arc::new(new_operation_id().map_err(io::Error::other)?),
            users: Some(users),
        })
    }

    #[cfg(test)]
    fn demo() -> Self {
        Self {
            project: Arc::new(Mutex::new(ProjectState::in_memory(Studio::new()))),
            ai: Ai::Deterministic(Duration::ZERO),
            edit_jobs: Arc::new(EditJobs::new()),
            edit_limiter: Limiter::new(MAX_ACTIVE_EDIT_JOBS),
            stream_limiter: Limiter::new(MAX_ACTIVE_HTTP_STREAMS),
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
        let user_id = match existing {
            Some(existing) => existing.to_owned(),
            None => new_operation_id().map_err(io::Error::other)?,
        };
        let mut users = registry.users.lock().recover_poison();
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
        let project = ProjectState::open_user(project_path.clone())?;
        discard_obsolete_spectrum_caches(&project_path);
        let router = Self {
            project: Arc::new(Mutex::new(project)),
            ai: registry.ai.clone(),
            edit_jobs: Arc::new(EditJobs::new()),
            edit_limiter: Arc::clone(&registry.edit_limiter),
            stream_limiter: Arc::clone(&registry.stream_limiter),
            audio_renderer: Arc::clone(&registry.audio_renderer),
            spectrum_cache: Arc::new(Mutex::new(())),
            audio_token: Arc::new(new_operation_id().map_err(io::Error::other)?),
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
                serde_json::json!({"streamToken":self.audio_token.as_str()}).to_string(),
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
                    self.audio_renderer.wake_waiters();
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
                let state = self.lock_project();
                self.project_response(&state)
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
        let new_prompt = match parse_optional_boolean(&form, "new_prompt") {
            Ok(value) => value,
            Err(message) => return Response::json(422, error_json(message)),
        };
        let project = {
            let state = self.lock_project();
            if let Err(error) = state.studio().validate_edit(start, end, prompt) {
                return Response::json(422, studio_error(error));
            }
            state.project().clone()
        };
        if let Some(operation_id) = operation_id
            && let Some(operation) = project
                .edit_operations
                .iter()
                .find(|operation| operation.operation_id == operation_id)
        {
            return Response::json(200, recovered_operation_json(operation));
        }
        let poll_after_ms = match &self.ai {
            Ai::Gemini => GEMINI_POLL_INTERVAL_MS,
            #[cfg(debug_assertions)]
            Ai::Deterministic(_) => TEST_AI_POLL_INTERVAL_MS,
            #[cfg(test)]
            Ai::GatedDeterministic(_) => TEST_AI_POLL_INTERVAL_MS,
        };
        let Some(edit_permit) = self.edit_limiter.acquire() else {
            if let Some(response) = operation_id
                .and_then(|operation_id| self.edit_jobs.response_for_operation(operation_id))
            {
                return response;
            }
            return Response::json(
                503,
                error_json("the server edit queue is full; retry shortly"),
            );
        };
        let (job_id, operation_id, created) = match self
            .edit_jobs
            .create(poll_after_ms, operation_id)
        {
            Ok(job) => job,
            Err(EditJobCreateError::Capacity) => {
                return Response::json(503, error_json("too many edits are already being planned"));
            }
            Err(EditJobCreateError::Entropy(error)) => {
                eprintln!("error: could not create a secure operation ID: {error}");
                return Response::json(500, error_json("could not create the edit job"));
            }
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
            deadline: Instant::now() + Duration::from_secs(EDIT_TIMEOUT_SECONDS),
            project,
            new_prompt,
        };
        let worker = self.clone();
        let spawn = thread::Builder::new()
            .name(format!("daw-ai-edit-{job_id}"))
            .spawn(move || worker.run_edit_job(job_id, edit, edit_permit));
        if let Err(error) = spawn {
            self.edit_jobs
                .fail(job_id, 503, "could not start the edit worker".to_owned());
            eprintln!("error: could not start edit worker: {error}");
            return Response::json(503, error_json("could not start the edit worker"));
        }
        Response::json(
            202,
            accepted_edit_job_json(job_id, &operation_id, poll_after_ms),
        )
    }

    fn playback_plan(&self, request: &Request) -> Result<PlaybackPlan, Response> {
        let Some(public_host) = request.public_host() else {
            return Err(Response::json(400, error_json("invalid host")));
        };
        if request.method != "GET" {
            return Err(
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            );
        }
        let Some((token, version, start_milliseconds)) = playback_audio_stream(&request.path)
        else {
            return Err(Response::json(404, error_json("audio stream not found")));
        };
        if token != self.audio_token.as_str() || !request.is_trusted_request(public_host) {
            return Err(Response::json(
                403,
                error_json("cross-origin audio request rejected"),
            ));
        }

        let project = self.lock_project().project().clone();
        if version != project.version {
            return Err(Response::json(
                409,
                error_json("project changed before playback started"),
            ));
        }
        let stream_start_sample =
            audio_analysis::playback_start_sample_milliseconds(start_milliseconds);
        let project_end_sample = audio_analysis::playback_sample_count(0.0, project.duration);
        if stream_start_sample >= project_end_sample {
            return Err(Response::json(
                422,
                error_json("playback start is outside the project"),
            ));
        }

        let sample_count = project_end_sample - stream_start_sample;
        let total_length = WAV_HEADER_BYTES.saturating_add(sample_count.saturating_mul(4));
        let selection = request
            .headers
            .get("range")
            .map_or(PlaybackSelection::Full, |range| {
                match audio_byte_range(range, total_length) {
                    Ok(range) => PlaybackSelection::Range(range),
                    Err(()) => PlaybackSelection::Unsatisfiable,
                }
            });
        Ok(PlaybackPlan {
            project,
            version,
            stream_start_sample,
            sample_count,
            total_length,
            selection,
        })
    }

    fn playback_response(
        &self,
        request: &Request,
        set_cookie: Option<&str>,
    ) -> HttpResponse<HttpBody> {
        let plan = match self.playback_plan(request) {
            Ok(plan) => plan,
            Err(response) => return response.into_http(),
        };
        match plan.selection {
            PlaybackSelection::Unsatisfiable => {
                let content_range = format!("bytes */{}", plan.total_length);
                response_with_body(
                    416,
                    "audio/wav",
                    0,
                    &[
                        ("Cache-Control", "no-store"),
                        ("Accept-Ranges", "bytes"),
                        ("Content-Range", &content_range),
                    ],
                    set_cookie,
                    empty_body(),
                )
            }
            PlaybackSelection::Range(range) => {
                let Some(stream_permit) = self.stream_limiter.acquire() else {
                    return stream_overload_response(set_cookie);
                };
                let content_range =
                    format!("bytes {}-{}/{}", range.start, range.end, plan.total_length);
                let worker = self.clone();
                let body = streaming_body(stream_permit, move |output, is_cancelled| {
                    worker.write_playback_byte_range(
                        &plan.project,
                        plan.stream_start_sample,
                        plan.sample_count,
                        range,
                        output,
                        is_cancelled,
                    )
                });
                response_with_body(
                    206,
                    "audio/wav",
                    range.len(),
                    &[
                        ("Cache-Control", "no-store"),
                        ("Accept-Ranges", "bytes"),
                        ("Content-Range", &content_range),
                    ],
                    set_cookie,
                    body,
                )
            }
            PlaybackSelection::Full => {
                let Some(stream_permit) = self.stream_limiter.acquire() else {
                    return stream_overload_response(set_cookie);
                };
                let total_length = plan.total_length;
                let worker = self.clone();
                let body = streaming_body(stream_permit, move |output, is_cancelled| {
                    worker.write_playback_body(
                        &plan,
                        output,
                        is_cancelled,
                        PlaybackPacing::RealTime,
                    )
                });
                response_with_body(
                    200,
                    "audio/wav",
                    total_length,
                    &[("Cache-Control", "no-store"), ("Accept-Ranges", "bytes")],
                    set_cookie,
                    body,
                )
            }
        }
    }

    fn write_playback_body(
        &self,
        plan: &PlaybackPlan,
        output: &mut impl Write,
        is_cancelled: &(impl Fn() -> bool + ?Sized),
        pacing: PlaybackPacing,
    ) -> io::Result<()> {
        if is_cancelled() {
            return Ok(());
        }
        output.write_all(&audio_analysis::wav_header(plan.sample_count))?;

        let mut remaining = plan.sample_count;
        let mut cursor = plan.stream_start_sample;
        let stream_started = Instant::now();
        let stream_id = new_operation_id().unwrap_or_else(|_| format!("stream-{}", plan.version));
        let mut bytes_written = WAV_HEADER_BYTES;
        while remaining > 0 {
            let next_region_samples = remaining.min(PLAYBACK_CHUNK_SAMPLES);
            let generated_samples = cursor - plan.stream_start_sample + next_region_samples;
            if matches!(pacing, PlaybackPacing::RealTime)
                && !wait_for_playback_window(
                    generated_samples,
                    AUDIO_STREAM_LOOKAHEAD_SAMPLES,
                    audio_analysis::SAMPLE_RATE,
                    stream_started,
                    is_cancelled,
                )
            {
                eprintln!(
                    "audio_stream id={stream_id} outcome=cancelled version={} bytes_written={bytes_written}",
                    plan.version
                );
                return Ok(());
            }
            let end = cursor + next_region_samples;
            let region = match self.audio_renderer.stream_sample_range(
                &plan.project,
                cursor,
                end,
                is_cancelled,
            ) {
                Ok(rendered) => rendered,
                Err(AudioRenderError::Render(error)) => {
                    eprintln!(
                        "error: audio_stream id={stream_id} outcome=render_failed version={} bytes_written={bytes_written}: {error}",
                        plan.version
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
            "audio_stream id={stream_id} outcome=completed version={} bytes_written={bytes_written} elapsed_ms={}",
            plan.version,
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
        is_cancelled: &(impl Fn() -> bool + ?Sized),
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
        self.lock_project()
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
        let mut state = self.lock_project();
        let mut candidate = state.studio().clone();
        if !candidate.mark_operation_failed(operation_id, interrupted, message) {
            return;
        }
        if let Err(error) = state.commit_metadata(candidate) {
            eprintln!(
                "error: operation_id={} could not persist terminal partial-edit state: {error}",
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
        let deadline = edit.deadline;
        let completed = GeminiPlanner::interpret_with_updates(
            &self.gemini_session_root(),
            &edit.prompt,
            edit.start,
            edit.end,
            &edit.project,
            edit.new_prompt,
            deadline,
            cancellation,
            |request| self.render_gemini_audio(job_id, request, &render_cancellation, deadline),
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

    fn render_gemini_audio(
        &self,
        job_id: u64,
        request: crate::gemini_tools::AudioRenderRequest,
        cancellation: &AtomicBool,
        deadline: Instant,
    ) -> Result<crate::gemini_tools::AudioRender, String> {
        self.edit_jobs.set_running(
            job_id,
            "rendering",
            "Rendering the current sound graph with Surge XT",
        );
        let cancelled = || cancellation.load(Ordering::SeqCst) || Instant::now() >= deadline;
        let result = self
            .audio_renderer
            .render_with(AudioRenderPriority::Foreground, &cancelled, || {
                render_audio_request_cancellable(request, &cancelled)
            })
            .map_err(|error| match error {
                AudioRenderError::Render(error) => error,
                AudioRenderError::Cancelled if cancellation.load(Ordering::SeqCst) => {
                    "audio render interrupted".to_owned()
                }
                AudioRenderError::Cancelled => "audio render deadline expired".to_owned(),
            });
        self.edit_jobs.set_running(
            job_id,
            "planning",
            "Gemini is listening to the backend audio render",
        );
        result
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
                selection_start: edit.start,
                selection_end: edit.end,
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
        let mut state = self.lock_project();
        if state.project().version != *expected_version {
            return Err(PlannerError::ProjectChanged);
        }
        let mut candidate = state.studio().clone();
        candidate
            .replace_graph(
                graph_edit.project,
                graph_edit.selection_start,
                graph_edit.selection_end,
                &edit.prompt,
                graph_edit.plan,
            )
            .map_err(|error| PlannerError::InvalidOutput(studio_error_message(error).to_owned()))?;
        if !candidate.record_operation_step(&edit.operation_id, "Gemini", &summary) {
            return Err(PlannerError::InvalidOutput(
                "could not record the published edit operation".to_owned(),
            ));
        }
        state.commit(candidate).map_err(|error| {
            project_save_error(error);
            PlannerError::SaveFailed
        })?;
        *expected_version = state.project().version;
        *published_update = true;
        self.edit_jobs
            .publish_update(job_id, *expected_version, &summary);
        Ok(state.project().clone())
    }

    fn complete_gemini_operation(
        &self,
        job_id: u64,
        edit: &EditRequest,
        expected_version: &mut u64,
        message: &str,
    ) -> Result<(), PlannerError> {
        let mut state = self.lock_project();
        if self.edit_jobs.is_interrupted(job_id) {
            return Err(PlannerError::Interrupted);
        }
        if state.project().version != *expected_version {
            return Err(PlannerError::ProjectChanged);
        }
        let mut candidate = state.studio().clone();
        if !candidate.mark_operation_complete(&edit.operation_id, message) {
            return Err(PlannerError::InvalidOutput(
                "could not mark the completed edit operation".to_owned(),
            ));
        }
        state.commit_metadata(candidate).map_err(|error| {
            project_save_error(error);
            PlannerError::SaveFailed
        })?;
        *expected_version = state.project().version;
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
        let mut state = self.lock_project();
        let mut candidate = state.studio().clone();
        match candidate.set_duration(duration) {
            Ok(()) => match state.commit(candidate) {
                Ok(()) => self.project_response(&state),
                Err(error) => project_save_error(error),
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
        let mut state = self.lock_project();
        let mut candidate = state.studio().clone();
        match candidate.set_mix(track_id, None, Some(muted)) {
            Ok(()) => match state.commit(candidate) {
                Ok(()) => self.project_response(&state),
                Err(error) => project_save_error(error),
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
        let mut state = self.lock_project();
        let Some(index) = state.history().parent(state.history().current) else {
            return Response::json(409, error_json("nothing to undo"));
        };
        match state.checkout(index) {
            Ok(()) => self.project_response(&state),
            Err(error) => project_save_error(error),
        }
    }

    fn reset(&self) -> Response {
        let mut state = self.lock_project();
        let mut candidate = state.studio().clone();
        candidate.reset();
        match state.commit(candidate) {
            Ok(()) => self.project_response(&state),
            Err(error) => project_save_error(error),
        }
    }

    fn history_response(&self) -> Response {
        let state = self.lock_project();
        let history = state.history();
        let entries = history
            .entries()
            .enumerate()
            .map(|(index, entry)| {
                serde_json::json!({
                    "index":index,
                    "version":entry.version,
                    "summary":entry.summary,
                    "source":entry.source,
                    "prompt":entry.prompt,
                    "start":entry.start,
                    "end":entry.end
                })
            })
            .collect::<Vec<_>>();
        let current = history
            .entry(history.current)
            .expect("project history contains its current entry");
        Response::json(
            200,
            serde_json::json!({
                "current":history.current,
                "currentVersion":current.version,
                "currentEditCount":current.edit_count,
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
        let mut state = self.lock_project();
        if index >= state.history().len() {
            return Response::json(404, error_json("history state not found"));
        }
        match state.checkout(index) {
            Ok(()) => self.project_response(&state),
            Err(error) => project_save_error(error),
        }
    }

    fn project_response(&self, state: &ProjectState) -> Response {
        Response::json(200, state.project_json())
    }

    fn project_path(&self) -> PathBuf {
        self.lock_project()
            .path()
            .expect("production router has a project store")
            .to_owned()
    }

    fn spectrum_cache_path(&self) -> Option<PathBuf> {
        let state = self.lock_project();
        let project_path = state.path()?;
        let parent = project_path.parent()?;
        let project_name = project_path.file_name()?.to_string_lossy();
        // One replaceable window bounds disk usage across arbitrary seek positions.
        Some(parent.join(format!(".{project_name}.track-spectrum.cache")))
    }

    fn gemini_session_root(&self) -> PathBuf {
        if let Some(path) = self.lock_project().path() {
            return session_root_for_project(path);
        }
        #[cfg(test)]
        return session_root();
        #[cfg(not(test))]
        unreachable!("production routers always have project storage")
    }

    fn lock_project(&self) -> std::sync::MutexGuard<'_, ProjectState> {
        self.project.lock().recover_poison()
    }
}

fn discard_obsolete_spectrum_caches(project_path: &std::path::Path) {
    let parent = project_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let Some(project_name) = project_path.file_name() else {
        return;
    };
    let prefix = format!(".{}.track-spectrum-", project_name.to_string_lossy());
    let cleanup = || -> io::Result<()> {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some((start, window)) = name
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|name| name.strip_suffix(".cache"))
                .and_then(|range| range.split_once('-'))
            else {
                continue;
            };
            if [start, window]
                .iter()
                .all(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
                && entry.file_type()?.is_file()
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    };
    if let Err(error) = cleanup() {
        eprintln!("warning: could not discard obsolete track spectrum caches: {error}");
    }
}

fn recovered_operation_json(operation: &crate::model::EditOperation) -> String {
    let mut response = serde_json::json!({
        "id": "recovered",
        "operationId": operation.operation_id,
        "elapsedSeconds": 0,
        "timeoutSeconds": EDIT_TIMEOUT_SECONDS,
        "appliedSteps": operation.applied_steps,
        "initialVersion": operation.initial_version,
        "projectVersion": operation.project_version
    });
    if operation.status == crate::model::EditOperationStatus::Completed {
        response["status"] = "completed".into();
        response["phase"] = "completed".into();
        response["message"] = operation.message.clone().into();
    } else {
        let recovered_status = match operation.status {
            crate::model::EditOperationStatus::Interrupted => "interrupted_with_changes",
            crate::model::EditOperationStatus::Running
            | crate::model::EditOperationStatus::Failed => "failed_with_changes",
            crate::model::EditOperationStatus::Completed => unreachable!(),
        };
        response["status"] = recovered_status.into();
        response["phase"] = "failed".into();
        response["errorStatus"] = 500.into();
        response["error"] = operation.message.clone().into();
    }
    response.to_string()
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

fn spectrum_levels(samples: &[f32], center_frame: usize) -> [u8; SPECTRUM_BANDS] {
    let channel_fft = |channel: usize| {
        let start = center_frame as isize - (SPECTRUM_FFT_SAMPLES / 2) as isize;
        crate::spectrum::magnitudes_1024((0..SPECTRUM_FFT_SAMPLES).map(|index| {
            let sample = start + index as isize;
            if sample < 0 {
                return 0.0;
            }
            samples
                .get(sample as usize * audio_analysis::CHANNEL_COUNT + channel)
                .copied()
                .unwrap_or(0.0) as f64
        }))
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
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

fn parse_optional_boolean(
    form: &HashMap<String, String>,
    name: &str,
) -> Result<bool, &'static str> {
    match form.get(name).map(String::as_str) {
        None => Ok(false),
        Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err("boolean setting must be true or false"),
    }
}

fn project_save_error(error: io::Error) -> Response {
    eprintln!("error: could not save project state: {error}");
    Response::json(500, error_json("could not save project history"))
}

fn error_json(message: &str) -> String {
    serde_json::json!({"error":message}).to_string()
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
        StudioError::ClipCapacity => "MIDI clip limit reached",
    }
}

#[cfg(test)]
mod tests;
