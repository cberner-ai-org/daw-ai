use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use serde_json::Map;
use serde_json::Value as JsonValue;

#[cfg(test)]
use crate::gemini_tools::render_audio_request;
use crate::gemini_tools::{
    AUDIO_TOOL_NAME, AudioRender, AudioRenderRequest, EditSession, INSTRUMENT_PARAMETER_TOOL_NAME,
    PRESET_TOOL_NAME, READ_TOOL_NAME, apply_agent_mutation, base64_audio, is_mutation_tool,
    list_instrument_parameters, list_surge_presets, prepare_audio_render, read_sound_graph,
    tool_declarations,
};
use crate::model::Project;
#[cfg(test)]
use crate::model::TrackRole;
use crate::prompt::{Action, EditPlan};
#[cfg(test)]
use crate::prompt::{AutomationPoint, MAX_COMPOUND_ACTIONS, MidiNote};

const STUDIO_CONTRACT: &str = include_str!("../gemini/STUDIO.md");
pub(crate) const GEMINI_MODEL: &str = "gemini-3.6-flash";
const DEFAULT_INTERACTIONS_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/interactions";
const SYSTEMD_CREDENTIAL_NAME: &str = "gemini-api-key";
pub(crate) const EDIT_TIMEOUT_SECONDS: u64 = 20 * 60;
const EDIT_TIMEOUT: Duration = Duration::from_secs(EDIT_TIMEOUT_SECONDS);
const TRANSIENT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
#[cfg(test)]
type Object = Map<String, JsonValue>;

#[derive(Debug)]
pub enum PlannerError {
    Unavailable(String),
    TimedOut,
    Failed {
        message: String,
        code: Option<String>,
    },
    ProjectChanged,
    SaveFailed,
    Interrupted,
    InvalidOutput(String),
    Io(std::io::Error),
}

impl fmt::Display for PlannerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "{message}"),
            Self::TimedOut => write!(
                formatter,
                "Gemini took too long to complete the edit; try again"
            ),
            Self::Failed { message, .. } => {
                write!(formatter, "Gemini could not complete the edit: {message}")
            }
            Self::ProjectChanged => write!(formatter, "the project changed; submit the edit again"),
            Self::SaveFailed => write!(formatter, "could not save the sound graph"),
            Self::Interrupted => write!(formatter, "the edit was interrupted"),
            Self::InvalidOutput(message) => {
                write!(
                    formatter,
                    "Gemini returned an invalid synth edit: {message}"
                )
            }
            Self::Io(error) => write!(formatter, "Gemini integration failed: {error}"),
        }
    }
}

pub struct GeminiPlanner;

pub struct GeminiEdit {
    pub plan: EditPlan,
    pub project: Project,
}

#[derive(Default)]
struct LoopState {
    plans: Vec<EditPlan>,
    audio_listens: usize,
    audio_artifacts: usize,
}

impl GeminiPlanner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn interpret_with_audio_renderer_updates(
        session_root: &std::path::Path,
        prompt: &str,
        start: f32,
        end: f32,
        project: &Project,
        cancellation: Arc<AtomicBool>,
        mut render_audio: impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
        mut on_update: impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
    ) -> Result<GeminiEdit, PlannerError> {
        let session = EditSession::create_in(session_root, project, prompt, start, end)
            .map_err(PlannerError::Io)?;
        let result = run_session(
            &session,
            prompt,
            start,
            end,
            cancellation,
            &mut render_audio,
            &mut on_update,
        );
        let (status, detail) = match &result {
            Ok(edit) => ("completed", edit.plan.summary.clone()),
            Err(error) => ("failed", error.to_string()),
        };
        let (applied_steps, audio_listens) = session.stats().unwrap_or((0, 0));
        // Keep the model/API transcript even if this final metadata update cannot be written.
        if let Err(error) = session.update_status(status, &detail, applied_steps, audio_listens) {
            eprintln!("warning: could not finalize Gemini session metadata: {error}");
        }
        if let Err(error) = crate::gemini_tools::apply_session_retention(session_root) {
            eprintln!("warning: could not apply Gemini session retention: {error}");
        }
        result
    }
}

fn run_session(
    session: &EditSession,
    prompt: &str,
    start: f32,
    end: f32,
    cancellation: Arc<AtomicBool>,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
) -> Result<GeminiEdit, PlannerError> {
    let api_key = load_api_key()?;
    let endpoint = std::env::var("DAW_AI_GEMINI_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_INTERACTIONS_ENDPOINT.to_owned());
    run_session_with_transport(
        session,
        prompt,
        start,
        end,
        render_audio,
        on_update,
        &|| cancellation.load(Ordering::SeqCst),
        &mut |sequence, request, remaining| {
            call_interactions_with_retry(
                session,
                sequence,
                request,
                &api_key,
                &endpoint,
                remaining,
                &cancellation,
                &TRANSIENT_RETRY_DELAYS,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn run_session_with_transport(
    session: &EditSession,
    prompt: &str,
    start: f32,
    end: f32,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
    is_cancelled: &impl Fn() -> bool,
    transport: &mut impl FnMut(usize, &JsonValue, Duration) -> Result<String, PlannerError>,
) -> Result<GeminiEdit, PlannerError> {
    let started = Instant::now();
    let tools = tool_declarations();
    let research_tools = [serde_json::json!({"type": "google_search"})];
    let mut input = JsonValue::String(research_task(prompt));
    let mut previous_interaction_id: Option<String> = None;
    let mut sequence = 0_usize;
    let mut research_complete = false;
    let mut state = LoopState::default();

    loop {
        if is_cancelled() {
            return Err(PlannerError::Interrupted);
        }
        let remaining = EDIT_TIMEOUT
            .checked_sub(started.elapsed())
            .ok_or(PlannerError::TimedOut)?;
        sequence += 1;
        let mut request = serde_json::json!({
            "model": GEMINI_MODEL,
            "input": input,
            "tools": if research_complete { &tools[..] } else { &research_tools[..] },
            "system_instruction": system_instruction(),
            "generation_config": {"thinking_level": "high"},
            "store": true
        });
        if let Some(previous) = &previous_interaction_id {
            request
                .as_object_mut()
                .expect("interaction request object")
                .insert(
                    "previous_interaction_id".to_owned(),
                    JsonValue::String(previous.clone()),
                );
        }
        let response_source = transport(sequence, &request, remaining)?;
        let response = serde_json::from_str::<JsonValue>(&response_source)
            .map_err(|error| invalid(&format!("interaction response was not JSON: {error}")))?;
        previous_interaction_id = Some(
            response
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| invalid("interaction response omitted its ID"))?
                .to_owned(),
        );
        let calls = function_calls(&response)?;
        if !research_complete {
            research_complete = true;
            input = JsonValue::String(planner_task(prompt, start, end));
            continue;
        }
        if calls.is_empty() {
            if state.plans.is_empty() {
                input = JsonValue::String(format!(
                    "You have not made an edit. Call {READ_TOOL_NAME}, then use a concrete CRUD graph mutation such as new_track, add_midi_clip, or set_parameter. {AUDIO_TOOL_NAME} is available whenever listening would help you decide."
                ));
                continue;
            }
            let (plan, project) = session
                .finish(state.plans)
                .map_err(|message| invalid(&message))?;
            return Ok(GeminiEdit { plan, project });
        }

        let mut results = Vec::with_capacity(calls.len() * 2);
        for call in calls {
            if is_cancelled() {
                return Err(PlannerError::Interrupted);
            }
            let output = execute_tool(
                session,
                sequence,
                &call,
                &mut state,
                render_audio,
                on_update,
            )?;
            results.push(serde_json::json!({
                "type": "function_result",
                "name": call.name,
                "call_id": call.id,
                "result": output.result
            }));
            results.extend(output.supplemental_input);
        }
        session
            .update_status(
                "running",
                "Gemini is editing and listening",
                applied_steps(&state),
                state.audio_listens,
            )
            .map_err(PlannerError::Io)?;
        input = JsonValue::Array(results);
    }
}

struct FunctionCall {
    id: String,
    name: String,
    arguments: JsonValue,
}

struct ToolOutput {
    result: Vec<JsonValue>,
    supplemental_input: Vec<JsonValue>,
}

impl ToolOutput {
    fn text(message: String) -> Self {
        Self {
            result: vec![serde_json::json!({"type": "text", "text": message})],
            supplemental_input: Vec::new(),
        }
    }
}

fn function_calls(response: &JsonValue) -> Result<Vec<FunctionCall>, PlannerError> {
    if let Some(error) = response.get("error") {
        return Err(api_failure(error));
    }
    let status = response
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("completed");
    if matches!(
        status,
        "failed" | "cancelled" | "incomplete" | "budget_exceeded"
    ) {
        return Err(PlannerError::Failed {
            message: format!("interaction ended with status {status}"),
            code: None,
        });
    }
    let steps = response
        .get("steps")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("interaction response omitted its steps"))?;
    steps
        .iter()
        .filter(|step| step.get("type").and_then(JsonValue::as_str) == Some("function_call"))
        .map(|step| {
            Ok(FunctionCall {
                id: step
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| invalid("function call omitted its ID"))?
                    .to_owned(),
                name: step
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| invalid("function call omitted its name"))?
                    .to_owned(),
                arguments: step
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect()
}

fn execute_tool(
    session: &EditSession,
    sequence: usize,
    call: &FunctionCall,
    state: &mut LoopState,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
) -> Result<ToolOutput, PlannerError> {
    match call.name.as_str() {
        READ_TOOL_NAME => Ok(ToolOutput::text(match read_sound_graph(session.path()) {
            Ok(graph) => graph,
            Err(error) => format!("Tool error: {error}"),
        })),
        PRESET_TOOL_NAME => Ok(ToolOutput::text(
            list_surge_presets(&call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        INSTRUMENT_PARAMETER_TOOL_NAME => Ok(ToolOutput::text(
            list_instrument_parameters(session.path(), &call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        "resample_audio_region" => {
            let object = call
                .arguments
                .as_object()
                .ok_or_else(|| invalid("resample arguments must be an object"))?;
            let render_arguments = serde_json::json!({
                "tracks": object.get("sourceTracks").cloned().unwrap_or(JsonValue::String("all".to_owned())),
                "start": object.get("sourceStart").cloned().unwrap_or(JsonValue::Null),
                "end": object.get("sourceEnd").cloned().unwrap_or(JsonValue::Null)
            });
            let output = match prepare_audio_render(session.path(), &render_arguments)
                .and_then(render_audio)
            {
                Ok(audio) => {
                    state.audio_artifacts += 1;
                    let name = session
                        .record_audio(sequence * 1_000_000 + state.audio_artifacts, &audio.wav)
                        .map_err(PlannerError::Io)?;
                    let mut arguments = call.arguments.clone();
                    let arguments_object = arguments
                        .as_object_mut()
                        .expect("validated resample arguments object");
                    arguments_object.insert(
                        "asset".to_owned(),
                        JsonValue::String(
                            session.path().join(&name).to_string_lossy().into_owned(),
                        ),
                    );
                    let duration = object
                        .get("sourceEnd")
                        .and_then(JsonValue::as_f64)
                        .zip(object.get("sourceStart").and_then(JsonValue::as_f64))
                        .map(|(end, start)| end - start)
                        .ok_or_else(|| invalid("resample source times must be numbers"))?;
                    arguments_object
                        .insert("sourceDuration".to_owned(), serde_json::json!(duration));
                    apply_and_commit_mutation(
                        session,
                        &arguments,
                        "resample_audio_region",
                        state,
                        on_update,
                    )?
                }
                Err(error) => format!("Tool error: {error}"),
            };
            Ok(ToolOutput::text(output))
        }
        name if is_mutation_tool(name) => Ok(ToolOutput::text(apply_and_commit_mutation(
            session,
            &call.arguments,
            name,
            state,
            on_update,
        )?)),
        AUDIO_TOOL_NAME => {
            match prepare_audio_render(session.path(), &call.arguments).and_then(render_audio) {
                Ok(audio) => {
                    state.audio_listens += 1;
                    state.audio_artifacts += 1;
                    let audio_name = session
                        .record_audio(sequence * 1_000_000 + state.audio_artifacts, &audio.wav)
                        .map_err(PlannerError::Io)?;
                    let description = format!(
                        "{} Objective measurements: {} Session artifact: {audio_name}.",
                        audio.description, audio.measurements
                    );
                    let output = ToolOutput {
                        result: vec![serde_json::json!({
                            "type": "text",
                            "text": description.clone()
                        })],
                        supplemental_input: vec![serde_json::json!({
                            "type": "user_input",
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!(
                                        "Audio produced by {AUDIO_TOOL_NAME} for function call {}. Listen to this WAV before deciding what to do next.",
                                        call.id
                                    )
                                },
                                {
                                    "type": "audio",
                                    "mime_type": "audio/wav",
                                    "data": base64_audio(&audio.wav)
                                }
                            ]
                        })],
                    };
                    Ok(output)
                }
                Err(error) => Ok(ToolOutput::text(format!("Tool error: {error}"))),
            }
        }
        _ => Ok(ToolOutput::text(format!(
            "Tool error: unknown tool {}",
            call.name
        ))),
    }
}

fn apply_and_commit_mutation(
    session: &EditSession,
    arguments: &JsonValue,
    name: &str,
    state: &mut LoopState,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
) -> Result<String, PlannerError> {
    match apply_agent_mutation(session.path(), name, arguments) {
        Ok(message) => {
            let (plan, project) = session
                .take_update()
                .map_err(|message| invalid(&message))?
                .ok_or_else(|| invalid("mutation tool did not publish its graph update"))?;
            let committed = on_update(GeminiEdit {
                plan: plan.clone(),
                project,
            })?;
            session
                .synchronize_project(&committed)
                .map_err(|message| invalid(&message))?;
            state.plans.push(plan);
            Ok(message)
        }
        Err(error) => Ok(format!("Tool error: {error}")),
    }
}

fn call_interactions(
    session: &EditSession,
    exchange_name: &str,
    request: &JsonValue,
    api_key: &str,
    endpoint: &str,
    remaining: Duration,
    cancellation: &Arc<AtomicBool>,
) -> Result<String, PlannerError> {
    let request_path = session
        .path()
        .join(format!(".{exchange_name}-pending.json"));
    fs::write(&request_path, request.to_string()).map_err(PlannerError::Io)?;
    let max_time = remaining.as_secs().max(1).to_string();
    let mut command = Command::new("curl");
    let response_path = session
        .path()
        .join(format!(".{exchange_name}-response-pending"));
    let error_path = session
        .path()
        .join(format!(".{exchange_name}-error-pending"));
    let response_file = fs::File::create(&response_path).map_err(PlannerError::Io)?;
    let error_file = fs::File::create(&error_path).map_err(PlannerError::Io)?;
    command
        .arg("--silent")
        .arg("--show-error")
        .arg("--fail-with-body")
        .arg("--connect-timeout")
        .arg("15")
        .arg("--max-time")
        .arg(max_time)
        .arg("--request")
        .arg("POST")
        .arg("--header")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg(format!("@{}", request_path.display()))
        .arg("--config")
        .arg("-")
        .arg(endpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(response_file))
        .stderr(Stdio::from(error_file));
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PlannerError::Unavailable(
                "curl is required for the Gemini API connection; install curl and try again"
                    .to_owned(),
            )
        } else {
            PlannerError::Io(error)
        }
    })?;
    let mut stdin = child.stdin.take().expect("piped curl stdin");
    writeln!(stdin, "header = \"x-goog-api-key: {api_key}\"").map_err(PlannerError::Io)?;
    drop(stdin);
    let status = loop {
        if cancellation.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&request_path);
            let _ = fs::remove_file(&response_path);
            let _ = fs::remove_file(&error_path);
            return Err(PlannerError::Interrupted);
        }
        if let Some(status) = child.try_wait().map_err(PlannerError::Io)? {
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let _ = fs::remove_file(&request_path);
    let response = fs::read_to_string(&response_path).map_err(PlannerError::Io)?;
    let stderr = fs::read_to_string(&error_path).map_err(PlannerError::Io)?;
    let _ = fs::remove_file(&response_path);
    let _ = fs::remove_file(&error_path);
    session
        .record_exchange(exchange_name, request, &response)
        .map_err(PlannerError::Io)?;
    if !status.success() {
        if status.code() == Some(28) {
            return Err(PlannerError::TimedOut);
        }
        if let Some(error) = serde_json::from_str::<JsonValue>(&response)
            .ok()
            .and_then(|body| body.get("error").cloned())
        {
            return Err(api_failure(&error));
        }
        return Err(PlannerError::Failed {
            message: bounded_text(&stderr, 1_000),
            code: None,
        });
    }
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
fn call_interactions_with_retry(
    session: &EditSession,
    sequence: usize,
    request: &JsonValue,
    api_key: &str,
    endpoint: &str,
    remaining: Duration,
    cancellation: &Arc<AtomicBool>,
    retry_delays: &[Duration],
) -> Result<String, PlannerError> {
    retry_transient_interaction(
        sequence,
        remaining,
        cancellation,
        retry_delays,
        &mut |exchange_name, available| {
            call_interactions(
                session,
                exchange_name,
                request,
                api_key,
                endpoint,
                available,
                cancellation,
            )
        },
    )
}

fn retry_transient_interaction(
    sequence: usize,
    remaining: Duration,
    cancellation: &Arc<AtomicBool>,
    retry_delays: &[Duration],
    transport: &mut impl FnMut(&str, Duration) -> Result<String, PlannerError>,
) -> Result<String, PlannerError> {
    let started = Instant::now();
    for attempt in 0..=retry_delays.len() {
        let exchange_name = if attempt == 0 {
            format!("interaction-{sequence:03}")
        } else {
            format!("interaction-{sequence:03}-retry-{attempt}")
        };
        let available = remaining
            .checked_sub(started.elapsed())
            .ok_or(PlannerError::TimedOut)?;
        let response = transport(&exchange_name, available);
        let retry = match &response {
            Ok(body) => transient_api_error(body),
            Err(PlannerError::Failed { message, code }) => {
                code.as_deref() == Some("service_unavailable") || transient_api_message(message)
            }
            _ => false,
        };
        if !retry || attempt == retry_delays.len() {
            return response;
        }
        wait_for_retry(retry_delays[attempt], remaining, started, cancellation)?;
    }
    unreachable!("retry loop always returns")
}

fn transient_api_error(source: &str) -> bool {
    serde_json::from_str::<JsonValue>(source)
        .ok()
        .and_then(|body| {
            body.get("error")
                .and_then(|error| error.get("code"))
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|code| code == "service_unavailable")
}

fn transient_api_message(message: &str) -> bool {
    message.eq_ignore_ascii_case("the service is currently unavailable.")
        || message.eq_ignore_ascii_case("service unavailable")
}

fn wait_for_retry(
    delay: Duration,
    remaining: Duration,
    started: Instant,
    cancellation: &Arc<AtomicBool>,
) -> Result<(), PlannerError> {
    let deadline = Instant::now() + delay;
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return Err(PlannerError::Interrupted);
        }
        if started.elapsed() >= remaining {
            return Err(PlannerError::TimedOut);
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(25));
        if wait.is_zero() {
            return Ok(());
        }
        thread::sleep(wait);
    }
}

fn load_api_key() -> Result<String, PlannerError> {
    for name in ["DAW_AI_GEMINI_API_KEY", "GEMINI_API_KEY"] {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            return validate_api_key(&value.to_string_lossy());
        }
    }
    let path = credential_path(
        std::env::var_os("DAW_AI_GEMINI_CREDENTIALS").map(PathBuf::from),
        std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
    .ok_or_else(|| missing_credentials(None))?;
    let source = fs::read_to_string(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing_credentials(Some(&path))
        } else {
            PlannerError::Io(error)
        }
    })?;
    let lines = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let candidate = lines
        .iter()
        .find_map(|line| labeled_api_key(line))
        .or_else(|| {
            lines.iter().find_map(|line| {
                (!line.contains(['=', ':']) && !line.contains(char::is_whitespace)).then_some(*line)
            })
        })
        .unwrap_or_default()
        .trim()
        .trim_matches(['\'', '"']);
    validate_api_key(candidate)
}

fn credential_path(
    explicit: Option<PathBuf>,
    systemd_directory: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    explicit
        .or_else(|| systemd_directory.map(|path| path.join(SYSTEMD_CREDENTIAL_NAME)))
        .or_else(|| home.map(|path| path.join("gemini_creds.txt")))
}

fn labeled_api_key(line: &str) -> Option<&str> {
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let (label, value) = line.split_once('=').or_else(|| line.split_once(':'))?;
    let label = label.trim().to_ascii_lowercase();
    (label.contains("key") || label == "token").then_some(value.trim())
}

fn validate_api_key(value: &str) -> Result<String, PlannerError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '\\'))
    {
        return Err(PlannerError::Unavailable(
            "the Gemini API key is empty or malformed".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn missing_credentials(path: Option<&Path>) -> PlannerError {
    let location = path.map_or_else(
        || "~/gemini_creds.txt".to_owned(),
        |path| path.display().to_string(),
    );
    PlannerError::Unavailable(format!(
        "Gemini API credentials are required; set GEMINI_API_KEY, load the {SYSTEMD_CREDENTIAL_NAME} systemd credential, or put the key in {location}"
    ))
}

fn planner_task(prompt: &str, start: f32, end: f32) -> String {
    format!(
        "Selected edit region: {start:.3} to {end:.3} seconds. This bounds graph edits, not listening.\nUser request: {prompt}\n\nBegin by reading the current sound graph. For creative work, listen after each change, compare the sound with the user's request, and iterate on composition and sound design until they match. Establish an audible baseline, audition important sound choices on isolated tracks, and evaluate the final full mix."
    )
}

fn research_task(prompt: &str) -> String {
    format!(
        "Use Google Search to research how producers create the requested musical effect or style and what listeners perceive as its signature. Focus on arrangement, tension and release, rhythm, orchestration, and sound design. Return concise findings for a subsequent DAW editing turn; do not attempt to edit the graph yet. User request: {prompt}"
    )
}

fn system_instruction() -> String {
    format!(
        concat!(
            "You are the autonomous sound-graph producer inside DAW-AI. Use the registered tools; ",
            "you cannot alter the graph by merely describing changes. Research unfamiliar musical ",
            "goals when useful. The selected region bounds edits only; every audio-tool call chooses ",
            "its own absolute project start and end, so include surrounding context when useful. Read ",
            "the graph before editing. For creative or style-based work, listen after each change, ",
            "compare the audible result with the user's request, and iterate on composition and sound ",
            "design until they match. Listen before editing, audition important preset or effect choices ",
            "on isolated tracks, and evaluate the final full mix. ",
            "When you listen, use the WAV and objective measurements and reason from ",
            "the actual audio - not event-count proxies - about groove, beat subdivision, energy contour, ",
            "tension, impact, timbre, and contrast. If a style depends on intensification, express it ",
            "through composition and rhythmic subdivision when appropriate. Default drums, bass grooves, ",
            "chord accompaniment, arpeggios, and repeated riffs to musical beat loops; reserve one-shot ",
            "MIDI phrases mainly for melody and genuinely non-repeating fills or transitions. Do not assume the project ",
            "tempo must change. Continue until the result fulfills the request, then finish. There is no ",
            "separate completion reviewer. There is no ",
            "predetermined tool-call or iteration limit; the request timeout is the only loop limit.\n\n{}"
        ),
        STUDIO_CONTRACT
    )
}

fn api_error_message(error: &JsonValue) -> String {
    error
        .get("message")
        .and_then(JsonValue::as_str)
        .unwrap_or("the Gemini API returned an error")
        .to_owned()
}

fn api_failure(error: &JsonValue) -> PlannerError {
    PlannerError::Failed {
        message: api_error_message(error),
        code: error
            .get("code")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
    }
}

fn bounded_text(value: &str, maximum: usize) -> String {
    value
        .trim()
        .chars()
        .take(maximum)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn edit_count(action: &Action) -> usize {
    match action {
        Action::Compound { actions } => actions.iter().map(edit_count).sum(),
        _ => 1,
    }
}

fn applied_steps(state: &LoopState) -> usize {
    state
        .plans
        .iter()
        .map(|plan| edit_count(&plan.action))
        .sum()
}

#[cfg(test)]
pub(crate) fn plan_from_json(source: &str) -> Result<EditPlan, PlannerError> {
    let value: JsonValue =
        serde_json::from_str(source).map_err(|error| invalid(&error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("top-level response must be an object"))?;
    let summary = concise_model_text(object, "summary", 160)?;
    let _musical_plan = concise_model_text(object, "musicalPlan", 300)?;
    let actions = object
        .get("actions")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("actions must be an array"))?;
    if actions.is_empty() || actions.len() > MAX_COMPOUND_ACTIONS {
        return Err(invalid("one to nine actions are required"));
    }
    let mut parsed = actions
        .iter()
        .map(action_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let action = if parsed.len() == 1 {
        parsed.pop().expect("one parsed action")
    } else {
        Action::Compound { actions: parsed }
    };
    Ok(EditPlan { action, summary })
}

#[cfg(test)]
fn action_from_json(value: &JsonValue) -> Result<Action, PlannerError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("each action must be an object"))?;
    let kind = string_field(object, "kind")?;
    let target_name = string_field(object, "target")?;
    let target = role_from_name(target_name)?;
    let name = string_field(object, "name")?;
    let value = number_field(object, "value")?;
    let action = match kind {
        "gain" if (0.0..=2.0).contains(&value) => Ok(Action::Gain {
            amount: value as f32,
            target,
        }),
        "mute" => Ok(Action::Mute { target }),
        "midi-clip" if name == "MIDI Clip" => {
            let target = target.ok_or_else(|| invalid("midi-clip requires a role target"))?;
            let label = string_field(object, "setting")?.trim();
            let start = number_field(object, "start")?;
            let end = number_field(object, "end")?;
            if label.is_empty()
                || label.chars().count() > 64
                || !(0.0..1.0).contains(&start)
                || !(0.0..=1.0).contains(&end)
                || end <= start
                || !(0.25..=16.0).contains(&value)
            {
                return Err(invalid("midi-clip fields are invalid"));
            }
            return Ok(Action::MidiClip {
                track_id: integer_field(object, "trackId")?,
                target,
                label: label.to_owned(),
                start: start as f32,
                end: end as f32,
                loop_beats: value as f32,
                notes: midi_notes_field(object, value)?,
            });
        }
        "add-track" => target
            .map(|role| Action::AddTrack { role })
            .ok_or_else(|| invalid("add-track requires a role target")),
        "instrument" if value == 0.0 => Ok(Action::Instrument {
            preset: surge_preset_name(name)?,
            target: target.ok_or_else(|| invalid("instrument requires a role target"))?,
        }),
        "modulator" if (0.0..=1.0).contains(&value) => Ok(Action::Modulator {
            parameter: modulator_parameter(name)?,
            shape: modulator_shape(string_field(object, "setting")?)?,
            rate: modulator_rate(number_field(object, "rate")?)?,
            depth: value as f32,
            target: target.ok_or_else(|| invalid("modulator requires a role target"))?,
        }),
        "configure" => {
            if name != "None" {
                return Err(invalid("configure name must be None"));
            }
            let clip_id = integer_field(object, "clipId")?;
            let parameter = sound_parameter_name(string_field(object, "parameter")?)?;
            let setting = configuration_value(object, parameter, value)?;
            Ok(Action::Configure {
                track_id: integer_field(object, "trackId")?,
                target: target.ok_or_else(|| invalid("configure requires a role target"))?,
                tool: sound_tool_name(string_field(object, "tool")?)?,
                tool_id: integer_field(object, "toolId")?,
                clip_id: (clip_id != 0).then_some(clip_id),
                parameter,
                value: setting,
            })
        }
        "automation" if value == 0.0 => Ok(Action::Automation {
            track_id: nonzero_integer_field(object, "trackId")?,
            parameter: automation_parameter(name)?,
            curve: automation_curve(string_field(object, "setting")?)?,
            points: automation_points_field(object)?,
            target: target.ok_or_else(|| invalid("automation requires a role target"))?,
        }),
        "effect" if (0.0..=1.0).contains(&value) => Ok(Action::Effect {
            name: effect_name(name, false)?,
            mix: value as f32,
            target,
        }),
        "remove-effect" => Ok(Action::RemoveEffect {
            name: effect_name(name, true)?,
            target,
        }),
        "filter" if (-1.0..=1.0).contains(&value) => Ok(Action::Filter {
            amount: value as f32,
            target,
        }),
        "rhythm" if (-1.0..=1.0).contains(&value) => Ok(Action::Rhythm {
            amount: value as f32,
            target,
        }),
        "tempo" if target.is_none() && value.fract() == 0.0 && (60.0..=180.0).contains(&value) => {
            Ok(Action::Tempo { bpm: value as u16 })
        }
        _ => Err(invalid("action fields are inconsistent or out of range")),
    }?;
    let start = object
        .get("start")
        .map(|_| number_field(object, "start"))
        .transpose()?
        .unwrap_or(0.0);
    let end = object
        .get("end")
        .map(|_| number_field(object, "end"))
        .transpose()?
        .unwrap_or(1.0);
    if !(0.0..1.0).contains(&start) || !(0.0..=1.0).contains(&end) || end <= start {
        return Err(invalid("action timing fields are invalid"));
    }
    if start == 0.0 && end == 1.0 {
        return Ok(action);
    }
    if !matches!(
        action,
        Action::Gain { .. }
            | Action::Mute { .. }
            | Action::AddTrack { .. }
            | Action::Automation { .. }
            | Action::Effect { .. }
            | Action::RemoveEffect { .. }
            | Action::Filter { .. }
            | Action::Rhythm { .. }
    ) {
        return Err(invalid(
            "partial timing is supported for arrangement, mix, effect, and automation actions",
        ));
    }
    Ok(Action::Timed {
        start: start as f32,
        end: end as f32,
        action: Box::new(action),
    })
}

#[cfg(test)]
fn automation_points_field(object: &Object) -> Result<Vec<AutomationPoint>, PlannerError> {
    let points = object
        .get("points")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("automation points must be an array"))?;
    if !(2..=16).contains(&points.len()) {
        return Err(invalid("automation requires between 2 and 16 points"));
    }
    let points = points
        .iter()
        .map(|point| {
            let point = point
                .as_object()
                .ok_or_else(|| invalid("each automation point must be an object"))?;
            Ok(AutomationPoint {
                time: number_field(point, "time")? as f32,
                value: number_field(point, "value")? as f32,
            })
        })
        .collect::<Result<Vec<_>, PlannerError>>()?;
    if points.first().map(|point| point.time) != Some(0.0)
        || points.last().map(|point| point.time) != Some(1.0)
        || points
            .iter()
            .any(|point| !(0.0..=1.0).contains(&point.time))
        || points
            .windows(2)
            .any(|points| points[1].time <= points[0].time)
    {
        return Err(invalid(
            "automation point times must increase from 0 through 1",
        ));
    }
    Ok(points)
}

#[cfg(test)]
fn midi_notes_field(object: &Object, loop_beats: f64) -> Result<Vec<MidiNote>, PlannerError> {
    let events = object
        .get("events")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("midi-clip events must be an array"))?;
    if events.len() > 32 {
        return Err(invalid("midi-clip supports up to 32 notes"));
    }
    events
        .iter()
        .map(|event| {
            let event = event
                .as_object()
                .ok_or_else(|| invalid("each MIDI note must be an object"))?;
            let time = number_field(event, "time")?;
            let duration = number_field(event, "duration")?;
            let pitch = integer_field(event, "pitch")?;
            let velocity = number_field(event, "velocity")?;
            if !(0.0..loop_beats).contains(&time)
                || !(0.0625..=loop_beats).contains(&duration)
                || pitch > 127
                || !(0.01..=1.0).contains(&velocity)
            {
                return Err(invalid("MIDI note fields are out of range"));
            }
            Ok(MidiNote {
                time: time as f32,
                duration: duration as f32,
                pitch: pitch as u8,
                velocity: velocity as f32,
            })
        })
        .collect()
}

#[cfg(test)]
fn role_from_name(name: &str) -> Result<Option<TrackRole>, PlannerError> {
    match name {
        "all" => Ok(None),
        "drums" => Ok(Some(TrackRole::Drums)),
        "bass" => Ok(Some(TrackRole::Bass)),
        "chords" => Ok(Some(TrackRole::Chords)),
        "lead" => Ok(Some(TrackRole::Lead)),
        "texture" => Ok(Some(TrackRole::Texture)),
        _ => Err(invalid("unknown action target")),
    }
}

#[cfg(test)]
fn effect_name(name: &str, allow_all: bool) -> Result<&'static str, PlannerError> {
    match name {
        "Reverb" => Ok("Reverb"),
        "Room" => Ok("Room"),
        "Echo" => Ok("Echo"),
        "Chorus" => Ok("Chorus"),
        "Low-pass filter" => Ok("Low-pass filter"),
        "Punch compressor" => Ok("Punch compressor"),
        "Drive" => Ok("Drive"),
        "Shimmer" => Ok("Shimmer"),
        "Effects" if allow_all => Ok("Effects"),
        _ => Err(invalid("unknown effect name")),
    }
}

#[cfg(test)]
fn surge_preset_name(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "Init" => Ok("Init"),
        "Surge Kick" => Ok("Surge Kick"),
        "Surge Snare" => Ok("Surge Snare"),
        "Surge Closed Hat" => Ok("Surge Closed Hat"),
        "Surge Open Hat" => Ok("Surge Open Hat"),
        "Surge Crash" => Ok("Surge Crash"),
        "Surge Percussion" => Ok("Surge Percussion"),
        "Surge Bass" => Ok("Surge Bass"),
        "Surge Pad" => Ok("Surge Pad"),
        "Surge Lead" => Ok("Surge Lead"),
        "Surge Atmosphere" => Ok("Surge Atmosphere"),
        _ => Err(invalid("unknown Surge XT starter patch")),
    }
}

#[cfg(test)]
fn modulator_parameter(name: &str) -> Result<String, PlannerError> {
    match name {
        "instrument.attack"
        | "instrument.release"
        | "instrument.cutoff"
        | "instrument.resonance"
        | "instrument.pitch"
        | "track.volume" => Ok(name.to_owned()),
        _ if effect_modulation_target(name).is_some() => Ok(name.to_owned()),
        _ => Err(invalid("unknown modulation target")),
    }
}

#[cfg(test)]
fn automation_parameter(name: &str) -> Result<String, PlannerError> {
    if modulator_parameter(name).is_ok() || modulator_automation_target(name).is_some() {
        Ok(name.to_owned())
    } else {
        Err(invalid("unknown automation target"))
    }
}

#[cfg(test)]
fn automation_curve(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "linear" => Ok("linear"),
        "hold" => Ok("hold"),
        _ => Err(invalid("unknown automation curve")),
    }
}

#[cfg(test)]
fn modulator_shape(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "sine" => Ok("sine"),
        "triangle" => Ok("triangle"),
        "square" => Ok("square"),
        "random" => Ok("random"),
        "envelope" => Ok("envelope"),
        _ => Err(invalid("unknown modulator shape")),
    }
}

#[cfg(test)]
fn modulator_rate(value: f64) -> Result<f32, PlannerError> {
    if (0.01..=20.0).contains(&value) {
        Ok(value as f32)
    } else {
        Err(invalid("modulator rate is out of range"))
    }
}

#[cfg(test)]
fn effect_modulation_target(name: &str) -> Option<u64> {
    let target = name.strip_prefix("effect:")?;
    [".mix", ".cutoff", ".resonance"]
        .iter()
        .find_map(|suffix| target.strip_suffix(suffix))?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

#[cfg(test)]
fn modulator_automation_target(name: &str) -> Option<u64> {
    let target = name.strip_prefix("modulator:")?;
    [".rate", ".depth"]
        .iter()
        .find_map(|suffix| target.strip_suffix(suffix))?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

#[cfg(test)]
fn sound_tool_name(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "instrument" => Ok("instrument"),
        "effect" => Ok("effect"),
        "modulator" => Ok("modulator"),
        "event" => Ok("event"),
        "routing" => Ok("routing"),
        _ => Err(invalid("unknown configurable sound tool")),
    }
}

#[cfg(test)]
fn sound_parameter_name(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "waveform" => Ok("waveform"),
        "oscillator1.waveform" => Ok("oscillator1.waveform"),
        "oscillator1.tuning" => Ok("oscillator1.tuning"),
        "oscillator1.level" => Ok("oscillator1.level"),
        "oscillator2.waveform" => Ok("oscillator2.waveform"),
        "oscillator2.tuning" => Ok("oscillator2.tuning"),
        "oscillator2.level" => Ok("oscillator2.level"),
        "attack" => Ok("attack"),
        "release" => Ok("release"),
        "tone" => Ok("tone"),
        "mix" => Ok("mix"),
        "cutoff" => Ok("cutoff"),
        "resonance" => Ok("resonance"),
        "enabled" => Ok("enabled"),
        "shape" => Ok("shape"),
        "rate" => Ok("rate"),
        "rateMode" => Ok("rateMode"),
        "trigger" => Ok("trigger"),
        "depth" => Ok("depth"),
        "target" => Ok("target"),
        "time" => Ok("time"),
        "duration" => Ok("duration"),
        "pitch" => Ok("pitch"),
        "velocity" => Ok("velocity"),
        "position" => Ok("position"),
        _ => Err(invalid("unknown sound-tool parameter")),
    }
}

#[cfg(test)]
fn configuration_value(
    object: &Object,
    parameter: &str,
    numeric_value: f64,
) -> Result<String, PlannerError> {
    let setting = string_field(object, "setting")?.trim();
    if !setting.is_empty() {
        if setting.chars().count() <= 64 {
            return Ok(setting.to_owned());
        }
        return Err(invalid("configure setting is longer than 64 characters"));
    }
    if matches!(
        parameter,
        "oscillator1.tuning"
            | "oscillator1.level"
            | "oscillator2.tuning"
            | "oscillator2.level"
            | "attack"
            | "release"
            | "tone"
            | "mix"
            | "cutoff"
            | "resonance"
            | "rate"
            | "depth"
            | "time"
            | "duration"
            | "pitch"
            | "velocity"
            | "position"
    ) {
        return Ok(numeric_value.to_string());
    }
    Err(invalid(&format!(
        "configure parameter {parameter} requires a non-empty string setting"
    )))
}

#[cfg(test)]
fn concise_model_text(object: &Object, name: &str, maximum: usize) -> Result<String, PlannerError> {
    let value = string_field(object, name)?.trim();
    if value.is_empty() {
        return Err(invalid(&format!("{name} must not be empty")));
    }
    if value.chars().count() <= maximum {
        return Ok(value.to_owned());
    }
    let mut shortened = value
        .chars()
        .take(maximum.saturating_sub(3))
        .collect::<String>();
    shortened.push_str("...");
    Ok(shortened)
}

#[cfg(test)]
fn string_field<'a>(object: &'a Object, name: &str) -> Result<&'a str, PlannerError> {
    object
        .get(name)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| invalid(&format!("{name} must be a string")))
}

#[cfg(test)]
fn number_field(object: &Object, name: &str) -> Result<f64, PlannerError> {
    object
        .get(name)
        .and_then(JsonValue::as_f64)
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid(&format!("{name} must be a finite number")))
}

#[cfg(test)]
fn integer_field(object: &Object, name: &str) -> Result<u64, PlannerError> {
    let value = number_field(object, name)?;
    if value.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&value) {
        Ok(value as u64)
    } else {
        Err(invalid(&format!(
            "{name} must be a non-negative safe integer"
        )))
    }
}

#[cfg(test)]
fn nonzero_integer_field(object: &Object, name: &str) -> Result<u64, PlannerError> {
    let value = integer_field(object, name)?;
    if value == 0 {
        Err(invalid(&format!("{name} must identify an existing track")))
    } else {
        Ok(value)
    }
}

fn invalid(message: &str) -> PlannerError {
    PlannerError::InvalidOutput(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: JsonValue) -> FunctionCall {
        FunctionCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
        }
    }

    #[test]
    fn systemd_credential_precedes_the_home_directory_fallback() {
        assert_eq!(
            credential_path(
                None,
                Some(PathBuf::from("/run/credentials/daw-ai.service")),
                Some(PathBuf::from("/root")),
            ),
            Some(PathBuf::from(
                "/run/credentials/daw-ai.service/gemini-api-key"
            ))
        );
        assert_eq!(
            credential_path(
                Some(PathBuf::from("/explicit/key")),
                Some(PathBuf::from("/run/credentials/daw-ai.service")),
                Some(PathBuf::from("/root")),
            ),
            Some(PathBuf::from("/explicit/key"))
        );
    }

    fn preset_edit(preset: &str) -> JsonValue {
        serde_json::json!({
            "trackId": 2, "tool": "instrument", "toolId": 201, "clipId": 0,
            "parameter": "preset", "value": preset
        })
    }

    #[test]
    fn audio_is_optional_between_consecutive_edits() {
        let session =
            EditSession::create(&Project::demo(), "shape the bass", 4.0, 8.0).expect("session");
        let mut state = LoopState::default();
        let mut updates = 0;
        let mut render_audio = render_audio_request;
        execute_tool(
            &session,
            1,
            &call("set_parameter", preset_edit("Surge Lead")),
            &mut state,
            &mut render_audio,
            &mut |edit| {
                updates += 1;
                Ok(edit.project)
            },
        )
        .expect("edit without baseline audio");
        assert_eq!(updates, 1);

        let audio = call(
            AUDIO_TOOL_NAME,
            serde_json::json!({"tracks": [1, 2, 3], "start": 4, "end": 8}),
        );
        let baseline = execute_tool(
            &session,
            2,
            &audio,
            &mut state,
            &mut render_audio,
            &mut |edit| Ok(edit.project),
        )
        .expect("baseline audio");
        assert_eq!(baseline.result.len(), 1);
        assert_eq!(baseline.result[0]["type"], "text");
        let audio_input = &baseline.supplemental_input[0]["content"][1];
        assert_eq!(audio_input["type"], "audio");
        assert_eq!(audio_input["mime_type"], "audio/wav");
        assert!(audio_input["data"].as_str().unwrap().starts_with("UklGR"));

        execute_tool(
            &session,
            3,
            &call("set_parameter", preset_edit("Surge Lead")),
            &mut state,
            &mut render_audio,
            &mut |edit| {
                updates += 1;
                Ok(edit.project)
            },
        )
        .expect("first edit");
        assert_eq!(updates, 2);

        execute_tool(
            &session,
            4,
            &call("set_parameter", preset_edit("Surge Pad")),
            &mut state,
            &mut render_audio,
            &mut |edit| Ok(edit.project),
        )
        .expect("consecutive edit without audio");

        execute_tool(
            &session,
            5,
            &audio,
            &mut state,
            &mut render_audio,
            &mut |edit| Ok(edit.project),
        )
        .expect("edited audio");
        assert_eq!(state.audio_listens, 2);
    }

    #[test]
    fn multiple_resamples_in_one_interaction_get_unique_artifacts() {
        let mut project = Project::initial();
        project.duration = 4.0;
        let session =
            EditSession::create(&project, "make two resamples", 0.0, 4.0).expect("session");
        let mut state = LoopState::default();
        let mut render_audio = |_: AudioRenderRequest| {
            Ok(AudioRender {
                wav: crate::audio_analysis::wav_bytes(&vec![0.1; 16_000]),
                description: "Rendered one second".to_owned(),
                measurements: serde_json::json!({}),
            })
        };
        for (index, destination) in [0.0, 1.0].into_iter().enumerate() {
            let resample = FunctionCall {
                id: format!("resample-{index}"),
                name: "resample_audio_region".to_owned(),
                arguments: serde_json::json!({
                    "sourceTracks": "all",
                    "sourceStart": 0.0,
                    "sourceEnd": 1.0,
                    "targetTrackId": 1,
                    "destinationStart": destination,
                    "label": format!("Slice {}", index + 1),
                    "gain": 1.0,
                    "reversed": false
                }),
            };
            execute_tool(
                &session,
                2,
                &resample,
                &mut state,
                &mut render_audio,
                &mut |edit| Ok(edit.project),
            )
            .expect("same-interaction resample");
        }

        let artifacts = std::fs::read_dir(session.path())
            .expect("session artifacts")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("audio-"))
            .count();
        assert_eq!(artifacts, 2);
        assert_eq!(state.audio_artifacts, 2);
    }

    #[test]
    fn parses_interactions_function_calls() {
        let response = serde_json::json!({
            "id": "interaction-1",
            "status": "requires_action",
            "steps": [
                {"type": "thought", "content": []},
                {"type": "function_call", "id": "call-1", "name": READ_TOOL_NAME, "arguments": {}}
            ]
        });
        let calls = function_calls(&response).expect("function calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].name, READ_TOOL_NAME);
    }
    #[test]
    fn producer_can_finish_immediately_after_listening() {
        let session =
            EditSession::create(&Project::demo(), "shape the bass", 0.0, 4.0).expect("session");
        let responses = [
            serde_json::json!({
                "id": "research", "status": "completed", "steps": [
                    {"type": "google_search_result"},
                    {"type": "model_output", "content": [{"type": "text", "text": "Use a brighter bass."}]}
                ]
            }),
            serde_json::json!({
                "id": "edit", "status": "requires_action", "steps": [{
                    "type": "function_call", "id": "edit-bass", "name": "set_parameter",
                    "arguments": preset_edit("Surge Lead")
                }]
            }),
            serde_json::json!({
                "id": "listen", "status": "requires_action", "steps": [{
                    "type": "function_call", "id": "listen-bass", "name": AUDIO_TOOL_NAME,
                    "arguments": {"tracks": [2], "start": 0, "end": 4}
                }]
            }),
            serde_json::json!({
                "id": "done", "status": "completed", "steps": [
                    {"type": "model_output", "content": [{"type": "text", "text": "Done."}]}
                ]
            }),
        ];
        let mut response_index = 0;
        let mut updates = 0;
        let result = run_session_with_transport(
            &session,
            "shape the bass",
            0.0,
            4.0,
            &mut render_audio_request,
            &mut |edit| {
                updates += 1;
                Ok(edit.project)
            },
            &|| false,
            &mut |_, _, _| {
                let response = responses[response_index].to_string();
                response_index += 1;
                Ok(response)
            },
        )
        .expect("producer session");

        assert_eq!(response_index, 4);
        assert_eq!(updates, 1);
        assert_eq!(result.project.tracks[1].instrument.preset, "Surge Lead");
        assert_eq!(session.stats().unwrap(), (1, 1));
    }

    #[test]
    fn cancelled_interaction_terminates_its_transport() {
        let session = EditSession::create(&Project::demo(), "cancel", 0.0, 4.0).expect("session");
        let cancellation = Arc::new(AtomicBool::new(true));
        let result = call_interactions(
            &session,
            "cancelled",
            &serde_json::json!({"model": GEMINI_MODEL}),
            "test-key",
            "http://127.0.0.1:9",
            Duration::from_secs(30),
            &cancellation,
        );
        assert!(matches!(result, Err(PlannerError::Interrupted)));
    }

    #[test]
    fn transient_service_unavailability_retries_the_same_interaction() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut attempts = Vec::new();
        let mut responses = [
            r#"{"error":{"message":"The service is currently unavailable.","code":"service_unavailable"}}"#,
            r#"{"error":{"message":"The service is currently unavailable.","code":"service_unavailable"}}"#,
            r#"{"id":"recovered","status":"completed","steps":[]}"#,
        ]
        .into_iter();
        let response = retry_transient_interaction(
            7,
            Duration::from_secs(1),
            &cancellation,
            &[Duration::ZERO, Duration::ZERO],
            &mut |name, _| {
                attempts.push(name.to_owned());
                Ok(responses.next().expect("retry response").to_owned())
            },
        )
        .expect("transient interaction recovery");

        assert_eq!(
            attempts,
            [
                "interaction-007",
                "interaction-007-retry-1",
                "interaction-007-retry-2"
            ]
        );
        assert!(response.contains("\"id\":\"recovered\""));
    }

    #[test]
    fn transient_error_code_retries_even_with_a_new_message() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut attempt = 0;
        let response = retry_transient_interaction(
            8,
            Duration::from_secs(1),
            &cancellation,
            &[Duration::ZERO],
            &mut |_, _| {
                attempt += 1;
                if attempt == 1 {
                    Err(PlannerError::Failed {
                        message: "capacity is temporarily constrained".to_owned(),
                        code: Some("service_unavailable".to_owned()),
                    })
                } else {
                    Ok(r#"{"status":"completed","steps":[]}"#.to_owned())
                }
            },
        )
        .expect("structured transient error recovery");
        assert_eq!(attempt, 2);
        assert!(response.contains("completed"));
    }

    #[test]
    fn gemini_prompt_encourages_iterative_listening_without_a_tempo_assumption() {
        let task = planner_task("make the bass hit harder", 4.0, 8.0);
        let instruction = system_instruction();
        assert!(task.contains("listen after each change"));
        assert!(task.contains("iterate on composition and sound design"));
        assert!(instruction.contains("selected region bounds edits only"));
        assert!(instruction.contains("chooses its own absolute project start and end"));
        assert!(instruction.contains("listen after each change"));
        assert!(instruction.contains("iterate on composition and sound design"));
        assert!(instruction.contains("objective measurements"));
        assert!(instruction.contains("actual audio - not event-count proxies"));
        assert!(instruction.contains("rhythmic subdivision"));
        assert!(instruction.contains("Default drums, bass grooves"));
        assert!(instruction.contains("reserve one-shot MIDI phrases mainly for melody"));
        assert!(instruction.contains("tempo must change"));
        assert!(instruction.contains("no separate completion reviewer"));
        assert!(instruction.contains("no predetermined tool-call or iteration limit"));
    }

    #[test]
    fn parses_scoped_parameter_automation() {
        let plan = plan_from_json(
            r#"{
                "summary":"Opened the bass filter through the transition",
                "musicalPlan":"Build brightness only through the middle of the selection.",
                "actions":[{
                    "kind":"automation","target":"bass","name":"effect:210.cutoff","value":0,
                    "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                    "setting":"linear","start":0.2,"end":0.8,"rate":0,
                    "points":[
                        {"time":0,"value":300},
                        {"time":0.5,"value":4000},
                        {"time":1,"value":12000}
                    ]
                }]
            }"#,
        )
        .expect("valid scoped automation");

        assert_eq!(
            plan.action,
            Action::Timed {
                start: 0.2,
                end: 0.8,
                action: Box::new(Action::Automation {
                    track_id: 2,
                    parameter: "effect:210.cutoff".to_owned(),
                    curve: "linear",
                    points: vec![
                        AutomationPoint {
                            time: 0.0,
                            value: 300.0,
                        },
                        AutomationPoint {
                            time: 0.5,
                            value: 4000.0,
                        },
                        AutomationPoint {
                            time: 1.0,
                            value: 12000.0,
                        },
                    ],
                    target: TrackRole::Bass,
                }),
            }
        );
    }

    #[test]
    fn parses_a_compound_structured_edit() {
        let plan = plan_from_json(
            r#"{
                "summary":"Warmed the chords and added space",
                "musicalPlan":"Darken the chord timbre and add a long ambient tail.",
                "actions":[
                    {"kind":"filter","target":"chords","name":"None","value":-0.3},
                    {"kind":"effect","target":"chords","name":"Reverb","value":0.42},
                    {"kind":"effect","target":"chords","name":"Drive","value":0.2}
                ]
            }"#,
        )
        .expect("valid plan");
        assert_eq!(
            plan.action,
            Action::Compound {
                actions: vec![
                    Action::Filter {
                        amount: -0.3,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Effect {
                        name: "Reverb",
                        mix: 0.42,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Effect {
                        name: "Drive",
                        mix: 0.2,
                        target: Some(TrackRole::Chords),
                    },
                ],
            }
        );
    }

    #[test]
    fn parses_an_explicit_midi_clip() {
        let plan = plan_from_json(
            r#"{
                "summary":"Wrote a syncopated bass phrase",
                "musicalPlan":"Replace the selected bass with a low, syncopated two-beat MIDI loop.",
                "actions":[{
                    "kind":"midi-clip","target":"bass","name":"MIDI Clip","value":2,
                    "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                    "setting":"Syncopated bass","start":0,"end":1,"rate":0,
                    "events":[
                        {"time":0,"duration":0.5,"pitch":29,"velocity":1},
                        {"time":1.25,"duration":0.5,"pitch":32,"velocity":0.85}
                    ]
                }]
            }"#,
        )
        .expect("valid MIDI clip plan");
        let Action::MidiClip {
            track_id,
            target,
            loop_beats,
            notes,
            ..
        } = plan.action
        else {
            panic!("expected MIDI clip");
        };
        assert_eq!(track_id, 2);
        assert_eq!(target, TrackRole::Bass);
        assert_eq!(loop_beats, 2.0);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].pitch, 32);
    }

    #[test]
    fn parses_an_empty_midi_clip_as_a_region_clear() {
        let plan = plan_from_json(
            r#"{
                "summary":"Cleared the selected bass region",
                "musicalPlan":"Make room for a replacement bass part by clearing the old MIDI.",
                "actions":[{
                    "kind":"midi-clip","target":"bass","name":"MIDI Clip","value":4,
                    "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                    "setting":"Bass rest","start":0,"end":1,"rate":0,"events":[]
                }]
            }"#,
        )
        .expect("valid empty MIDI clip plan");

        assert!(matches!(
            plan.action,
            Action::MidiClip {
                track_id: 2,
                target: TrackRole::Bass,
                notes,
                ..
            } if notes.is_empty()
        ));
    }

    #[test]
    fn rejects_midi_note_duration_longer_than_its_loop() {
        let invalid = r#"{
            "summary":"Wrote a short bass loop",
            "musicalPlan":"Replace the selection with a quarter-beat bass loop.",
            "actions":[{
                "kind":"midi-clip","target":"bass","name":"MIDI Clip","value":0.25,
                "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                "setting":"Short bass loop","start":0,"end":1,"rate":0,
                "events":[{"time":0,"duration":16,"pitch":29,"velocity":1}]
            }]
        }"#;
        assert!(plan_from_json(invalid).is_err());
    }

    #[test]
    fn rejects_invalid_structured_edits() {
        let invalid = r#"{
            "summary":"Unsafe tempo",
            "musicalPlan":"Raise the tempo beyond the supported range.",
            "actions":[{"kind":"tempo","target":"all","name":"None","value":999}]
        }"#;
        assert!(plan_from_json(invalid).is_err());
    }

    #[test]
    fn parses_sound_tool_actions() {
        let plan = plan_from_json(
            r#"{
                "summary":"Changed the bass source and added movement",
                "musicalPlan":"Use a lead patch and square-wave cutoff modulation.",
                "actions":[
                    {"kind":"instrument","target":"bass","name":"Surge Lead","value":0},
                    {"kind":"modulator","target":"bass","name":"instrument.cutoff","value":0.25,"setting":"square","rate":2}
                ]
            }"#,
        )
        .expect("valid sound tool plan");
        assert_eq!(
            plan.action,
            Action::Compound {
                actions: vec![
                    Action::Instrument {
                        preset: "Surge Lead",
                        target: TrackRole::Bass,
                    },
                    Action::Modulator {
                        parameter: "instrument.cutoff".to_owned(),
                        shape: "square",
                        rate: 2.0,
                        depth: 0.25,
                        target: TrackRole::Bass,
                    },
                ],
            }
        );
    }

    #[test]
    fn parses_any_published_modulation_target() {
        let plan = plan_from_json(
            r#"{
                "summary":"Route movement to the bass filter cutoff",
                "musicalPlan":"Add slow sine movement to the existing bass filter cutoff.",
                "actions":[{
                    "kind":"modulator","target":"bass","name":"effect:210.cutoff","value":0.25,
                    "trackId":0,"tool":"None","toolId":0,"clipId":0,"parameter":"None","setting":"sine","rate":0.5
                }]
            }"#,
        )
        .expect("valid stable-ID modulation target");
        assert_eq!(
            plan.action,
            Action::Modulator {
                parameter: "effect:210.cutoff".to_owned(),
                shape: "sine",
                rate: 0.5,
                depth: 0.25,
                target: TrackRole::Bass,
            }
        );
    }

    #[test]
    fn parses_stable_id_sound_tool_configuration() {
        let plan = plan_from_json(
            r#"{
                "summary":"Shortened the selected bass event",
                "musicalPlan":"Tighten the first bass note while preserving its pitch and velocity.",
                "actions":[{
                    "kind":"configure","target":"bass","name":"None","value":0,
                    "trackId":2,"tool":"event","toolId":1201,"clipId":12,
                    "parameter":"duration","setting":"0.0625"
                }]
            }"#,
        )
        .expect("valid configuration action");
        assert_eq!(
            plan.action,
            Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "event",
                tool_id: 1201,
                clip_id: Some(12),
                parameter: "duration",
                value: "0.0625".to_owned(),
            }
        );
    }

    #[test]
    fn parses_numeric_configuration_from_the_numeric_value_field() {
        let plan = plan_from_json(
            r#"{
                "summary":"Raised the bass filter mix",
                "musicalPlan":"Make the existing low-pass filter more prominent.",
                "actions":[{
                    "kind":"configure","target":"bass","name":"None","value":0.9,
                    "trackId":2,"tool":"effect","toolId":210,"clipId":0,
                    "parameter":"mix","setting":"","start":0,"end":1,"rate":0,"events":[]
                }]
            }"#,
        )
        .expect("numeric configure value");
        assert_eq!(
            plan.action,
            Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "effect",
                tool_id: 210,
                clip_id: None,
                parameter: "mix",
                value: "0.9".to_owned(),
            }
        );
    }

    #[test]
    fn shortens_model_prose_without_rejecting_a_valid_edit() {
        let long_summary = "bass ".repeat(50);
        let long_plan = "wobble ".repeat(100);
        let source = serde_json::json!({
            "summary": long_summary,
            "musicalPlan": long_plan,
            "actions": [{
                "kind": "effect", "target": "bass", "name": "Drive", "value": 0.5
            }]
        })
        .to_string();
        let plan = plan_from_json(&source).expect("valid edit with verbose prose");
        assert_eq!(plan.summary.chars().count(), 160);
        assert!(plan.summary.ends_with("..."));
    }

    #[test]
    fn decodes_json_surrogate_pairs_and_rejects_unpaired_surrogates() {
        let valid = r#"{
            "summary":"Added sparkle \uD83C\uDFB6",
            "musicalPlan":"Open the chord tone slightly.",
            "actions":[{"kind":"filter","target":"chords","name":"None","value":0.2}]
        }"#;
        assert_eq!(
            plan_from_json(valid).expect("valid surrogate pair").summary,
            "Added sparkle \u{1F3B6}"
        );

        for summary in [r#""Bad \uD83C text""#, r#""Bad \uDFB6 text""#] {
            let invalid = format!(
                r#"{{"summary":{summary},"musicalPlan":"Open the chord tone slightly.","actions":[{{"kind":"filter","target":"chords","name":"None","value":0.2}}]}}"#
            );
            assert!(plan_from_json(&invalid).is_err());
        }
    }
}
