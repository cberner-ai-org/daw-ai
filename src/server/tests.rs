use super::*;
use crate::http::valid_user_id;
use crate::http::{AUDIO_REQUEST_HEADER, authority};
use crate::model::Project;
use crate::project_history::{ProjectHistory, open_project_with_history};
use crate::storage::ProjectStore;
use http::Request as HttpRequest;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_FILE_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn request(method: &str, path: &str, body: &str) -> Request {
    Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers: HashMap::from([("host".to_owned(), "127.0.0.1:8888".to_owned())]),
        body: body.to_owned(),
    }
}

fn framework_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Request, String> {
    let mut builder = HttpRequest::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(()).expect("framework request");
    let (parts, ()) = request.into_parts();
    Request::from_http(parts, body)
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
    (open_persisted_router(path.clone()), path)
}

fn open_persisted_router(path: std::path::PathBuf) -> Router {
    let project = ProjectState::open(path).expect("open persisted test project");
    Router {
        project: Arc::new(Mutex::new(project)),
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

#[test]
fn recovers_an_invalid_persisted_project_and_preserves_the_source() {
    let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "daw-ai-server-invalid-{}-{id}.json",
        std::process::id()
    ));
    fs::write(&path, "{not json}\n").expect("invalid graph fixture");

    let (_, studio, history) = open_project_with_history(path.clone()).expect("recovered project");

    assert_eq!(history.len(), 1);
    assert_eq!(
        history
            .checkout(0, studio.project())
            .expect("initial history state")
            .1
            .to_json(),
        studio.project().to_json()
    );
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
fn reset_is_undoable_without_losing_edit_idempotency() {
    let (router, path) = persisted_demo();
    let edit_body = "operation_id=reset-operation&start=4&end=8&prompt=increase+volume";
    let accepted = router.handle(&request("POST", "/api/edits", edit_body));
    let completed = wait_for_edit(&router, &accepted);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["operationId"], "reset-operation");
    let original: serde_json::Value =
        serde_json::from_str(&router.handle(&request("GET", "/api/project", "")).body)
            .expect("initial project response");
    assert_eq!(original["edits"].as_array().map(Vec::len), Some(1));
    assert_eq!(original["editOperations"].as_array().map(Vec::len), Some(1));

    let reset = router.handle(&request("POST", "/api/reset", ""));
    assert_eq!(reset.status, 200);
    let reset: serde_json::Value = serde_json::from_str(&reset.body).expect("reset response");
    assert_eq!(reset["tracks"].as_array().map(Vec::len), Some(1));
    assert_eq!(reset["clips"].as_array().map(Vec::len), Some(0));
    assert_eq!(reset["edits"].as_array().map(Vec::len), Some(0));
    assert_eq!(reset["editOperations"], original["editOperations"]);
    assert_eq!(reset["canUndo"], true);

    drop(router);
    let reloaded = open_persisted_router(path.clone());
    let persisted_reset: serde_json::Value =
        serde_json::from_str(&reloaded.handle(&request("GET", "/api/project", "")).body)
            .expect("persisted reset project");
    assert_eq!(persisted_reset["tracks"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        persisted_reset["editOperations"],
        original["editOperations"]
    );

    let undone = reloaded.handle(&request("POST", "/api/undo", ""));
    assert_eq!(undone.status, 200);
    let undone: serde_json::Value = serde_json::from_str(&undone.body).expect("undo response");
    assert_eq!(undone["canUndo"], true);
    for field in [
        "name",
        "bpm",
        "duration",
        "instrumentRacks",
        "clips",
        "tracks",
        "edits",
        "editOperations",
    ] {
        assert_eq!(undone[field], original[field], "restored {field}");
    }

    drop(reloaded);
    let restarted = open_persisted_router(path.clone());
    let recovered = restarted.handle(&request("POST", "/api/edits", edit_body));
    assert_eq!(recovered.status, 200);
    let recovered: serde_json::Value =
        serde_json::from_str(&recovered.body).expect("recovered operation response");
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["operationId"], "reset-operation");
    assert_eq!(
        restarted.lock_project().project().edits.len(),
        1,
        "recovering a completed operation must not queue another edit"
    );
    fs::remove_file(path).expect("remove test graph");
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
fn history_checkout_requires_a_trusted_origin() {
    let router = Router::demo();
    let mut hostile = request("POST", "/api/history", "index=0");
    hostile
        .headers
        .insert("origin".to_owned(), "http://attacker.invalid".to_owned());

    assert_eq!(router.handle(&hostile).status, 403);
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
fn exhausted_process_capacity_does_not_publish_an_edit_job() {
    let mut router = Router::demo();
    let limiter = Limiter::new(1);
    let held_permit = limiter.acquire().expect("held edit permit");
    router.edit_limiter = limiter;
    let body = "operation_id=capacity-operation&start=4&end=8&prompt=increase+volume";

    let rejected = router.handle(&request("POST", "/api/edits", body));
    assert_eq!(rejected.status, 503);
    assert!(
        router
            .edit_jobs
            .response_for_operation("capacity-operation")
            .is_none()
    );

    drop(held_permit);
    let accepted = router.handle(&request("POST", "/api/edits", body));
    assert_eq!(accepted.status, 202);
    let completed = wait_for_edit(&router, &accepted);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["operationId"], "capacity-operation");
}

#[test]
fn converts_framework_requests_and_decodes_forms() {
    let body = "prompt=warm+%26+wide&start=0&end=4";
    let parsed = framework_request(
        "POST",
        "/api/edits?source=test",
        &[("host", "localhost")],
        body.as_bytes(),
    )
    .expect("valid request");
    assert_eq!(parsed.path, "/api/edits");
    assert_eq!(parsed.headers["host"], "localhost");
    assert_eq!(parse_form(&parsed.body)["prompt"], "warm & wide");
}

#[test]
fn rejects_ambiguous_unbounded_or_absolute_framework_requests() {
    assert!(
        framework_request(
            "POST",
            "/api/logs",
            &[("content-length", "0"), ("content-length", "1")],
            b"",
        )
        .is_err()
    );
    assert!(framework_request("GET", "http://localhost/", &[("host", "localhost")], b"",).is_err());
    let oversized = "x".repeat(crate::http::MAX_REQUEST_HEADER_BYTES);
    assert!(
        framework_request(
            "GET",
            "/",
            &[("host", "localhost"), ("x-fill", &oversized)],
            b""
        )
        .is_err()
    );
}

#[test]
fn validates_the_optional_prompt_experiment() {
    assert!(!parse_optional_boolean(&HashMap::new(), "new_prompt").unwrap());
    assert!(parse_optional_boolean(&parse_form("new_prompt=true"), "new_prompt").unwrap());
    assert_eq!(
        parse_optional_boolean(&parse_form("new_prompt=enabled"), "new_prompt").unwrap_err(),
        "boolean setting must be true or false"
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
    let mut project = router.lock_project().project().clone();
    project.duration = audio_analysis::MAX_REGION_SECONDS + 0.125;
    project.bpm = 113;
    project.tracks.truncate(1);
    router.lock_project().replace_project_for_test(project);
    let access = router.handle(&audio_request("/api/audio-access"));
    assert_eq!(access.status, 200);
    let access: serde_json::Value = serde_json::from_str(&access.body).expect("audio access JSON");
    assert_eq!(access["streamToken"], "test-audio-token");

    let version = router.lock_project().project().version;
    let stream_request = request(
        "GET",
        &format!("/api/audio-stream/test-audio-token/{version}/0"),
        "",
    );
    let plan = router
        .playback_plan(&stream_request)
        .expect("continuous playback plan");
    assert!(matches!(plan.selection, PlaybackSelection::Full));
    let mut body = Vec::new();
    router
        .write_playback_body(&plan, &mut body, &|| false, PlaybackPacing::Unpaced)
        .expect("continuous WAV stream");
    let expected_samples =
        audio_analysis::playback_sample_count(0.0, router.lock_project().project().duration);

    assert_eq!(plan.total_length, body.len());
    assert_eq!(body.len(), 44 + expected_samples * 4);
    assert_eq!(&body[..4], b"RIFF");
    assert_eq!(&body[8..12], b"WAVE");
    assert_eq!(
        u32::from_le_bytes(body[40..44].try_into().expect("WAV data length")) as usize,
        expected_samples * 4
    );
    let render_boundary =
        44 + (audio_analysis::MAX_REGION_SECONDS * audio_analysis::SAMPLE_RATE as f32) as usize * 4;
    assert_ne!(&body[render_boundary..render_boundary + 4], b"RIFF");

    let rejected = router
        .playback_plan(&request(
            "GET",
            &format!("/api/audio-stream/wrong-token/{version}/0"),
            "",
        ))
        .expect_err("wrong token must be rejected");
    assert_eq!(rejected.status, 403);
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
fn obsolete_spectrum_cleanup_preserves_other_files_and_directories() {
    let root = std::env::temp_dir().join(format!(
        "daw-ai-cache-cleanup-{}-{}",
        std::process::id(),
        crate::storage::unique_test_id()
    ));
    fs::create_dir(&root).expect("cache directory");
    let project_path = root.join("sound-graph.json");
    let preserved = [
        "sound-graph.json",
        ".sound-graph.json.track-spectrum.cache",
        ".other.json.track-spectrum-0-48000.cache",
        ".sound-graph.json.track-spectrum-not-a-range.cache",
        ".sound-graph.json.track-spectrum-0-48000.cache.txt",
    ];
    for name in preserved {
        fs::write(root.join(name), "preserved").expect("unrelated file");
    }
    let obsolete = [
        ".sound-graph.json.track-spectrum-0-3072000.cache",
        ".sound-graph.json.track-spectrum-1500-48000.cache",
    ];
    for name in obsolete {
        fs::write(root.join(name), "obsolete").expect("obsolete cache");
    }
    let directory = root.join(".sound-graph.json.track-spectrum-1-2.cache");
    fs::create_dir(&directory).expect("unrelated directory");
    #[cfg(unix)]
    let link = {
        let link = root.join(".sound-graph.json.track-spectrum-3-4.cache");
        std::os::unix::fs::symlink(&project_path, &link).expect("unrelated symbolic link");
        link
    };

    discard_obsolete_spectrum_caches(&project_path);
    discard_obsolete_spectrum_caches(&project_path);

    for name in obsolete {
        assert!(!root.join(name).exists());
    }
    for name in preserved {
        assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "preserved");
    }
    assert!(directory.is_dir());
    #[cfg(unix)]
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    fs::remove_dir_all(root).expect("remove cache fixture");

    let basename = format!(
        "daw-ai-relative-cache-{}-{}.json",
        std::process::id(),
        crate::storage::unique_test_id()
    );
    let cache_path = PathBuf::from(format!(".{basename}.track-spectrum-0-48000.cache"));
    fs::write(&cache_path, "obsolete").expect("relative cache fixture");
    discard_obsolete_spectrum_caches(std::path::Path::new(&basename));
    let removed = !cache_path.exists();
    if !removed {
        fs::remove_file(cache_path).expect("remove failed fixture");
    }
    assert!(
        removed,
        "a bare project filename uses the working directory"
    );
}

#[test]
fn persisted_spectrum_is_served_without_entering_the_renderer() {
    let (router, project_path) = persisted_demo();
    let mut project = router.lock_project().project().clone();
    project.duration = 0.25;
    router
        .lock_project()
        .replace_project_for_test(project.clone());
    let path = format!("/api/track-spectrum/test-audio-token/{}/0", project.version);
    let first = router
        .track_spectrum_data(&request("GET", &path, ""), || false)
        .expect("cold spectrum request")
        .expect("cold spectrum response")
        .body;
    let cache_path = router.spectrum_cache_path().expect("spectrum cache path");
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
        cold_router.track_spectrum_data(&request("GET", &cold_path, ""), || false)
    });
    router
        .audio_renderer
        .wait_until_queued_for_test(AudioRenderPriority::Background);
    let second = router
        .track_spectrum_data(&request("GET", &path, ""), || false)
        .expect("cached spectrum request")
        .expect("cached spectrum response")
        .body;
    assert_eq!(first, second);

    router.audio_renderer.release_for_test();
    cold.join()
        .expect("cold spectrum worker")
        .expect("unrelated cold spectrum request")
        .expect("unrelated cold spectrum response");

    assert!(matches!(
        router.prepare_track_spectrum(&request("GET", &path, "")),
        Ok(TrackSpectrumPreparation::Render(_))
    ));
    // Equal frame counts must not alias different positions or end samples.
    for suffix in ["0/30", "1/30", "1/31"] {
        let path = format!(
            "/api/track-spectrum/test-audio-token/{}/{suffix}",
            project.version
        );
        assert!(matches!(
            router.prepare_track_spectrum(&request("GET", &path, "")),
            Ok(TrackSpectrumPreparation::Render(_))
        ));
        router
            .track_spectrum_data(&request("GET", &path, ""), || false)
            .expect("render replacement window")
            .expect("replacement response");
        assert!(matches!(
            router.prepare_track_spectrum(&request("GET", &path, "")),
            Ok(TrackSpectrumPreparation::Ready(_))
        ));
    }
    let prefix = format!(
        ".{}.track-spectrum",
        project_path.file_name().unwrap().to_string_lossy()
    );
    let cache_count = fs::read_dir(project_path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .count();
    assert_eq!(cache_count, 1);
    project.version += 1;
    router
        .lock_project()
        .replace_project_for_test(project.clone());
    let path = format!(
        "/api/track-spectrum/test-audio-token/{}/1/31",
        project.version
    );
    assert!(matches!(
        router.prepare_track_spectrum(&request("GET", &path, "")),
        Ok(TrackSpectrumPreparation::Render(_))
    ));
    fs::remove_file(cache_path).expect("remove spectrum cache");
    fs::remove_file(project_path).expect("remove project fixture");
}

#[test]
fn export_streams_wav_bytes() {
    let router = Router::demo();
    let mut project = router.lock_project().project().clone();
    project.duration = 0.5;
    router.lock_project().replace_project_for_test(project);
    let project = router.lock_project().project().clone();
    let expected_samples = audio_analysis::playback_sample_count(0.0, 0.5);
    let mut body = Vec::new();
    router
        .write_export_body(&project, expected_samples, &mut body, &|| false)
        .expect("streamed export");

    assert_eq!(body.len(), WAV_HEADER_BYTES + expected_samples * 4);
    assert_eq!(&body[..4], b"RIFF");
}

#[test]
fn serves_bounded_byte_ranges_for_maximum_wav_audio() {
    let router = Router::demo();
    let mut project = router.lock_project().project().clone();
    project.duration = audio_analysis::MAX_WAV_SECONDS;
    router.lock_project().replace_project_for_test(project);
    let version = router.lock_project().project().version;
    let mut range_request = request(
        "GET",
        &format!("/api/audio-stream/test-audio-token/{version}/0"),
        "",
    );
    range_request
        .headers
        .insert("range".to_owned(), "bytes=0-99".to_owned());
    let plan = router
        .playback_plan(&range_request)
        .expect("partial playback plan");
    let total_length = WAV_HEADER_BYTES
        + audio_analysis::playback_sample_count(0.0, audio_analysis::MAX_WAV_SECONDS) * 4;
    assert_eq!(plan.total_length, total_length);
    let PlaybackSelection::Range(range) = plan.selection else {
        panic!("expected a byte range");
    };
    assert_eq!(range, ByteRange { start: 0, end: 99 });
    let mut body = Vec::new();
    router
        .write_playback_byte_range(
            &plan.project,
            plan.stream_start_sample,
            plan.sample_count,
            range,
            &mut body,
            &|| false,
        )
        .expect("partial WAV response");
    assert_eq!(body.len(), 100);
    assert_eq!(&body[..4], b"RIFF");

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
fn queued_gemini_render_stops_at_the_edit_deadline() {
    let router = Router::demo();
    router.audio_renderer.occupy_for_test();
    let (job_id, _, _) = router
        .edit_jobs
        .create(TEST_AI_POLL_INTERVAL_MS, Some("render-deadline"))
        .expect("edit job");
    let cancellation = router.edit_jobs.cancellation(job_id);
    let request = crate::gemini_tools::AudioRenderRequest {
        project: Project::demo(),
        track_ids: Vec::new(),
        start: 0.0,
        end: 1.0,
        description: "deadline test".to_owned(),
        require_audible_output: false,
    };
    let deadline = Instant::now() + Duration::from_millis(150);
    let worker_router = router.clone();
    let (completed, completion) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let result = worker_router.render_gemini_audio(job_id, request, &cancellation, deadline);
        completed.send(result).expect("completion receiver");
    });
    router
        .audio_renderer
        .wait_until_queued_for_test(AudioRenderPriority::Foreground);

    let error = completion
        .recv_timeout(Duration::from_secs(1))
        .expect("deadline must cancel the queued render")
        .expect_err("queued render must not run");
    assert_eq!(error, "audio render deadline expired");
    assert_eq!(
        router
            .audio_renderer
            .queued_for_test(AudioRenderPriority::Foreground),
        0
    );

    router.audio_renderer.release_for_test();
    worker.join().expect("queued render worker");
}

#[test]
fn interrupted_gemini_render_keeps_the_job_terminal() {
    let router = Router::demo();
    router.audio_renderer.occupy_for_test();
    let (job_id, _, _) = router
        .edit_jobs
        .create(TEST_AI_POLL_INTERVAL_MS, Some("render-interrupt"))
        .expect("edit job");
    let cancellation = router.edit_jobs.cancellation(job_id);
    let request = crate::gemini_tools::AudioRenderRequest {
        project: Project::demo(),
        track_ids: Vec::new(),
        start: 0.0,
        end: 1.0,
        description: "interrupt test".to_owned(),
        require_audible_output: false,
    };
    let worker_router = router.clone();
    let (completed, completion) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let result = worker_router.render_gemini_audio(
            job_id,
            request,
            &cancellation,
            Instant::now() + Duration::from_secs(1),
        );
        completed.send(result).expect("completion receiver");
    });
    router
        .audio_renderer
        .wait_until_queued_for_test(AudioRenderPriority::Foreground);

    assert!(router.edit_jobs.interrupt(job_id));
    router.audio_renderer.wake_waiters();
    let error = completion
        .recv_timeout(Duration::from_secs(1))
        .expect("interruption must cancel the queued render")
        .expect_err("interrupted render must not run");
    assert_eq!(error, "audio render interrupted");
    let response = router.edit_jobs.response(job_id).expect("interrupted job");
    let body: serde_json::Value =
        serde_json::from_str(&response.body).expect("interrupted job JSON");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["errorStatus"], 409);

    router.audio_renderer.release_for_test();
    worker.join().expect("queued render worker");
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
    let version = router.lock_project().project().version;
    let request = request(
        "GET",
        &format!("/api/track-spectrum/test-audio-token/{version}/0/1000"),
        "",
    );
    let worker_router = router.clone();
    let worker = thread::spawn(move || worker_router.track_spectrum_data(&request, || false));

    router
        .audio_renderer
        .wait_until_queued_for_test(AudioRenderPriority::Background);
    let mut project = router.lock_project().project().clone();
    project.version += 1;
    router.lock_project().replace_project_for_test(project);
    router.audio_renderer.release_for_test();

    let result = worker.join().expect("spectrum worker");
    assert!(matches!(result, Ok(None)));
}

#[test]
fn rejects_non_utf8_request_bodies() {
    let error = framework_request("POST", "/api/edits", &[("host", "localhost")], &[0xff])
        .expect_err("non-UTF-8 body must be rejected");
    assert_eq!(error, "request body must be UTF-8");
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
    let public = authority("studio.example:8443").expect("public authority");
    assert_eq!(public.host(), "studio.example");
    assert_eq!(public.port_u16(), Some(8443));
    let ipv6 = authority("[::1]:8888").expect("IPv6 authority");
    assert_eq!(ipv6.host(), "[::1]");
    assert_eq!(ipv6.port_u16(), Some(8888));
    assert!(authority("studio.example/path").is_none());

    let mut request = request("POST", "/api/mix", "");
    request
        .headers
        .insert("host".to_owned(), "[::1]:8888".to_owned());
    request
        .headers
        .insert("origin".to_owned(), "http://[::1]:8888".to_owned());
    assert!(request.is_trusted_request("[::1]:8888"));

    request
        .headers
        .insert("origin".to_owned(), "http://[::1]:8888/path".to_owned());
    assert!(!request.is_trusted_request("[::1]:8888"));
}

#[test]
fn response_contains_security_and_length_headers() {
    let mut response = Response::json(200, "{\"ok\":true}".to_owned());
    response.set_cookie = Some("daw_ai_user=0123456789abcdef0123456789abcdef; Path=/".to_owned());
    let response = response.into_http();
    assert_eq!(response.headers()["content-length"], "11");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["set-cookie"],
        "daw_ai_user=0123456789abcdef0123456789abcdef; Path=/"
    );
}

#[test]
fn history_keeps_every_committed_state() {
    let mut current = Studio::new().project().clone();
    let mut history = ProjectHistory::new(current.clone());
    for index in 0..8 {
        let mut project = Studio::new().project().clone();
        project.name = index.to_string();
        project.version += index + 1;
        history
            .push(&current, &project)
            .expect("append project history");
        current = project;
    }
    assert_eq!(history.len(), 9);
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
    let stream_limiter = Limiter::new(MAX_ACTIVE_HTTP_STREAMS);
    let audio_renderer = Arc::new(AudioRenderer::default());
    let base = Router {
        users: Some(Arc::new(UserRegistry {
            root: root.clone(),
            ai: Ai::Deterministic(Duration::ZERO),
            edit_limiter,
            stream_limiter,
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
    let stream_limiter = Limiter::new(MAX_ACTIVE_HTTP_STREAMS);
    let audio_renderer = Arc::new(AudioRenderer::default());
    let base = Router {
        edit_limiter: Arc::clone(&edit_limiter),
        stream_limiter: Arc::clone(&stream_limiter),
        audio_renderer: Arc::clone(&audio_renderer),
        users: Some(Arc::new(UserRegistry {
            root: root.clone(),
            ai: Ai::Deterministic(Duration::ZERO),
            edit_limiter,
            stream_limiter,
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

    assert_eq!(
        first.lock_project().project().to_json(),
        Project::demo().to_json()
    );
    assert_eq!(
        second.lock_project().project().to_json(),
        Project::demo().to_json()
    );
    assert!(Arc::ptr_eq(&first.edit_limiter, &second.edit_limiter));
    assert!(Arc::ptr_eq(&first.stream_limiter, &second.stream_limiter));
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
    jobs.set_running(id, "rendering", "too late");
    jobs.publish_update(id, 2, "too late");
    jobs.finalize_updates(id, 3);
    jobs.complete(id, "too late".to_owned());
    jobs.fail(id, 500, "too late".to_owned());
    let response = jobs.response(id).expect("interrupted response");
    let body: serde_json::Value = serde_json::from_str(&response.body).expect("job JSON");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["errorStatus"], 409);
    assert_eq!(body["appliedSteps"], 0);
    assert_eq!(body["projectVersion"], serde_json::Value::Null);
    assert!(jobs.is_interrupted(id));
    assert!(jobs.create(100, None).is_err());
    jobs.worker_finished(id);
    assert!(jobs.create(100, None).is_ok());
}

#[test]
fn random_user_ids_preserve_the_cookie_contract() {
    let first = new_operation_id().expect("first random ID");
    let second = new_operation_id().expect("second random ID");
    assert!(valid_user_id(&first));
    assert!(valid_user_id(&second));
    assert_ne!(first, second);
}
