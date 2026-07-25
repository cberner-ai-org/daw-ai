use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::gemini::{GeminiEdit, PlannerError, ReferenceAudio};
use crate::gemini_tools::{AudioRenderRequest, EditSession, prepare_audio_render};
use crate::model::Project;
use crate::project_history::ProjectHistory;
use crate::prompt::{Action, EditPlan};

const CODEX_TIMEOUT: Duration = Duration::from_secs(crate::gemini::EDIT_TIMEOUT_SECONDS);
const STUDIO_CONTRACT: &str = include_str!("../gemini/STUDIO.md");
const CODEX_APPROVAL_CONFIG: &str = "approval_policy=\"never\"";
static CODEX_HOME_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const SANDBOX_SESSION_PATH: &str = "/workspace";
const PROJECT_ASSETS_DIRECTORY: &str = "project-assets";
const MAX_ACCEPTED_AUDIO_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CODEX_ERROR_BYTES: u64 = 64 * 1024;
const MAX_CODEX_WORKSPACE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CODEX_WORKSPACE_ENTRIES: u64 = 4096;

pub(crate) fn prune_project_assets(
    session_root: &std::path::Path,
    history: &ProjectHistory,
) -> std::io::Result<()> {
    let assets_path = session_root.join(PROJECT_ASSETS_DIRECTORY);
    let entries = match fs::read_dir(&assets_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let referenced = history
        .snapshots
        .iter()
        .flat_map(|project| &project.tracks)
        .flat_map(|track| &track.audio_clips)
        .map(|clip| std::path::PathBuf::from(&clip.asset))
        .collect::<HashSet<_>>();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if (file_type.is_file() || file_type.is_symlink()) && !referenced.contains(&path) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

struct TemporaryCodexHome {
    path: std::path::PathBuf,
}

impl TemporaryCodexHome {
    fn create_in(
        root: &std::path::Path,
        credential: Option<&std::path::Path>,
        deny_commands: bool,
    ) -> std::io::Result<Self> {
        for _ in 0..64 {
            let sequence = CODEX_HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "daw-ai-codex-home-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let setup = (|| {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                        }
                        if let Some(credential) = credential {
                            let auth_path = path.join("auth.json");
                            fs::copy(credential, &auth_path)?;
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
                            }
                        }
                        if deny_commands {
                            fs::write(
                                path.join("config.toml"),
                                concat!(
                                    "default_permissions = \"daw-ai\"\n",
                                    "[permissions.daw-ai.filesystem]\n",
                                    "\":minimal\" = \"read\"\n",
                                    "\"/codex-home\" = \"deny\"\n",
                                    "[permissions.daw-ai.filesystem.\":workspace_roots\"]\n",
                                    "\".\" = \"write\"\n",
                                    "[permissions.daw-ai.network]\n",
                                    "enabled = false\n"
                                ),
                            )?;
                        }
                        Ok::<(), std::io::Error>(())
                    })();
                    if let Err(error) = setup {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a temporary Codex home",
        ))
    }
}

impl Drop for TemporaryCodexHome {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("warning: could not remove temporary Codex home: {error}");
            }
        }
    }
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.stdin.take()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        {
            if let Ok(process_group) = i32::try_from(self.child.id()) {
                // The child is its process-group leader, so a negative PID
                // reaches Codex and every command or MCP server it spawned.
                unsafe {
                    libc::kill(-process_group, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn configure_exec_command(
    command: &mut Command,
    executable: &std::path::Path,
    session_path: &std::path::Path,
    workspace_sandbox: bool,
) {
    let mcp_command = format!(
        "mcp_servers.daw_ai.command={:?}",
        executable.to_string_lossy()
    );
    let mcp_arguments = format!(
        "mcp_servers.daw_ai.args=[\"--codex-mcp\",{:?}]",
        session_path.to_string_lossy()
    );
    let mcp_environment = ["DAW_AI_SURGE_PRESET_DIR", "SURGE_DATA_HOME"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| format!("{name}={value:?}"))
        })
        .collect::<Vec<_>>()
        .join(",");
    command
        .arg("exec")
        .arg("--ephemeral")
        .arg("--skip-git-repo-check")
        .arg("--ignore-rules")
        .arg("--config")
        .arg(CODEX_APPROVAL_CONFIG)
        .arg("--config")
        .arg(mcp_command)
        .arg("--config")
        .arg(mcp_arguments)
        .args(
            (!mcp_environment.is_empty())
                .then(|| {
                    [
                        "--config".to_owned(),
                        format!("mcp_servers.daw_ai.env={{{mcp_environment}}}"),
                    ]
                })
                .into_iter()
                .flatten(),
        )
        .arg("--cd")
        .arg(session_path);
    if workspace_sandbox {
        command.arg("--sandbox").arg("workspace-write");
    }
    command.arg("-");
}

fn packaged_service() -> bool {
    std::env::var_os("INVOCATION_ID").is_some_and(|value| !value.is_empty())
        && std::env::var_os("CREDENTIALS_DIRECTORY")
            .map(std::path::PathBuf::from)
            .is_some_and(|path| path.starts_with("/run/credentials/"))
}

#[cfg(unix)]
fn packaged_codex_command(
    executable: &std::path::Path,
    session_path: &std::path::Path,
    codex_home: &TemporaryCodexHome,
    preset_directories: &[std::path::PathBuf],
) -> Command {
    let mut command = Command::new("bwrap");
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--share-net",
        "--clearenv",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--dir",
        "/etc",
        "--ro-bind",
        "/etc/ssl",
        "/etc/ssl",
        "--ro-bind",
        "/etc/resolv.conf",
        "/etc/resolv.conf",
        "--ro-bind",
        "/etc/hosts",
        "/etc/hosts",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--size",
        "67108864",
        "--tmpfs",
        "/tmp",
    ]);
    let mut created_directories = HashSet::new();
    for preset_directory in preset_directories {
        if !preset_directory.is_absolute()
            || !preset_directory.is_dir()
            || preset_directory.starts_with("/usr")
        {
            continue;
        }
        let mut parents = preset_directory.ancestors().skip(1).collect::<Vec<_>>();
        parents.reverse();
        for parent in parents {
            if parent != std::path::Path::new("/")
                && !parent.starts_with("/usr")
                && created_directories.insert(parent.to_path_buf())
            {
                command.arg("--dir").arg(parent);
            }
        }
        command
            .arg("--ro-bind")
            .arg(preset_directory)
            .arg(preset_directory);
    }
    command
        .arg("--bind")
        .arg(session_path)
        .arg("/workspace")
        .args(["--bind"])
        .arg(&codex_home.path)
        .arg("/codex-home");
    command.args([
        "--setenv",
        "PATH",
        "/usr/local/bin:/usr/bin:/bin",
        "--setenv",
        "HOME",
        "/workspace",
        "--setenv",
        "CODEX_HOME",
        "/codex-home",
        "--chdir",
        "/workspace",
        "codex",
    ]);
    configure_exec_command(
        &mut command,
        executable,
        std::path::Path::new("/workspace"),
        false,
    );
    command
}

#[cfg(unix)]
fn configured_preset_directories() -> Vec<std::path::PathBuf> {
    let mut directories = std::env::var_os("DAW_AI_SURGE_PRESET_DIR")
        .map(std::path::PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(home) = std::env::var_os("SURGE_DATA_HOME").map(std::path::PathBuf::from) {
        directories.push(home.join("patches_factory"));
        directories.push(home.join("resources/data/patches_factory"));
    }
    directories
}

fn codex_spawn_unavailable(packaged: bool) -> String {
    if packaged {
        "bubblewrap is required for packaged Codex edits".to_owned()
    } else {
        "Codex CLI is required; install it and authenticate with `codex login`".to_owned()
    }
}

fn missing_codex_in_sandbox(stderr: &str) -> bool {
    stderr.contains("execvp codex") && stderr.contains("No such file or directory")
}

fn prepare_initial_listening(
    session_path: &std::path::Path,
    visible_session_path: &std::path::Path,
    start: f32,
    end: f32,
    deadline: Instant,
    render_audio: &mut impl FnMut(
        AudioRenderRequest,
        Instant,
    ) -> Result<crate::gemini_tools::AudioRender, String>,
) -> Result<String, std::io::Error> {
    let initial_end = end.min(start + 16.0);
    match prepare_audio_render(
        session_path,
        &serde_json::json!({"tracks":"all","start":start,"end":initial_end}),
    )
    .and_then(|request| render_audio(request, deadline))
    {
        Ok(listening) => {
            let listening_path = session_path.join("codex-listening.wav");
            fs::write(&listening_path, listening.wav)?;
            Ok(format!(
                "The initial Surge XT WAV at {} is the all-tracks render of the requested section \
from {start:.3} to {initial_end:.3} seconds.",
                visible_session_path.join("codex-listening.wav").display()
            ))
        }
        Err(message) => Ok(format!(
            "The initial all-tracks render of the requested section from {start:.3} to \
{initial_end:.3} seconds was unavailable: {message}. Inspect the graph and use the listening tool \
after repairing any render-blocking problem."
        )),
    }
}

fn stage_sandbox_assets(
    session_path: &std::path::Path,
    visible_session_path: &std::path::Path,
    project: &Project,
) -> (Project, HashMap<String, String>) {
    let mut sandbox_project = project.clone();
    let mut paths: HashMap<String, String> = HashMap::new();
    let mut sequence = 0_usize;
    for clip in sandbox_project
        .tracks
        .iter_mut()
        .flat_map(|track| &mut track.audio_clips)
    {
        let sandbox_path = if let Some(existing) = paths.get(&clip.asset) {
            existing.clone()
        } else {
            sequence += 1;
            let name = format!("staged-audio-{sequence:03}.wav");
            if fs::copy(&clip.asset, session_path.join(&name)).is_err() {
                continue;
            }
            let sandbox_path = visible_session_path
                .join(name)
                .to_string_lossy()
                .into_owned();
            paths.insert(clip.asset.clone(), sandbox_path.clone());
            sandbox_path
        };
        clip.asset = sandbox_path;
    }
    (sandbox_project, paths)
}

fn translate_sandbox_assets_to_host(
    project: &mut Project,
    session_path: &std::path::Path,
    visible_session_path: &std::path::Path,
    project_assets_path: &std::path::Path,
    staged_paths: &mut HashMap<String, String>,
) -> Result<(), String> {
    for clip in project
        .tracks
        .iter_mut()
        .flat_map(|track| &mut track.audio_clips)
    {
        if let Some((host_path, _)) = staged_paths
            .iter()
            .find(|(_, sandbox_path)| *sandbox_path == &clip.asset)
        {
            clip.asset.clone_from(host_path);
            continue;
        }
        let relative = std::path::Path::new(&clip.asset)
            .strip_prefix(visible_session_path)
            .map_err(|_| "Codex audio assets must be inside its session workspace".to_owned())?;
        let mut components = relative.components();
        let Some(std::path::Component::Normal(name)) = components.next() else {
            return Err("Codex audio asset path is invalid".to_owned());
        };
        if components.next().is_some() {
            return Err("Codex audio assets must be direct workspace files".to_owned());
        }
        let source_path = session_path.join(name);
        let session_id = session_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Codex session ID is invalid".to_owned())?;
        let accepted_name = format!("{session_id}-audio-{:03}.wav", staged_paths.len() + 1);
        let accepted_path = project_assets_path.join(accepted_name);
        copy_accepted_audio(&source_path, &accepted_path)?;
        let accepted = accepted_path.to_string_lossy().into_owned();
        staged_paths.insert(accepted.clone(), clip.asset.clone());
        clip.asset = accepted;
    }
    Ok(())
}

fn copy_accepted_audio(
    source_path: &std::path::Path,
    accepted_path: &std::path::Path,
) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut source = options
        .open(source_path)
        .map_err(|error| format!("could not securely open Codex audio asset: {error}"))?;
    let metadata = source
        .metadata()
        .map_err(|error| format!("could not inspect Codex audio asset: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_ACCEPTED_AUDIO_BYTES {
        return Err("Codex audio asset must be a bounded regular file".to_owned());
    }
    let mut header = [0_u8; 12];
    source
        .read_exact(&mut header)
        .map_err(|_| "Codex audio asset is not a complete WAV file".to_owned())?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err("Codex audio asset is not a WAV file".to_owned());
    }
    source
        .rewind()
        .map_err(|error| format!("could not rewind Codex audio asset: {error}"))?;
    let mut accepted = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(accepted_path)
        .map_err(|error| format!("could not reserve accepted Codex audio asset: {error}"))?;
    let copied = std::io::copy(
        &mut source.take(MAX_ACCEPTED_AUDIO_BYTES + 1),
        &mut accepted,
    );
    if let Err(error) = copied {
        drop(accepted);
        let _ = fs::remove_file(accepted_path);
        return Err(format!("could not preserve Codex audio asset: {error}"));
    }
    Ok(())
}

fn bounded_workspace_usage(
    path: &std::path::Path,
    byte_limit: u64,
    entry_limit: u64,
) -> std::io::Result<(u64, u64)> {
    let mut bytes = 0_u64;
    let mut entries = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        entries = entries.saturating_add(1);
        if entries > entry_limit {
            break;
        }
        let metadata = entry.path().symlink_metadata()?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let (nested_bytes, nested_entries) = bounded_workspace_usage(
                &entry.path(),
                byte_limit.saturating_sub(bytes),
                entry_limit.saturating_sub(entries),
            )?;
            bytes = bytes.saturating_add(nested_bytes);
            entries = entries.saturating_add(nested_entries);
        } else {
            bytes = bytes.saturating_add(metadata.len());
        }
        if bytes > byte_limit || entries > entry_limit {
            break;
        }
    }
    Ok((bytes, entries))
}

fn translate_host_assets_to_sandbox(
    project: &mut Project,
    session_path: &std::path::Path,
    staged_paths: &HashMap<String, String>,
) {
    for clip in project
        .tracks
        .iter_mut()
        .flat_map(|track| &mut track.audio_clips)
    {
        if let Some(sandbox_path) = staged_paths.get(&clip.asset) {
            clip.asset.clone_from(sandbox_path);
        } else if let Ok(relative) = std::path::Path::new(&clip.asset).strip_prefix(session_path) {
            clip.asset = std::path::Path::new(SANDBOX_SESSION_PATH)
                .join(relative)
                .to_string_lossy()
                .into_owned();
        }
    }
}

pub(crate) struct CodexPlanner;

impl CodexPlanner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn interpret_with_updates(
        session_root: &std::path::Path,
        prompt: &str,
        start: f32,
        end: f32,
        project: &Project,
        reference_audio: Option<ReferenceAudio>,
        cancellation: Arc<AtomicBool>,
        mut render_audio: impl FnMut(
            AudioRenderRequest,
            Instant,
        ) -> Result<crate::gemini_tools::AudioRender, String>,
        mut on_progress: impl FnMut(&str),
        mut on_update: impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
    ) -> Result<GeminiEdit, PlannerError> {
        let started = Instant::now();
        let check_budget = || {
            if cancellation.load(Ordering::SeqCst) {
                Err(PlannerError::Interrupted)
            } else if started.elapsed() >= CODEX_TIMEOUT {
                Err(PlannerError::TimedOut)
            } else {
                Ok(())
            }
        };
        check_budget()?;
        let session = EditSession::create_in(session_root, project, prompt, start, end)
            .map_err(PlannerError::Io)?;
        let project_assets_path = session_root.join(PROJECT_ASSETS_DIRECTORY);
        fs::create_dir_all(&project_assets_path).map_err(PlannerError::Io)?;
        check_budget()?;
        session
            .identify_provider("Codex CLI", "Codex session started")
            .map_err(PlannerError::Io)?;
        let trusted_session_metadata = session.metadata_source().map_err(PlannerError::Io)?;
        let packaged = packaged_service();
        let result = (|| {
            let visible_session_path = if packaged {
                std::path::Path::new(SANDBOX_SESSION_PATH)
            } else {
                session.path()
            };
            check_budget()?;
            let initial_listening = prepare_initial_listening(
                session.path(),
                visible_session_path,
                start,
                end,
                started + CODEX_TIMEOUT,
                &mut render_audio,
            )
            .map_err(PlannerError::Io)?;
            check_budget()?;
            let reference_path = if let Some(reference) = reference_audio {
                Some(
                    reference
                        .materialize_in(session.path())
                        .map_err(PlannerError::Io)?,
                )
            } else {
                None
            };
            check_budget()?;
            let (sandbox_project, mut staged_paths) =
                stage_sandbox_assets(session.path(), visible_session_path, project);
            check_budget()?;
            session
                .synchronize_project(&sandbox_project)
                .map_err(PlannerError::InvalidOutput)?;
            check_budget()?;
            let instructions = format!(
                "You are the autonomous sound-graph producer inside DAW-AI. Work only in this directory. \
Read request.json and the contract below. Form a musical arrangement plan from the request, genre, \
selected region, and existing composition. Use the registered daw_ai MCP tools for every graph read, \
mutation, preset/control lookup, undo, and listening render. The render_audio_region tool saves its \
WAV locally and returns its directly accessible absolute path, identifying the exact tracks and time \
section requested. {initial_listening}{} \
Analyze local WAV files when useful. Finish only after the registered tools have completed the edit.\n\n{}",
                reference_path.as_ref().map_or_else(String::new, |path| {
                    let visible_path = if packaged {
                        visible_session_path.join(path.file_name().unwrap_or_default())
                    } else {
                        path.clone()
                    };
                    format!(
                        " The user's reference audio is at {}.",
                        visible_path.display()
                    )
                }),
                STUDIO_CONTRACT
            );
            let credential = std::env::var_os("CREDENTIALS_DIRECTORY")
                .map(std::path::PathBuf::from)
                .map(|directory| directory.join("codex-auth"))
                .filter(|path| path.is_file());
            let codex_home = if packaged {
                Some(
                    TemporaryCodexHome::create_in(
                        &std::env::temp_dir(),
                        credential.as_deref(),
                        true,
                    )
                    .map_err(PlannerError::Io)?,
                )
            } else {
                credential
                    .as_deref()
                    .map(|credential| {
                        TemporaryCodexHome::create_in(
                            &std::env::temp_dir(),
                            Some(credential),
                            false,
                        )
                    })
                    .transpose()
                    .map_err(PlannerError::Io)?
            };
            check_budget()?;
            let executable = std::env::current_exe().map_err(PlannerError::Io)?;
            #[cfg(unix)]
            let mut command = if packaged {
                packaged_codex_command(
                    &executable,
                    session.path(),
                    codex_home.as_ref().expect("packaged Codex home"),
                    &configured_preset_directories(),
                )
            } else {
                let mut command = Command::new("codex");
                if let Some(home) = codex_home.as_ref() {
                    command.env("CODEX_HOME", &home.path);
                }
                configure_exec_command(&mut command, &executable, session.path(), true);
                command
            };
            #[cfg(not(unix))]
            let mut command = {
                let mut command = Command::new("codex");
                if let Some(home) = codex_home.as_ref() {
                    command.env("CODEX_HOME", &home.path);
                }
                configure_exec_command(&mut command, &executable, session.path(), true);
                command
            };
            command
                .env_remove("CREDENTIALS_DIRECTORY")
                .env_remove("GEMINI_API_KEY");
            #[cfg(unix)]
            {
                command.process_group(0);
                unsafe {
                    command.pre_exec(|| {
                        let limit = libc::rlimit {
                            rlim_cur: MAX_ACCEPTED_AUDIO_BYTES,
                            rlim_max: MAX_ACCEPTED_AUDIO_BYTES,
                        };
                        if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            let stdout_path = session.path().join("codex-stdout.log");
            let stderr_path = session.path().join("codex-stderr.log");
            let stderr_file = fs::File::create(&stderr_path).map_err(PlannerError::Io)?;
            let child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::from(
                    fs::File::create(&stdout_path).map_err(PlannerError::Io)?,
                ))
                .stderr(Stdio::from(
                    stderr_file.try_clone().map_err(PlannerError::Io)?,
                ))
                .spawn()
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        PlannerError::Unavailable(codex_spawn_unavailable(packaged))
                    } else {
                        PlannerError::Io(error)
                    }
                })?;
            let mut child = ChildGuard::new(child);
            check_budget()?;
            child
                .take_stdin()
                .expect("piped Codex stdin")
                .write_all(instructions.as_bytes())
                .map_err(PlannerError::Io)?;
            let mut plans = Vec::new();
            let mut committed_project = project.clone();
            let mut last_detail = String::new();
            let status = loop {
                if cancellation.load(Ordering::SeqCst) {
                    child.terminate();
                    return Err(PlannerError::Interrupted);
                }
                if started.elapsed() >= CODEX_TIMEOUT {
                    child.terminate();
                    return Err(PlannerError::TimedOut);
                }
                let (workspace_bytes, workspace_entries) = bounded_workspace_usage(
                    session.path(),
                    MAX_CODEX_WORKSPACE_BYTES,
                    MAX_CODEX_WORKSPACE_ENTRIES,
                )
                .map_err(PlannerError::Io)?;
                let (home_bytes, home_entries) = codex_home
                    .as_ref()
                    .map(|home| {
                        bounded_workspace_usage(
                            &home.path,
                            MAX_CODEX_WORKSPACE_BYTES.saturating_sub(workspace_bytes),
                            MAX_CODEX_WORKSPACE_ENTRIES.saturating_sub(workspace_entries),
                        )
                    })
                    .transpose()
                    .map_err(PlannerError::Io)?
                    .unwrap_or_default();
                if workspace_bytes.saturating_add(home_bytes) > MAX_CODEX_WORKSPACE_BYTES
                    || workspace_entries.saturating_add(home_entries) > MAX_CODEX_WORKSPACE_ENTRIES
                {
                    child.terminate();
                    return Err(PlannerError::Failed {
                        message: "Codex workspace exceeded its storage limit".to_owned(),
                        code: Some("codex_workspace_limit".to_owned()),
                    });
                }
                if let Ok(detail) = session.detail() {
                    if detail != last_detail {
                        on_progress(&detail);
                        last_detail = detail;
                    }
                }
                if let Some((plan, mut update)) =
                    session.take_update().map_err(PlannerError::InvalidOutput)?
                {
                    translate_sandbox_assets_to_host(
                        &mut update,
                        session.path(),
                        visible_session_path,
                        &project_assets_path,
                        &mut staged_paths,
                    )
                    .map_err(PlannerError::InvalidOutput)?;
                    committed_project = on_update(GeminiEdit {
                        plan: plan.clone(),
                        project: update,
                    })?;
                    let mut synchronized_project = committed_project.clone();
                    translate_host_assets_to_sandbox(
                        &mut synchronized_project,
                        session.path(),
                        &staged_paths,
                    );
                    session
                        .synchronize_project(&synchronized_project)
                        .map_err(PlannerError::InvalidOutput)?;
                    session
                        .acknowledge_codex_update()
                        .map_err(PlannerError::InvalidOutput)?;
                    plans.push(plan);
                }
                if let Some(status) = child.try_wait().map_err(PlannerError::Io)? {
                    break status;
                }
                thread::sleep(Duration::from_millis(50));
            };
            if !status.success() {
                let mut stderr_file = stderr_file;
                let message = (|| {
                    stderr_file.rewind()?;
                    let mut message = String::new();
                    stderr_file
                        .take(MAX_CODEX_ERROR_BYTES)
                        .read_to_string(&mut message)?;
                    Ok::<_, std::io::Error>(message)
                })()
                .unwrap_or_else(|error| format!("could not read Codex error output: {error}"))
                .trim()
                .to_owned();
                if packaged && missing_codex_in_sandbox(&message) {
                    return Err(PlannerError::Unavailable(codex_spawn_unavailable(false)));
                }
                return Err(PlannerError::Failed {
                    message,
                    code: Some("codex_cli".to_owned()),
                });
            }
            if plans.is_empty() {
                return Err(PlannerError::InvalidOutput(
                    "Codex completed without changing the sound graph".to_owned(),
                ));
            }
            Ok(GeminiEdit {
                plan: EditPlan {
                    action: Action::GraphMutation,
                    summary: "Completed the Codex sound graph edit".to_owned(),
                },
                project: committed_project,
            })
        })();
        let (status, detail) = match &result {
            Ok(edit) => ("completed", edit.plan.summary.clone()),
            Err(error) => ("failed", error.to_string()),
        };
        let (applied_steps, audio_listens) = session.stats().unwrap_or((0, 0));
        if let Err(error) = session.update_status_from(
            &trusted_session_metadata,
            status,
            &detail,
            applied_steps,
            audio_listens,
        ) {
            eprintln!("warning: could not finalize Codex session metadata: {error}");
        }
        if let Err(error) = crate::gemini_tools::apply_session_retention(session_root) {
            eprintln!("warning: could not apply Codex session retention: {error}");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::packaged_codex_command;
    use super::{
        ChildGuard, PROJECT_ASSETS_DIRECTORY, SANDBOX_SESSION_PATH, TemporaryCodexHome,
        bounded_workspace_usage, codex_spawn_unavailable, configure_exec_command,
        copy_accepted_audio, missing_codex_in_sandbox, prepare_initial_listening,
        prune_project_assets, stage_sandbox_assets, translate_host_assets_to_sandbox,
        translate_sandbox_assets_to_host,
    };
    use crate::model::{AudioClip, Project};
    use crate::project_history::ProjectHistory;
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    #[test]
    fn codex_exec_uses_supported_approval_configuration_and_safe_default_sandbox() {
        let mut command = Command::new("codex");
        configure_exec_command(
            &mut command,
            std::path::Path::new("/usr/local/bin/daw-ai"),
            std::path::Path::new("/tmp/edit-session"),
            true,
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--ask-for-approval")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "approval_policy=\"never\"")
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.contains("XDG_DATA_HOME"))
        );
        let sandbox = arguments
            .windows(2)
            .find(|arguments| arguments[0] == "--sandbox")
            .map(|arguments| arguments[1].as_str());
        assert_eq!(sandbox, Some("workspace-write"));
    }

    #[test]
    fn failed_initial_listening_does_not_block_codex_instructions() {
        let session_path = std::env::temp_dir().join(format!(
            "daw-ai-codex-listening-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&session_path).expect("temporary session");
        let note = prepare_initial_listening(
            &session_path,
            &session_path,
            4.0,
            24.0,
            Instant::now() + Duration::from_secs(1),
            &mut |_, _| Err("missing audio asset".to_owned()),
        )
        .expect("optional listening note");

        assert!(note.contains("was unavailable:"));
        assert!(note.contains("after repairing"));
        fs::remove_dir_all(session_path).expect("temporary session cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn packaged_codex_sees_only_its_session_and_temporary_home() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-packaged-codex-home-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temporary root");
        let home = TemporaryCodexHome::create_in(&root, None, true).expect("temporary Codex home");
        let command = packaged_codex_command(
            std::path::Path::new("/usr/local/bin/daw-ai"),
            std::path::Path::new("/var/lib/daw-ai/gemini-sessions/current"),
            &home,
            std::slice::from_ref(&root),
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "bwrap");
        assert!(arguments.windows(3).any(|arguments| arguments
            == [
                "--bind",
                "/var/lib/daw-ai/gemini-sessions/current",
                "/workspace"
            ]));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "/var/lib/daw-ai")
        );
        assert!(arguments.iter().any(|argument| argument == "--clearenv"));
        assert!(arguments.windows(3).any(|arguments| {
            arguments[0] == "--ro-bind"
                && arguments[1] == root.to_string_lossy()
                && arguments[2] == root.to_string_lossy()
        }));
        assert!(!arguments.iter().any(|argument| argument == "--sandbox"));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--cd", "/workspace"])
        );
        let config = fs::read_to_string(home.path.join("config.toml")).expect("permission profile");
        assert!(config.contains("\"/codex-home\" = \"deny\""));
        drop(home);
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn codex_dependency_errors_identify_the_missing_layer() {
        assert!(codex_spawn_unavailable(true).contains("bubblewrap"));
        assert!(codex_spawn_unavailable(false).contains("Codex CLI"));
        assert!(missing_codex_in_sandbox(
            "bwrap: execvp codex: No such file or directory"
        ));
        assert!(!missing_codex_in_sandbox(
            "bwrap: execvp something-else: No such file or directory"
        ));
    }

    #[test]
    fn packaged_audio_paths_round_trip_between_host_and_sandbox() {
        let root =
            std::env::temp_dir().join(format!("daw-ai-codex-assets-test-{}", std::process::id()));
        let session = root.join("session");
        fs::create_dir_all(&session).expect("temporary session");
        let existing = root.join("existing.wav");
        fs::write(&existing, b"existing audio").expect("existing audio");
        let mut project = Project::initial();
        project.tracks[0].audio_clips.push(AudioClip {
            id: 900,
            label: "Existing".to_owned(),
            start: 0.0,
            end: 1.0,
            asset: existing.to_string_lossy().into_owned(),
            source_offset: 0.0,
            source_duration: 1.0,
            gain: 1.0,
            reversed: false,
        });

        let (mut sandbox_project, mut staged) = stage_sandbox_assets(
            &session,
            std::path::Path::new(SANDBOX_SESSION_PATH),
            &project,
        );
        let project_assets = root.join(PROJECT_ASSETS_DIRECTORY);
        fs::create_dir(&project_assets).expect("project asset directory");
        fs::write(session.join("audio-001.wav"), b"RIFF\x04\x00\x00\x00WAVE")
            .expect("generated WAV");
        assert_eq!(
            sandbox_project.tracks[0].audio_clips[0].asset,
            "/workspace/staged-audio-001.wav"
        );
        sandbox_project.tracks[0].audio_clips.push(AudioClip {
            id: 901,
            label: "Resampled".to_owned(),
            start: 1.0,
            end: 2.0,
            asset: "/workspace/audio-001.wav".to_owned(),
            source_offset: 0.0,
            source_duration: 1.0,
            gain: 1.0,
            reversed: false,
        });
        translate_sandbox_assets_to_host(
            &mut sandbox_project,
            &session,
            std::path::Path::new(SANDBOX_SESSION_PATH),
            &project_assets,
            &mut staged,
        )
        .expect("valid sandbox assets");
        assert_eq!(
            sandbox_project.tracks[0].audio_clips[0].asset,
            project.tracks[0].audio_clips[0].asset
        );
        let accepted_asset = project_assets.join("session-audio-002.wav");
        assert_eq!(
            sandbox_project.tracks[0].audio_clips[1].asset,
            accepted_asset.to_string_lossy()
        );
        assert!(accepted_asset.is_file());

        sandbox_project.tracks[0].audio_clips[1].asset =
            "/workspace/../another-session/private.wav".to_owned();
        assert!(
            translate_sandbox_assets_to_host(
                &mut sandbox_project,
                &session,
                std::path::Path::new(SANDBOX_SESSION_PATH),
                &project_assets,
                &mut staged,
            )
            .is_err()
        );
        sandbox_project.tracks[0].audio_clips[1].asset =
            accepted_asset.to_string_lossy().into_owned();
        translate_host_assets_to_sandbox(&mut sandbox_project, &session, &staged);
        assert_eq!(
            sandbox_project.tracks[0].audio_clips[0].asset,
            "/workspace/staged-audio-001.wav"
        );
        assert_eq!(
            sandbox_project.tracks[0].audio_clips[1].asset,
            "/workspace/audio-001.wav"
        );
        fs::remove_dir_all(root).expect("temporary assets cleanup");
    }

    #[test]
    fn cancelled_codex_edit_stops_before_baseline_rendering() {
        let cancellation = Arc::new(AtomicBool::new(true));
        let result = super::CodexPlanner::interpret_with_updates(
            std::path::Path::new("/unused"),
            "test",
            0.0,
            1.0,
            &Project::initial(),
            None,
            cancellation,
            |_, _| panic!("cancelled edit rendered audio"),
            |_| {},
            |_| panic!("cancelled edit published an update"),
        );
        assert!(matches!(
            result,
            Err(crate::gemini::PlannerError::Interrupted)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn accepted_codex_audio_does_not_follow_workspace_symlinks() {
        let root =
            std::env::temp_dir().join(format!("daw-ai-codex-symlink-test-{}", std::process::id()));
        fs::create_dir(&root).expect("temporary root");
        let target = root.join("target.wav");
        fs::write(&target, b"RIFF\x04\x00\x00\x00WAVE").expect("target WAV");
        let link = root.join("audio.wav");
        std::os::unix::fs::symlink(&target, &link).expect("workspace symlink");

        assert!(copy_accepted_audio(&link, &root.join("accepted.wav")).is_err());
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn project_asset_pruning_preserves_every_history_reference() {
        let root =
            std::env::temp_dir().join(format!("daw-ai-codex-pruning-test-{}", std::process::id()));
        let assets = root.join(PROJECT_ASSETS_DIRECTORY);
        fs::create_dir_all(&assets).expect("project assets");
        let retained = assets.join("retained.wav");
        let orphaned = assets.join("orphaned.wav");
        fs::write(&retained, b"retained").expect("retained asset");
        fs::write(&orphaned, b"orphaned").expect("orphaned asset");
        let mut project = Project::initial();
        project.tracks[0].audio_clips.push(AudioClip {
            id: 901,
            label: "Retained".to_owned(),
            start: 0.0,
            end: 1.0,
            asset: retained.to_string_lossy().into_owned(),
            source_offset: 0.0,
            source_duration: 1.0,
            gain: 1.0,
            reversed: false,
        });

        prune_project_assets(&root, &ProjectHistory::new(project)).expect("prune assets");

        assert!(retained.is_file());
        assert!(!orphaned.exists());
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[test]
    fn workspace_accounting_stops_after_its_limit() {
        let root =
            std::env::temp_dir().join(format!("daw-ai-workspace-limit-{}", std::process::id()));
        fs::create_dir(&root).expect("temporary root");
        fs::File::create(root.join("large"))
            .expect("large workspace file")
            .set_len(1024)
            .expect("sparse file");
        assert!(
            bounded_workspace_usage(&root, 512, 100)
                .expect("workspace usage")
                .0
                > 512
        );
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_terminates_the_complete_process_group() {
        use std::os::unix::process::CommandExt;

        let child = Command::new("sh")
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("shell child");
        let pid = child.id();
        let mut child = ChildGuard::new(child);
        let mut descendant_pid = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.child.stdout.as_mut().expect("piped stdout")),
            &mut descendant_pid,
        )
        .expect("descendant pid");
        drop(child);
        std::thread::sleep(Duration::from_millis(50));

        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        let descendant_status =
            fs::read_to_string(format!("/proc/{}/stat", descendant_pid.trim())).ok();
        assert!(
            descendant_status.is_none_or(|status| status
                .rsplit_once(") ")
                .is_some_and(|(_, process)| process.starts_with("Z "))),
            "descendant remained running after its process group was terminated"
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_terminates_descendants_after_the_leader_exits() {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("sh")
            .args(["-c", "sleep 60 & echo $!"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("shell child");
        let mut descendant_pid = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.as_mut().expect("piped stdout")),
            &mut descendant_pid,
        )
        .expect("descendant pid");
        child.wait().expect("leader exit");
        drop(ChildGuard::new(child));
        std::thread::sleep(Duration::from_millis(50));

        let descendant_status =
            fs::read_to_string(format!("/proc/{}/stat", descendant_pid.trim())).ok();
        assert!(descendant_status.is_none_or(|status| {
            status
                .rsplit_once(") ")
                .is_some_and(|(_, process)| process.starts_with("Z "))
        }));
    }

    #[test]
    fn temporary_codex_home_removes_its_credential_copy_on_drop() {
        let root =
            std::env::temp_dir().join(format!("daw-ai-codex-home-test-{}", std::process::id()));
        fs::create_dir_all(&root).expect("temporary root");
        let credential = root.join("source-auth.json");
        fs::write(&credential, b"secret").expect("source credential");

        let home_path = {
            let home = TemporaryCodexHome::create_in(&root, Some(&credential), false)
                .expect("temporary Codex home");
            assert_eq!(
                fs::read(home.path.join("auth.json")).expect("credential copy"),
                b"secret"
            );
            home.path.clone()
        };

        assert!(!home_path.exists());
        fs::remove_dir_all(root).expect("temporary root cleanup");
    }
}
