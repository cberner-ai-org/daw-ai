use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use curl::easy::{Easy, List};
use serde_json::Value as JsonValue;

use crate::gemini_session::{EditSession, SessionVariants, apply_session_retention};
#[cfg(test)]
use crate::gemini_tools::render_audio_request;
use crate::gemini_tools::{
    ANALYZE_AUDIO_TOOL_NAME, AUDIO_TOOL_NAME, AUDITION_TOOL_NAME, AudioRender, AudioRenderRequest,
    COMMIT_AUDITION_TOOL_NAME, CREATE_AUDITION_TOOL_NAME, DELETE_AUDITION_TOOL_NAME,
    INSTRUMENT_PARAMETER_TOOL_NAME, LOAD_TOOL_GROUP_NAME, PRESET_TOOL_NAME,
    READ_AUDITION_TOOL_NAME, READ_TOOL_NAME, SOUND_TOOL_PARAMETER_TOOL_NAME, ToolGroup,
    apply_agent_mutation, apply_audition_mutation, base64_audio, create_audition_slot,
    current_project, delete_audition_slot, dynamic_tool_declarations, dynamic_tool_group,
    is_mutation_tool, list_instrument_parameters, list_sound_tool_parameters, list_surge_presets,
    prepare_audio_render, prepare_instrument_audition, read_audition_slot, read_sound_graph,
    record_instrument_audition,
};
use crate::model::{Clip, Project};
use crate::prompt::EditPlan;
use crate::storage::read_bounded_text_following_links;

const STUDIO_CONTRACT: &str = include_str!("../gemini/STUDIO.md");
pub(crate) const GEMINI_MODEL: &str = "gemini-3.6-flash";
const DEFAULT_INTERACTIONS_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/interactions";
const SYSTEMD_CREDENTIAL_NAME: &str = "gemini-api-key";
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
const MAX_INTERACTION_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_MUTATIONS_WITHOUT_FEEDBACK: usize = 4;
const MUSICAL_PLANNING_GUIDANCE: &str = concat!(
    "Before the first sound-graph mutation, inspect the current graph and form a concise musical ",
    "plan based on the user's request, selected region, and existing composition. Use that plan ",
    "to choose tools: rhythm, melody, and harmony map to MIDI pitches, timing, durations, and ",
    "velocities; register and layering map to tracks and Rack key zones; timbre and articulation ",
    "map to an initialized or preset instrument and its parameters; movement maps to modulators; ",
    "tone, dynamics, and space map to effects, routing, and track mix. For example, a planned ",
    "shorter articulation can use MIDI duration and/or envelope parameters, while a planned ",
    "register-specific layer can use a new instrument track plus a key zone. These mappings ",
    "explain how to realize your plan; they do not prescribe the musical choices in it."
);
pub(crate) const EDIT_TIMEOUT_SECONDS: u64 = 20 * 60;
#[cfg(test)]
const EDIT_TIMEOUT: Duration = Duration::from_secs(EDIT_TIMEOUT_SECONDS);
const TRANSIENT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

#[derive(Clone, Copy)]
struct SessionOptions {
    new_prompt: bool,
    deadline: Instant,
}

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
    pub selection_start: f32,
    pub selection_end: f32,
}

#[derive(Default)]
struct LoopState {
    plans: Vec<EditPlan>,
    audio_listens: usize,
    audio_artifacts: usize,
    input_tokens: u64,
    output_tokens: u64,
    thought_tokens: u64,
    tool_calls: BTreeMap<String, usize>,
    failed_tool_calls: usize,
    mutations_since_listen: usize,
    mutations_since_feedback: usize,
    mutations_since_arrangement_listen: usize,
    arrangement_listen_requirement: Option<ArrangementListenRequirement>,
    mutations_before_first_listen: Option<usize>,
    mutations_between_listens: Vec<usize>,
    auditions: usize,
    auditioned_slots: BTreeSet<u64>,
    applied_auditions: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct ArrangementListenRequirement {
    start: f32,
    end: f32,
    track_ids: BTreeSet<u64>,
}

impl ArrangementListenRequirement {
    fn covers(&self, request: &AudioRenderRequest) -> bool {
        request.start < self.end
            && self.start < request.end
            && self
                .track_ids
                .iter()
                .all(|track_id| request.track_ids.contains(track_id))
    }

    fn guidance(&self) -> String {
        let track_ids = self
            .track_ids
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "The render range must overlap {:.3}-{:.3} seconds and include every affected track: [{track_ids}].",
            self.start, self.end
        )
    }
}

impl LoopState {
    fn record_usage(&mut self, response: &JsonValue) {
        let usage = &response["usage"];
        self.input_tokens += usage["total_input_tokens"].as_u64().unwrap_or(0);
        self.output_tokens += usage["total_output_tokens"].as_u64().unwrap_or(0);
        self.thought_tokens += usage["total_thought_tokens"].as_u64().unwrap_or(0);
    }

    fn record_call(&mut self, name: &str) {
        *self.tool_calls.entry(name.to_owned()).or_default() += 1;
    }

    fn record_mutation(&mut self) {
        self.mutations_since_listen += 1;
        self.mutations_since_feedback += 1;
        self.mutations_since_arrangement_listen += 1;
    }

    fn record_project_mutation(&mut self, requirement: ArrangementListenRequirement) {
        self.record_mutation();
        self.arrangement_listen_requirement = Some(requirement);
    }

    fn record_listen(&mut self) {
        if self.audio_listens == 0 {
            self.mutations_before_first_listen = Some(self.mutations_since_listen);
        } else {
            self.mutations_between_listens
                .push(self.mutations_since_listen);
        }
        self.mutations_since_listen = 0;
        self.audio_listens += 1;
    }

    fn record_arrangement_feedback(&mut self) {
        self.mutations_since_feedback = 0;
    }

    fn arrangement_render_covers_latest_mutation(&self, request: &AudioRenderRequest) -> bool {
        self.arrangement_listen_requirement
            .as_ref()
            .is_none_or(|requirement| requirement.covers(request))
    }

    fn record_arrangement_listen(&mut self, covers_latest_mutation: bool) {
        self.record_listen();
        self.record_arrangement_feedback();
        if covers_latest_mutation {
            self.mutations_since_arrangement_listen = 0;
            self.arrangement_listen_requirement = None;
        }
    }

    fn periodic_feedback_required(&self) -> bool {
        self.mutations_since_feedback >= MAX_MUTATIONS_WITHOUT_FEEDBACK
    }

    fn completion_listen_required(&self) -> bool {
        self.mutations_since_arrangement_listen > 0
    }

    fn completion_listen_guidance(&self) -> Option<String> {
        self.arrangement_listen_requirement
            .as_ref()
            .map(ArrangementListenRequirement::guidance)
    }

    fn metrics(&self, elapsed: Duration) -> JsonValue {
        let average_between = if self.mutations_between_listens.is_empty() {
            0.0
        } else {
            self.mutations_between_listens.iter().sum::<usize>() as f64
                / self.mutations_between_listens.len() as f64
        };
        serde_json::json!({
            "durationMs": elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
            "thoughtTokens": self.thought_tokens,
            "totalToolCalls": self.tool_calls.values().sum::<usize>(),
            "failedToolCalls": self.failed_tool_calls,
            "toolCalls": self.tool_calls,
            "mutationsBeforeFirstListen": self.mutations_before_first_listen.unwrap_or(self.mutations_since_listen),
            "averageMutationsBetweenListens": average_between,
            "maxMutationsBetweenListens": self.mutations_between_listens.iter().copied().max().unwrap_or(0),
            "auditions": self.auditions,
            "appliedAuditions": self.applied_auditions,
            "auditionApplyRate": if self.auditions == 0 { 0.0 } else { self.applied_auditions as f64 / self.auditions as f64 }
        })
    }
}

fn clip_recipient_track_ids(project: &Project, clip: &Clip) -> BTreeSet<u64> {
    project
        .tracks
        .iter()
        .filter_map(|track| {
            clip.events
                .iter()
                .any(|event| project.track_receives_pitch(track, event.pitch))
                .then_some(track.id)
        })
        .collect()
}

fn arrangement_listen_requirement(
    name: &str,
    arguments: &JsonValue,
    result: &str,
    previous_project: &Project,
    project: &Project,
    selection_start: f32,
    selection_end: f32,
) -> ArrangementListenRequirement {
    let response = serde_json::from_str::<JsonValue>(result).ok();
    let clip_timing = response.as_ref().and_then(|value| value.get("clipTiming"));
    let previous_clip = matches!(name, "update_midi_clip" | "delete_midi_clip")
        .then(|| arguments["clipId"].as_u64())
        .flatten()
        .and_then(|clip_id| {
            previous_project
                .clips
                .iter()
                .find(|clip| clip.id == clip_id)
        });
    let start = clip_timing
        .and_then(|timing| timing["startSeconds"].as_f64())
        .map(|seconds| seconds as f32)
        .or_else(|| {
            (name == "delete_midi_clip")
                .then(|| previous_clip.map(|clip| clip.start.max(selection_start)))
                .flatten()
        })
        .unwrap_or(selection_start);
    let end = clip_timing
        .and_then(|timing| timing["endSeconds"].as_f64())
        .map(|seconds| seconds as f32)
        .or_else(|| {
            (name == "delete_midi_clip")
                .then(|| previous_clip.map(|clip| clip.end.min(selection_end)))
                .flatten()
        })
        .unwrap_or(selection_end);

    let mut track_ids = BTreeSet::new();
    if let Some(clip) = previous_clip {
        track_ids.extend(clip_recipient_track_ids(previous_project, clip));
    }
    if matches!(name, "update_key_zone" | "delete_key_zone")
        && let Some(zone_id) = arguments["keyZoneId"].as_u64()
        && let Some(zone) = previous_project
            .key_zones
            .iter()
            .find(|zone| zone.id == zone_id)
        && let Some(track) = previous_project
            .tracks
            .iter()
            .find(|track| track.instrument.id == zone.instrument_id)
        && project.tracks.iter().any(|current| current.id == track.id)
    {
        track_ids.insert(track.id);
    }
    if let Some(track_id) = arguments["trackId"].as_u64()
        && project.tracks.iter().any(|track| track.id == track_id)
    {
        track_ids.insert(track_id);
    }
    if let Some(instrument_id) = arguments["instrumentId"].as_u64()
        && let Some(track) = project
            .tracks
            .iter()
            .find(|track| track.instrument.id == instrument_id)
    {
        track_ids.insert(track.id);
    }
    if matches!(name, "new_track" | COMMIT_AUDITION_TOOL_NAME)
        && let Some(track_id) = response.as_ref().and_then(|value| value["id"].as_u64())
        && project.tracks.iter().any(|track| track.id == track_id)
    {
        track_ids.insert(track_id);
    }
    if matches!(name, "add_midi_clip" | "update_midi_clip")
        && let Some(clip_id) = response.as_ref().and_then(|value| value["id"].as_u64())
        && let Some(clip) = project.clips.iter().find(|clip| clip.id == clip_id)
    {
        track_ids.extend(clip_recipient_track_ids(project, clip));
    }
    if track_ids.is_empty() {
        track_ids.extend(project.tracks.iter().map(|track| track.id));
    }

    ArrangementListenRequirement {
        start,
        end,
        track_ids,
    }
}

impl GeminiPlanner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn interpret_with_updates(
        session_root: &std::path::Path,
        prompt: &str,
        start: f32,
        end: f32,
        project: &Project,
        new_prompt: bool,
        deadline: Instant,
        cancellation: Arc<AtomicBool>,
        mut render_audio: impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
        mut on_update: impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
    ) -> Result<GeminiEdit, PlannerError> {
        let session = EditSession::create_in(
            session_root,
            project,
            prompt,
            start,
            end,
            SessionVariants { new_prompt },
        )
        .map_err(PlannerError::Io)?;
        let options = SessionOptions {
            new_prompt,
            deadline,
        };
        let result = run_session(
            &session,
            prompt,
            start,
            end,
            options,
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
        if let Err(error) = apply_session_retention(session_root) {
            eprintln!("warning: could not apply Gemini session retention: {error}");
        }
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    session: &EditSession,
    prompt: &str,
    start: f32,
    end: f32,
    options: SessionOptions,
    cancellation: Arc<AtomicBool>,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
) -> Result<GeminiEdit, PlannerError> {
    let api_key = load_api_key()?;
    let endpoint = std::env::var("DAW_AI_GEMINI_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_INTERACTIONS_ENDPOINT.to_owned());
    run_session_with_transport_options(
        session,
        prompt,
        start,
        end,
        options,
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

#[cfg(test)]
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
    run_session_with_transport_options(
        session,
        prompt,
        start,
        end,
        SessionOptions {
            new_prompt: false,
            deadline: Instant::now() + EDIT_TIMEOUT,
        },
        render_audio,
        on_update,
        is_cancelled,
        transport,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_session_with_transport_options(
    session: &EditSession,
    prompt: &str,
    start: f32,
    end: f32,
    options: SessionOptions,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
    is_cancelled: &impl Fn() -> bool,
    transport: &mut impl FnMut(usize, &JsonValue, Duration) -> Result<String, PlannerError>,
) -> Result<GeminiEdit, PlannerError> {
    let started = Instant::now();
    let mut loaded_tool_group: Option<ToolGroup> = None;
    let mut input = JsonValue::String(planner_task(prompt, start, end, options.new_prompt));
    let mut previous_interaction_id: Option<String> = None;
    let mut sequence = 0_usize;
    let mut state = LoopState::default();

    loop {
        if is_cancelled() {
            return Err(PlannerError::Interrupted);
        }
        let remaining = options
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(PlannerError::TimedOut)?;
        sequence += 1;
        let tools = dynamic_tool_declarations(loaded_tool_group);
        let mut request = serde_json::json!({
            "model": GEMINI_MODEL,
            "input": input,
            "tools": &tools,
            "system_instruction": system_instruction(options.new_prompt),
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
        state.record_usage(&response);
        previous_interaction_id = Some(
            response
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| invalid("interaction response omitted its ID"))?
                .to_owned(),
        );
        let calls = function_calls(&response)?;
        if calls.is_empty() {
            session
                .update_metrics(&state.metrics(started.elapsed()))
                .map_err(PlannerError::Io)?;
            if state.plans.is_empty() {
                input = JsonValue::String(format!(
                    "You have not made an edit. Call {READ_TOOL_NAME}, then use a concrete CRUD graph mutation such as new_track, add_midi_clip, or set_instrument_parameter. {AUDIO_TOOL_NAME} is available whenever listening would help you decide."
                ));
                continue;
            }
            if state.completion_listen_required() {
                let coverage = state
                    .completion_listen_guidance()
                    .unwrap_or_else(|| "Render the active edit selection.".to_owned());
                input = JsonValue::String(format!(
                    "Before completing, call {AUDIO_TOOL_NAME} after the latest successful project mutation and listen to the returned WAV. {coverage} The render must succeed; {ANALYZE_AUDIO_TOOL_NAME} measurements and isolated audition audio do not verify the arrangement."
                ));
                continue;
            }
            let (plan, project) = session
                .finish(state.plans)
                .map_err(|message| invalid(&message))?;
            let (selection_start, selection_end) =
                crate::gemini_tools::edit_selection(session.path()).map_err(|message| {
                    invalid(&format!(
                        "could not read the active edit selection: {message}"
                    ))
                })?;
            return Ok(GeminiEdit {
                plan,
                project,
                selection_start,
                selection_end,
            });
        }

        let mut results = Vec::with_capacity(calls.len() * 2);
        for (index, call) in calls.into_iter().enumerate() {
            if is_cancelled() {
                return Err(PlannerError::Interrupted);
            }
            state.record_call(&call.name);
            let output = if index == 0 && call.name == LOAD_TOOL_GROUP_NAME {
                let group = call
                    .arguments
                    .get("group")
                    .and_then(JsonValue::as_str)
                    .and_then(ToolGroup::parse);
                match group {
                    Some(group) => {
                        loaded_tool_group = Some(group);
                        let group_name = group.as_str();
                        let available_tools = dynamic_tool_declarations(loaded_tool_group)
                            .into_iter()
                            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                            .collect::<Vec<_>>();
                        ToolOutput::text(
                            serde_json::json!({
                                "message":format!("Loaded {group_name} editing tools"),
                                "group":group_name,
                                "availableTools":available_tools
                            })
                            .to_string(),
                        )
                    }
                    None => ToolOutput::text(
                        "Tool error: group must be arrangement or sound".to_owned(),
                    ),
                }
            } else if index == 0 && !tools.iter().any(|tool| tool["name"] == call.name) {
                let recovery = dynamic_tool_group(&call.name).map_or_else(
                    || format!("tool {} is unknown", call.name),
                    |group| {
                        let group = group.as_str();
                        format!(
                            "{} is not currently available; call {LOAD_TOOL_GROUP_NAME} with {{\"group\":\"{group}\"}}",
                            call.name
                        )
                    },
                );
                ToolOutput::text(format!("Tool error: {recovery}"))
            } else if index == 0
                && state.periodic_feedback_required()
                && is_project_mutation_call(&call)
            {
                ToolOutput::text(format!(
                    "Tool error: at most {MAX_MUTATIONS_WITHOUT_FEEDBACK} successful project mutations may occur without arrangement feedback. Call {ANALYZE_AUDIO_TOOL_NAME} or {AUDIO_TOOL_NAME}, then retry this mutation. The feedback must succeed; isolated audition audio does not qualify."
                ))
            } else if index == 0 {
                execute_tool(
                    session,
                    sequence,
                    &call,
                    &mut state,
                    render_audio,
                    on_update,
                )?
            } else {
                ToolOutput::text("Tool error: call one tool at a time; retry this call".to_owned())
            };
            if output.failed() {
                state.failed_tool_calls += 1;
            }
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
        session
            .update_metrics(&state.metrics(started.elapsed()))
            .map_err(PlannerError::Io)?;
        input = JsonValue::Array(results);
    }
}

struct FunctionCall {
    id: String,
    name: String,
    arguments: JsonValue,
}

fn is_project_mutation_call(call: &FunctionCall) -> bool {
    (is_mutation_tool(&call.name) || call.name == "set_parameter")
        && (call.name == COMMIT_AUDITION_TOOL_NAME || call.arguments.get("auditionId").is_none())
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

    fn failed(&self) -> bool {
        self.result.iter().any(|part| {
            part.get("text")
                .and_then(JsonValue::as_str)
                .is_some_and(|text| text.starts_with("Tool error:"))
        })
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
        READ_TOOL_NAME => Ok(ToolOutput::text(
            match read_sound_graph(session.path(), &call.arguments) {
                Ok(graph) => graph,
                Err(error) => format!("Tool error: {error}"),
            },
        )),
        PRESET_TOOL_NAME => Ok(ToolOutput::text(
            list_surge_presets(&call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        INSTRUMENT_PARAMETER_TOOL_NAME => Ok(ToolOutput::text(
            list_instrument_parameters(session.path(), &call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        SOUND_TOOL_PARAMETER_TOOL_NAME => Ok(ToolOutput::text(
            list_sound_tool_parameters(session.path(), &call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        CREATE_AUDITION_TOOL_NAME => Ok(ToolOutput::text(
            create_audition_slot(session.path(), &call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        READ_AUDITION_TOOL_NAME => Ok(ToolOutput::text(
            read_audition_slot(session.path(), &call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        DELETE_AUDITION_TOOL_NAME => Ok(ToolOutput::text(
            delete_audition_slot(session.path(), &call.arguments)
                .unwrap_or_else(|error| format!("Tool error: {error}")),
        )),
        name if name != COMMIT_AUDITION_TOOL_NAME
            && (is_mutation_tool(name) || name == "set_parameter")
            && call.arguments.get("auditionId").is_some() =>
        {
            Ok(ToolOutput::text(
                apply_audition_mutation(session.path(), name, &call.arguments)
                    .unwrap_or_else(|error| format!("Tool error: {error}")),
            ))
        }
        name if is_mutation_tool(name) || name == "set_parameter" => Ok(ToolOutput::text(
            apply_and_commit_mutation(session, &call.arguments, name, state, on_update)?,
        )),
        ANALYZE_AUDIO_TOOL_NAME => {
            let result = match prepare_audio_render(session.path(), &call.arguments)
                .and_then(render_audio)
            {
                Ok(audio) => {
                    state.record_arrangement_feedback();
                    audio.measurements.to_string()
                }
                Err(error) => format!("Tool error: {error}"),
            };
            Ok(ToolOutput::text(result))
        }
        AUDIO_TOOL_NAME | AUDITION_TOOL_NAME => {
            let prepared = if call.name == AUDIO_TOOL_NAME {
                prepare_audio_render(session.path(), &call.arguments)
            } else {
                prepare_instrument_audition(session.path(), &call.arguments)
            };
            let rendered = prepared.and_then(|request| {
                let covers_latest_mutation = call.name == AUDIO_TOOL_NAME
                    && state.arrangement_render_covers_latest_mutation(&request);
                render_audio(request).map(|audio| (audio, covers_latest_mutation))
            });
            match rendered {
                Ok((audio, covers_latest_mutation)) => {
                    if call.name == AUDIO_TOOL_NAME {
                        state.record_arrangement_listen(covers_latest_mutation);
                    } else {
                        state.record_listen();
                    }
                    if call.name == AUDITION_TOOL_NAME {
                        state.auditions += 1;
                        if let Some(audition_id) = call.arguments["auditionId"].as_u64() {
                            state.auditioned_slots.insert(audition_id);
                        }
                    }
                    state.audio_artifacts += 1;
                    let audio_name = session
                        .record_audio(sequence * 1_000_000 + state.audio_artifacts, &audio.wav)
                        .map_err(PlannerError::Io)?;
                    let mut description =
                        format!("{} Session artifact: {audio_name}.", audio.description);
                    if call.name == AUDIO_TOOL_NAME
                        && !covers_latest_mutation
                        && let Some(coverage) = state.completion_listen_guidance()
                    {
                        description.push_str(&format!(
                            " This render does not verify the latest project mutation. {coverage}"
                        ));
                    }
                    if call.name == AUDITION_TOOL_NAME
                        && let Err(error) =
                            record_instrument_audition(session.path(), &call.arguments)
                    {
                        description.push_str(&format!(
                            " Advisory warning: the audition succeeded, but its note provenance could not be recorded: {error}."
                        ));
                    }
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
                                        "Audio produced by {} for function call {}. Listen to this WAV before deciding what to do next.",
                                        call.name, call.id
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
    let previous_project = current_project(session.path()).map_err(|message| {
        invalid(&format!(
            "could not snapshot the project before editing: {message}"
        ))
    })?;
    match apply_agent_mutation(session.path(), name, arguments) {
        Ok(message) => {
            let (plan, project) = session
                .take_update()
                .map_err(|message| invalid(&message))?
                .ok_or_else(|| invalid("mutation tool did not publish its graph update"))?;
            let (selection_start, selection_end) =
                crate::gemini_tools::edit_selection(session.path()).map_err(|message| {
                    invalid(&format!(
                        "could not read the active edit selection: {message}"
                    ))
                })?;
            let committed = on_update(GeminiEdit {
                plan: plan.clone(),
                project,
                selection_start,
                selection_end,
            })?;
            session
                .synchronize_project(&committed)
                .map_err(|message| invalid(&message))?;
            let listen_requirement = arrangement_listen_requirement(
                name,
                arguments,
                &message,
                &previous_project,
                &committed,
                selection_start,
                selection_end,
            );
            state.plans.push(plan);
            state.record_project_mutation(listen_requirement);
            if name == COMMIT_AUDITION_TOOL_NAME
                && arguments["auditionId"]
                    .as_u64()
                    .is_some_and(|audition_id| state.auditioned_slots.contains(&audition_id))
            {
                state.applied_auditions += 1;
            }
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
    if cancellation.load(Ordering::SeqCst) {
        return Err(PlannerError::Interrupted);
    }
    if remaining.is_zero() {
        return Err(PlannerError::TimedOut);
    }
    let request_source = request.to_string();
    let mut response = Vec::new();
    let mut response_too_large = false;
    let mut handle = Easy::new();
    configure_interaction_request(
        &mut handle,
        endpoint,
        api_key,
        request_source.as_bytes(),
        remaining,
    )?;
    let transfer_result = {
        let mut transfer = handle.transfer();
        transfer
            .write_function(|data| {
                let Some(new_length) = response.len().checked_add(data.len()) else {
                    response_too_large = true;
                    return Ok(0);
                };
                if new_length > MAX_INTERACTION_RESPONSE_BYTES {
                    response_too_large = true;
                    return Ok(0);
                }
                response.extend_from_slice(data);
                Ok(data.len())
            })
            .map_err(curl_configuration_error)?;
        transfer
            .progress_function(|_, _, _, _| !cancellation.load(Ordering::SeqCst))
            .map_err(curl_configuration_error)?;
        transfer.perform()
    };
    let recorded_response = String::from_utf8_lossy(&response);
    session
        .record_exchange(exchange_name, request, &recorded_response)
        .map_err(PlannerError::Io)?;
    if let Err(error) = transfer_result {
        if cancellation.load(Ordering::SeqCst) || error.is_aborted_by_callback() {
            return Err(PlannerError::Interrupted);
        }
        if error.is_operation_timedout() {
            return Err(PlannerError::TimedOut);
        }
        if response_too_large {
            return Err(invalid("interaction response exceeded the 32 MiB limit"));
        }
        return Err(PlannerError::Failed {
            message: bounded_text(&error.to_string(), 1_000),
            code: None,
        });
    }
    let status = handle.response_code().map_err(curl_configuration_error)?;
    let response =
        String::from_utf8(response).map_err(|_| invalid("interaction response was not UTF-8"))?;
    if status >= 400 {
        if let Some(error) = serde_json::from_str::<JsonValue>(&response)
            .ok()
            .and_then(|body| body.get("error").cloned())
        {
            return Err(api_failure(&error));
        }
        return Err(PlannerError::Failed {
            message: format!("Gemini API returned HTTP {status}"),
            code: None,
        });
    }
    Ok(response)
}

fn configure_interaction_request(
    handle: &mut Easy,
    endpoint: &str,
    api_key: &str,
    request: &[u8],
    remaining: Duration,
) -> Result<(), PlannerError> {
    let remaining = remaining.max(Duration::from_millis(1));
    handle.url(endpoint).map_err(curl_configuration_error)?;
    handle.post(true).map_err(curl_configuration_error)?;
    handle
        .connect_timeout(remaining.min(Duration::from_secs(15)))
        .map_err(curl_configuration_error)?;
    handle
        .timeout(remaining)
        .map_err(curl_configuration_error)?;
    handle.signal(false).map_err(curl_configuration_error)?;
    handle.progress(true).map_err(curl_configuration_error)?;
    handle
        .post_fields_copy(request)
        .map_err(curl_configuration_error)?;
    let mut headers = List::new();
    headers
        .append("Content-Type: application/json")
        .map_err(curl_configuration_error)?;
    headers
        .append(&format!("x-goog-api-key: {api_key}"))
        .map_err(curl_configuration_error)?;
    handle
        .http_headers(headers)
        .map_err(curl_configuration_error)
}

fn curl_configuration_error(error: curl::Error) -> PlannerError {
    PlannerError::Unavailable(format!(
        "could not configure the Gemini API connection: {error}"
    ))
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
    let source =
        read_bounded_text_following_links(&path, MAX_CREDENTIAL_BYTES, "Gemini credentials")
            .map_err(|error| {
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

fn planner_task(prompt: &str, start: f32, end: f32, new_prompt: bool) -> String {
    if new_prompt {
        return format!(
            concat!(
                "Selected edit region: {start:.3} to {end:.3} seconds. This bounds MIDI arrangement edits, not listening or analysis.\n",
                "User request: {prompt}\n\n",
                "Complete the request in the existing project. Inspect the current graph first, identify the exact material the request refers to, form a concise musical plan before changing the graph, and preserve unrelated material. ",
                "Load the editing tool group needed for the next action, work through studio tools, evaluate the resulting project with explicitly selected audio, and continue until the project itself fulfills the request."
            ),
            start = start,
            end = end,
            prompt = prompt
        );
    }
    format!(
        "Selected edit region: {start:.3} to {end:.3} seconds. This bounds MIDI arrangement edits, not listening.\nUser request: {prompt}\n\nRead the current sound graph, infer the musical intent from the request and existing composition, and form a concise musical plan before changing the graph. Then implement that plan with the studio tools. Use audition slots when exploring sounds, and render the arrangement to listen after its final audible change."
    )
}

fn system_instruction(new_prompt: bool) -> String {
    let mut instruction = if new_prompt {
        format!(
            concat!(
                "You are the autonomous producer inside DAW-AI. Make the user's requested change in the existing project with tools; prose alone changes nothing. ",
                "Choose the musical result yourself from the request and current composition. Contract descriptions and examples explain mechanics, never preferred musical choices. ",
                "{} ",
                "Work from evidence: inspect authoritative graph state before acting, treat follow-up requests as edits to that current state, and preserve material the user did not ask to change. ",
                "Use exact returned IDs and values, load the editing tool group needed for the next action, and update your understanding after every result because rejected calls change nothing. ",
                "Use audition slots when exploring or customizing sounds outside the arrangement. For arrangement evidence, explicitly choose the tracks and range to render. Reason from heard WAV audio for audible claims; use analyze_audio only for objective signal measurements. ",
                "Continue until the actual project state fulfills the request. There is no separate completion reviewer or predetermined tool-call limit; the request timeout is the only loop limit.\n\n{}"
            ),
            MUSICAL_PLANNING_GUIDANCE, STUDIO_CONTRACT
        )
    } else {
        format!(
            concat!(
                "You are the autonomous producer inside DAW-AI. Decide the musical result from the user's request, selected region, and current composition. ",
                "Use registered tools to implement the result; describing an edit does not change the project. Inspect authoritative state before relying on it. ",
                "Use your own musical judgment rather than treating tool descriptions or examples as composition advice. The studio contract explains mechanics and constraints. ",
                "{} ",
                "When audio would resolve uncertainty or verify completion, render it and reason from the WAV itself. Use analyze_audio only for objective signal measurements, not as a substitute for listening or musical judgment. ",
                "You may iterate as needed until the user's request is fulfilled. There is no separate completion reviewer and no predetermined tool-call limit; the request timeout is the only loop limit.\n\n{}"
            ),
            MUSICAL_PLANNING_GUIDANCE, STUDIO_CONTRACT
        )
    };
    instruction.push_str(&format!(
        "\n\nAfter at most {MAX_MUTATIONS_WITHOUT_FEEDBACK} successful project mutations, call {ANALYZE_AUDIO_TOOL_NAME} or {AUDIO_TOOL_NAME}; the checkpoint call must succeed. Before completing, you must successfully call {AUDIO_TOOL_NAME} after the final project mutation and listen to its returned WAV. {ANALYZE_AUDIO_TOOL_NAME} is useful for objective measurements but does not satisfy this final-listen requirement. Isolated audition audio does not satisfy either arrangement requirement."
    ));
    instruction
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

fn applied_steps(state: &LoopState) -> usize {
    state.plans.len()
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
            &call("set_parameter", preset_edit("Factory/Leads/Classic Lead 1")),
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
        let listening_text = baseline.result[0]["text"].as_str().unwrap();
        assert!(!listening_text.contains("Objective measurements"));
        assert!(!listening_text.contains("peakDbfs"));
        let audio_input = &baseline.supplemental_input[0]["content"][1];
        assert_eq!(audio_input["type"], "audio");
        assert_eq!(audio_input["mime_type"], "audio/wav");
        assert!(audio_input["data"].as_str().unwrap().starts_with("UklGR"));

        let analysis = execute_tool(
            &session,
            3,
            &call(
                ANALYZE_AUDIO_TOOL_NAME,
                serde_json::json!({"tracks": [2], "start": 4, "end": 8}),
            ),
            &mut state,
            &mut render_audio,
            &mut |edit| Ok(edit.project),
        )
        .expect("objective analysis");
        assert!(analysis.supplemental_input.is_empty());
        let measurements: JsonValue =
            serde_json::from_str(analysis.result[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(measurements["mix"].as_object().unwrap().len(), 10);
        assert_eq!(state.audio_listens, 1);

        execute_tool(
            &session,
            4,
            &call("set_parameter", preset_edit("Factory/Leads/Classic Lead 1")),
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
            5,
            &call(
                "set_parameter",
                preset_edit("Factory/Polysynths/Anthemish 1"),
            ),
            &mut state,
            &mut render_audio,
            &mut |edit| Ok(edit.project),
        )
        .expect("consecutive edit without audio");

        execute_tool(
            &session,
            6,
            &audio,
            &mut state,
            &mut render_audio,
            &mut |edit| Ok(edit.project),
        )
        .expect("edited audio");
        assert_eq!(state.audio_listens, 2);
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
    fn producer_executes_only_one_returned_tool_call_per_interaction() {
        let session =
            EditSession::create(&Project::demo(), "shape the bass", 0.0, 4.0).expect("session");
        let responses = [
            serde_json::json!({
                "id":"multi","status":"requires_action","steps":[
                    {"type":"function_call","id":"load-sound","name":LOAD_TOOL_GROUP_NAME,
                     "arguments":{"group":"sound"}},
                    {"type":"function_call","id":"tempo-early","name":"set_tempo",
                     "arguments":{"bpm":140}}
                ]
            }),
            serde_json::json!({
                "id":"edit","status":"requires_action","steps":[{
                    "type":"function_call","id":"edit-bass","name":"set_surge_preset",
                    "arguments":{"trackId":2,"presetId":"Factory/Leads/Classic Lead 1"}
                }]
            }),
            serde_json::json!({
                "id":"load","status":"requires_action","steps":[{
                    "type":"function_call","id":"load-arrangement","name":LOAD_TOOL_GROUP_NAME,
                    "arguments":{"group":"arrangement"}
                }]
            }),
            serde_json::json!({
                "id":"tempo","status":"requires_action","steps":[{
                    "type":"function_call","id":"tempo-retry","name":"set_tempo",
                    "arguments":{"bpm":140}
                }]
            }),
            serde_json::json!({
                "id":"listen","status":"requires_action","steps":[{
                    "type":"function_call","id":"listen-final","name":AUDIO_TOOL_NAME,
                    "arguments":{"tracks":"all","start":0,"end":4}
                }]
            }),
            serde_json::json!({"id":"done","status":"completed","steps":[]}),
        ];
        let mut response_index = 0;
        let mut saw_retry_message = false;
        let mut selections = Vec::new();
        let original_bpm = Project::demo().bpm;
        let result = run_session_with_transport(
            &session,
            "shape the bass",
            0.0,
            4.0,
            &mut render_audio_request,
            &mut |edit| {
                selections.push((edit.selection_start, edit.selection_end));
                Ok(edit.project)
            },
            &|| false,
            &mut |sequence, request, _| {
                if sequence == 2 {
                    saw_retry_message = request
                        .to_string()
                        .contains("call one tool at a time; retry this call");
                }
                let response = responses[response_index].to_string();
                response_index += 1;
                Ok(response)
            },
        )
        .expect("producer session");
        assert!(saw_retry_message);
        assert_eq!(result.project.bpm, 140);
        let tempo_scale = f32::from(original_bpm) / 140.0;
        assert_eq!(selections, vec![(0.0, 4.0), (0.0, 4.0 * tempo_scale)]);
        assert_eq!(result.selection_start, 0.0);
        assert_eq!(result.selection_end, 4.0 * tempo_scale);
        assert_eq!(session.stats().unwrap().0, 2);
    }

    #[test]
    fn dynamic_tool_loading_is_always_enforced() {
        let session =
            EditSession::create(&Project::demo(), "change tempo", 0.0, 4.0).expect("session");
        let responses = [
            serde_json::json!({
                "id":"stale","status":"requires_action","steps":[{
                    "type":"function_call","id":"tempo-stale","name":"set_tempo",
                    "arguments":{"bpm":140}
                }]
            }),
            serde_json::json!({
                "id":"load","status":"requires_action","steps":[{
                    "type":"function_call","id":"load-arrangement","name":LOAD_TOOL_GROUP_NAME,
                    "arguments":{"group":"arrangement"}
                }]
            }),
            serde_json::json!({
                "id":"tempo","status":"requires_action","steps":[{
                    "type":"function_call","id":"tempo-loaded","name":"set_tempo",
                    "arguments":{"bpm":140}
                }]
            }),
            serde_json::json!({
                "id":"listen","status":"requires_action","steps":[{
                    "type":"function_call","id":"listen-final","name":AUDIO_TOOL_NAME,
                    "arguments":{"tracks":"all","start":0,"end":4}
                }]
            }),
            serde_json::json!({"id":"done","status":"completed","steps":[]}),
        ];
        let mut response_index = 0;
        let mut rejected_stale_call = false;
        let mut load_reported_available_tools = false;
        let result = run_session_with_transport_options(
            &session,
            "change tempo",
            0.0,
            4.0,
            SessionOptions {
                new_prompt: false,
                deadline: Instant::now() + EDIT_TIMEOUT,
            },
            &mut render_audio_request,
            &mut |edit| Ok(edit.project),
            &|| false,
            &mut |sequence, request, _| {
                if sequence == 2 {
                    let request = request.to_string();
                    rejected_stale_call = request.contains("not currently available")
                        && request.contains(r#"load_tool_group with {\"group\":\"arrangement\"}"#);
                }
                if sequence == 3 {
                    let request = request.to_string();
                    load_reported_available_tools =
                        request.contains("availableTools") && request.contains("add_midi_clip");
                }
                let response = responses[response_index].to_string();
                response_index += 1;
                Ok(response)
            },
        )
        .expect("dynamic tool session");

        assert!(rejected_stale_call);
        assert!(load_reported_available_tools);
        assert_eq!(result.project.bpm, 140);
        assert_eq!(session.stats().unwrap().0, 1);
    }

    #[test]
    fn batch_parameter_tools_are_available_in_the_sound_group() {
        let session =
            EditSession::create(&Project::demo(), "change tempo", 0.0, 4.0).expect("session");
        let responses = [
            serde_json::json!({
                "id":"load","status":"requires_action","steps":[{
                    "type":"function_call","id":"load-sound","name":LOAD_TOOL_GROUP_NAME,
                    "arguments":{"group":"sound"}
                }]
            }),
            serde_json::json!({
                "id":"batch","status":"requires_action","steps":[{
                    "type":"function_call","id":"batch-edit",
                    "name":"set_instrument_parameters",
                    "arguments":{"trackId":2,"changes":[{"parameter":"native:264","value":"0.4"}]}
                }]
            }),
            serde_json::json!({
                "id":"listen","status":"requires_action","steps":[{
                    "type":"function_call","id":"listen-final","name":AUDIO_TOOL_NAME,
                    "arguments":{"tracks":[2],"start":0,"end":4}
                }]
            }),
            serde_json::json!({"id":"done","status":"completed","steps":[]}),
        ];
        let mut response_index = 0;
        let result = run_session_with_transport_options(
            &session,
            "change tempo",
            0.0,
            4.0,
            SessionOptions {
                new_prompt: false,
                deadline: Instant::now() + EDIT_TIMEOUT,
            },
            &mut render_audio_request,
            &mut |edit| Ok(edit.project),
            &|| false,
            &mut |_, _, _| {
                let response = responses[response_index].to_string();
                response_index += 1;
                Ok(response)
            },
        )
        .expect("standard batch session");

        assert_eq!(
            result.project.tracks[1].instrument.native_overrides[&264],
            0.4
        );
        assert_eq!(session.stats().unwrap().0, 1);
    }

    #[test]
    fn arrangement_feedback_is_always_enforced_periodically_and_before_completion() {
        let session =
            EditSession::create(&Project::demo(), "shape the mix", 0.0, 4.0).expect("session");
        let volume_call = |id: &str, volume: f64| {
            serde_json::json!({
                "id":id,"status":"requires_action","steps":[{
                    "type":"function_call","id":format!("{id}-call"),"name":"set_track_volume",
                    "arguments":{"trackId":2,"volume":volume}
                }]
            })
        };
        let responses = [
            serde_json::json!({
                "id":"load","status":"requires_action","steps":[{
                    "type":"function_call","id":"load-arrangement","name":LOAD_TOOL_GROUP_NAME,
                    "arguments":{"group":"arrangement"}
                }]
            }),
            volume_call("edit-1", 0.9),
            volume_call("edit-2", 0.8),
            volume_call("edit-3", 0.7),
            volume_call("edit-4", 0.6),
            volume_call("blocked-edit-5", 0.5),
            serde_json::json!({
                "id":"analysis","status":"requires_action","steps":[{
                    "type":"function_call","id":"analysis-call","name":ANALYZE_AUDIO_TOOL_NAME,
                    "arguments":{"tracks":[2],"start":0,"end":1}
                }]
            }),
            volume_call("edit-5", 0.5),
            serde_json::json!({"id":"premature-done","status":"completed","steps":[]}),
            serde_json::json!({
                "id":"listen","status":"requires_action","steps":[{
                    "type":"function_call","id":"listen-call","name":AUDIO_TOOL_NAME,
                    "arguments":{"tracks":[2],"start":0,"end":1}
                }]
            }),
            serde_json::json!({"id":"done","status":"completed","steps":[]}),
        ];
        let mut response_index = 0;
        let mut updates = 0;
        let mut renders = 0;
        let mut periodic_enforcement_reported = false;
        let mut completion_enforcement_reported = false;
        let result = run_session_with_transport_options(
            &session,
            "shape the mix",
            0.0,
            4.0,
            SessionOptions {
                new_prompt: false,
                deadline: Instant::now() + EDIT_TIMEOUT,
            },
            &mut |request| {
                renders += 1;
                render_audio_request(request)
            },
            &mut |edit| {
                updates += 1;
                Ok(edit.project)
            },
            &|| false,
            &mut |sequence, request, _| {
                if sequence == 1 {
                    assert!(
                        request["system_instruction"]
                            .as_str()
                            .is_some_and(|instruction| instruction
                                .contains("After at most 4 successful project mutations"))
                    );
                }
                if sequence == 7 {
                    periodic_enforcement_reported = request
                        .to_string()
                        .contains("at most 4 successful project mutations");
                }
                if sequence == 10 {
                    completion_enforcement_reported =
                        request.to_string().contains("Before completing");
                }
                let response = responses[response_index].to_string();
                response_index += 1;
                Ok(response)
            },
        )
        .expect("arrangement-feedback session");

        assert!(periodic_enforcement_reported);
        assert!(completion_enforcement_reported);
        assert_eq!(response_index, responses.len());
        assert_eq!(updates, 5);
        assert_eq!(renders, 2);
        assert!((result.project.tracks[1].volume - 0.5).abs() < f32::EPSILON);
        assert_eq!(session.stats().unwrap(), (5, 1));
        let metadata: JsonValue =
            serde_json::from_str(&session.metadata_source().unwrap()).expect("session metadata");
        assert_eq!(metadata["metrics"]["failedToolCalls"], 1);
        assert_eq!(metadata["metrics"]["toolCalls"][ANALYZE_AUDIO_TOOL_NAME], 1);
        assert_eq!(metadata["metrics"]["toolCalls"][AUDIO_TOOL_NAME], 1);
    }

    #[test]
    fn producer_can_finish_immediately_after_listening() {
        let session =
            EditSession::create(&Project::demo(), "shape the bass", 0.0, 4.0).expect("session");
        let responses = [
            serde_json::json!({
                "id": "load", "status": "requires_action", "steps": [{
                    "type": "function_call", "id": "load-sound", "name": LOAD_TOOL_GROUP_NAME,
                    "arguments": {"group": "sound"}
                }]
            }),
            serde_json::json!({
                "id": "edit", "status": "requires_action", "steps": [{
                    "type": "function_call", "id": "edit-bass", "name": "set_surge_preset",
                    "arguments": {"trackId": 2, "presetId": "Factory/Leads/Classic Lead 1"}
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
        assert_eq!(
            result.project.tracks[1].instrument.preset,
            "Factory/Leads/Classic Lead 1"
        );
        assert_eq!(session.stats().unwrap(), (1, 1));
        let metadata: JsonValue =
            serde_json::from_str(&session.metadata_source().unwrap()).expect("session metadata");
        assert_eq!(metadata["metrics"]["totalToolCalls"], 3);
        assert_eq!(metadata["metrics"]["toolCalls"][AUDIO_TOOL_NAME], 1);
        assert_eq!(metadata["metrics"]["mutationsBeforeFirstListen"], 1);
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
    fn failed_interaction_transport_records_the_attempt() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let endpoint = format!(
            "http://{}/",
            listener.local_addr().expect("fixture address")
        );
        let fixture = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fixture connection");
            drop(stream);
        });
        let session =
            EditSession::create(&Project::demo(), "transport failure", 0.0, 1.0).expect("session");
        let result = call_interactions(
            &session,
            "connection-failure",
            &serde_json::json!({"model": GEMINI_MODEL}),
            "test-key",
            &endpoint,
            Duration::from_secs(1),
            &Arc::new(AtomicBool::new(false)),
        );
        fixture.join().expect("fixture thread");

        assert!(matches!(result, Err(PlannerError::Failed { .. })));
        let recorded_request =
            std::fs::read_to_string(session.path().join("connection-failure-request.json"))
                .expect("recorded failed request");
        let recorded_response =
            std::fs::read_to_string(session.path().join("connection-failure-response.json"))
                .expect("recorded failed response");
        assert_eq!(
            serde_json::from_str::<JsonValue>(&recorded_request).expect("request JSON")["model"],
            GEMINI_MODEL
        );
        assert!(recorded_response.trim().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interaction_transport_posts_json_with_api_authentication() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("fixture connection");
            let service = hyper::service::service_fn(
                |request: http::Request<hyper::body::Incoming>| async move {
                    use http_body_util::BodyExt;

                    let authenticated = request
                        .headers()
                        .get("x-goog-api-key")
                        .is_some_and(|value| value == "test-key");
                    let body = request.into_body().collect().await?.to_bytes();
                    let json = serde_json::from_slice::<JsonValue>(&body).ok();
                    let (status, body) = if authenticated
                        && json.as_ref().and_then(|value| value.get("model")).is_some()
                    {
                        (
                            http::StatusCode::OK,
                            r#"{"id":"transport-ok","status":"completed"}"#,
                        )
                    } else {
                        (http::StatusCode::BAD_REQUEST, r#"{"error":{}}"#)
                    };
                    let response = http::Response::builder()
                        .status(status)
                        .body(http_body_util::Full::new(hyper::body::Bytes::from_static(
                            body.as_bytes(),
                        )))
                        .expect("fixture response");
                    Ok::<_, hyper::Error>(response)
                },
            );
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.keep_alive(false);
            builder
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await
        });
        let session =
            EditSession::create(&Project::demo(), "transport", 0.0, 1.0).expect("session");
        let response = tokio::task::spawn_blocking(move || {
            call_interactions(
                &session,
                "transport",
                &serde_json::json!({"model": GEMINI_MODEL}),
                "test-key",
                &format!("http://{address}/"),
                Duration::from_secs(5),
                &Arc::new(AtomicBool::new(false)),
            )
        })
        .await
        .expect("interaction worker")
        .expect("interaction response");
        server.await.expect("fixture task").expect("fixture server");

        assert_eq!(
            serde_json::from_str::<JsonValue>(&response).expect("response JSON")["id"],
            "transport-ok"
        );
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
    fn gemini_prompt_explains_mechanics_without_composition_prescriptions() {
        let task = planner_task("make the bass hit harder", 4.0, 8.0, false);
        let instruction = system_instruction(false);
        assert!(task.contains("infer the musical intent"));
        assert!(task.contains("form a concise musical plan before changing the graph"));
        assert!(task.contains("audition slots"));
        assert!(instruction.contains("own musical judgment"));
        assert!(instruction.contains("mechanics and constraints"));
        assert!(instruction.contains("Before the first sound-graph mutation"));
        assert!(instruction.contains("rhythm, melody, and harmony map to MIDI"));
        assert!(instruction.contains("register and layering map to tracks and Rack key zones"));
        assert!(instruction.contains("timbre and articulation"));
        assert!(instruction.contains("planned shorter articulation"));
        assert!(instruction.contains("Instrument Rack"));
        assert!(
            instruction.contains("same Instrument, that Instrument receives the note only once")
        );
        assert!(instruction.contains("mutable, session-scoped sound state"));
        assert!(instruction.contains("analyze_audio"));
        assert!(instruction.contains("reason from the WAV itself"));
        assert!(!instruction.contains("Default drums"));
        assert!(!instruction.contains("rhythmic subdivision"));
        assert!(!instruction.contains("energy contour"));
        assert!(instruction.contains("no separate completion reviewer"));
        assert!(instruction.contains("no predetermined tool-call limit"));
        assert!(instruction.contains("After at most 4 successful project mutations"));
        assert!(instruction.contains(ANALYZE_AUDIO_TOOL_NAME));
        assert!(instruction.contains(AUDIO_TOOL_NAME));
        assert!(instruction.contains("does not satisfy this final-listen requirement"));
        let new_instruction = system_instruction(true);
        assert!(new_instruction.contains("examples explain mechanics"));
        assert!(new_instruction.contains("follow-up requests as edits to that current state"));
        assert!(new_instruction.contains("preserve material the user did not ask to change"));
        assert!(new_instruction.contains("explicitly choose the tracks and range to render"));
        assert!(new_instruction.contains("create_audition_slot"));
        let new_task = planner_task("make the bass hit harder", 4.0, 8.0, true);
        assert!(new_task.contains("User request: make the bass hit harder"));
        assert!(new_task.contains("Inspect the current graph first"));
        assert!(new_task.contains("form a concise musical plan before changing the graph"));
        assert!(new_task.contains("preserve unrelated material"));
        assert!(new_task.contains("Load the editing tool group"));
    }

    #[test]
    fn final_arrangement_listen_must_cover_the_latest_mutation() {
        let project = Project::demo();
        let requirement = arrangement_listen_requirement(
            "set_track_volume",
            &serde_json::json!({"trackId":2,"volume":0.8}),
            "{}",
            &project,
            &project,
            24.0,
            32.0,
        );
        assert_eq!(requirement.track_ids, BTreeSet::from([2]));
        let mut state = LoopState::default();
        state.record_project_mutation(requirement);
        state.record_arrangement_feedback();
        assert!(!state.periodic_feedback_required());
        assert!(state.completion_listen_required());

        let unrelated_track = AudioRenderRequest {
            project: project.clone(),
            track_ids: vec![1],
            start: 24.0,
            end: 25.0,
            description: String::new(),
            require_audible_output: false,
        };
        let covers = state.arrangement_render_covers_latest_mutation(&unrelated_track);
        assert!(!covers);
        state.record_arrangement_listen(covers);
        assert!(state.completion_listen_required());

        let unrelated_time = AudioRenderRequest {
            project: project.clone(),
            track_ids: vec![2],
            start: 0.0,
            end: 1.0,
            description: String::new(),
            require_audible_output: false,
        };
        let covers = state.arrangement_render_covers_latest_mutation(&unrelated_time);
        assert!(!covers);
        state.record_arrangement_listen(covers);
        assert!(state.completion_listen_required());

        let relevant = AudioRenderRequest {
            project,
            track_ids: vec![2],
            start: 24.0,
            end: 25.0,
            description: String::new(),
            require_audible_output: false,
        };
        let covers = state.arrangement_render_covers_latest_mutation(&relevant);
        assert!(covers);
        state.record_arrangement_listen(covers);
        assert!(!state.completion_listen_required());
    }

    #[test]
    fn deleted_clips_and_zones_keep_their_prior_listen_scope() {
        let mut before_clip = Project::demo();
        let clip = before_clip
            .clips
            .iter_mut()
            .find(|clip| clip.id == 12)
            .expect("bass clip");
        clip.start = 24.0;
        clip.end = 32.0;
        let mut after_clip = before_clip.clone();
        after_clip.clips.retain(|clip| clip.id != 12);
        let clip_requirement = arrangement_listen_requirement(
            "delete_midi_clip",
            &serde_json::json!({"clipId":12}),
            "{}",
            &before_clip,
            &after_clip,
            0.0,
            32.0,
        );
        assert_eq!((clip_requirement.start, clip_requirement.end), (24.0, 32.0));
        assert_eq!(clip_requirement.track_ids, BTreeSet::from([2]));

        let before_zone = Project::demo();
        let mut after_zone = before_zone.clone();
        after_zone.key_zones.retain(|zone| zone.id != 202);
        let zone_requirement = arrangement_listen_requirement(
            "delete_key_zone",
            &serde_json::json!({"keyZoneId":202}),
            "{}",
            &before_zone,
            &after_zone,
            8.0,
            12.0,
        );
        assert_eq!((zone_requirement.start, zone_requirement.end), (8.0, 12.0));
        assert_eq!(zone_requirement.track_ids, BTreeSet::from([2]));
    }

    #[test]
    fn clip_and_zone_updates_require_every_pre_and_post_recipient() {
        let before_clip = Project::demo();
        let mut after_clip = before_clip.clone();
        for event in &mut after_clip
            .clips
            .iter_mut()
            .find(|clip| clip.id == 12)
            .expect("bass clip")
            .events
        {
            event.pitch = 60;
        }
        let clip_result = serde_json::json!({
            "id":12,
            "clipTiming":{"startSeconds":0,"endSeconds":32}
        })
        .to_string();
        let clip_requirement = arrangement_listen_requirement(
            "update_midi_clip",
            &serde_json::json!({"clipId":12}),
            &clip_result,
            &before_clip,
            &after_clip,
            0.0,
            32.0,
        );
        assert_eq!(clip_requirement.track_ids, BTreeSet::from([2, 3]));

        let before_zone = Project::demo();
        let mut after_zone = before_zone.clone();
        let new_instrument_id = after_zone.tracks[2].instrument.id;
        after_zone
            .key_zones
            .iter_mut()
            .find(|zone| zone.id == 202)
            .expect("bass zone")
            .instrument_id = new_instrument_id;
        let zone_requirement = arrangement_listen_requirement(
            "update_key_zone",
            &serde_json::json!({
                "keyZoneId":202,
                "instrumentId":new_instrument_id
            }),
            "{}",
            &before_zone,
            &after_zone,
            8.0,
            12.0,
        );
        assert_eq!(zone_requirement.track_ids, BTreeSet::from([2, 3]));

        let partial_render = AudioRenderRequest {
            project: after_zone.clone(),
            track_ids: vec![3],
            start: 8.0,
            end: 12.0,
            description: String::new(),
            require_audible_output: false,
        };
        assert!(!zone_requirement.covers(&partial_render));
        let complete_render = AudioRenderRequest {
            project: after_zone,
            track_ids: vec![2, 3],
            start: 8.0,
            end: 12.0,
            description: String::new(),
            require_audible_output: false,
        };
        assert!(zone_requirement.covers(&complete_render));
    }

    #[test]
    fn track_deletion_requires_the_full_remaining_mix() {
        let before = Project::demo();
        let mut after = before.clone();
        let deleted_instrument_id = after.tracks[1].instrument.id;
        after.tracks.retain(|track| track.id != 2);
        after
            .key_zones
            .retain(|zone| zone.instrument_id != deleted_instrument_id);
        let requirement = arrangement_listen_requirement(
            "delete_track",
            &serde_json::json!({"trackId":2}),
            "{}",
            &before,
            &after,
            0.0,
            4.0,
        );
        assert_eq!(requirement.track_ids, BTreeSet::from([1, 3]));

        let partial_render = AudioRenderRequest {
            project: after.clone(),
            track_ids: vec![1],
            start: 0.0,
            end: 4.0,
            description: String::new(),
            require_audible_output: false,
        };
        assert!(!requirement.covers(&partial_render));
        let full_mix = AudioRenderRequest {
            project: after,
            track_ids: vec![1, 3],
            start: 0.0,
            end: 4.0,
            description: String::new(),
            require_audible_output: false,
        };
        assert!(requirement.covers(&full_mix));
    }

    #[test]
    fn session_metrics_capture_cost_and_iteration_behavior() {
        let mut state = LoopState::default();
        state.record_usage(&serde_json::json!({"usage":{
            "total_input_tokens":1200,
            "total_output_tokens":80,
            "total_thought_tokens":240
        }}));
        state.record_call("set_instrument_parameter");
        state.record_mutation();
        state.record_mutation();
        state.record_listen();
        state.record_mutation();
        state.record_listen();
        state.failed_tool_calls = 1;
        state.auditions = 2;
        state.applied_auditions = 1;
        let metrics = state.metrics(Duration::from_millis(1500));
        assert_eq!(metrics["durationMs"], 1500);
        assert_eq!(metrics["inputTokens"], 1200);
        assert_eq!(metrics["thoughtTokens"], 240);
        assert_eq!(metrics["toolCalls"]["set_instrument_parameter"], 1);
        assert_eq!(metrics["failedToolCalls"], 1);
        assert_eq!(metrics["mutationsBeforeFirstListen"], 2);
        assert_eq!(metrics["averageMutationsBetweenListens"], 1.0);
        assert_eq!(metrics["maxMutationsBetweenListens"], 1);
        assert_eq!(metrics["auditionApplyRate"], 0.5);
    }
}
