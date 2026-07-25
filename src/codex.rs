use std::fs;
use std::io::Write;
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
use crate::prompt::{Action, EditPlan};

const CODEX_TIMEOUT: Duration = Duration::from_secs(crate::gemini::EDIT_TIMEOUT_SECONDS);
const STUDIO_CONTRACT: &str = include_str!("../gemini/STUDIO.md");
const CODEX_APPROVAL_CONFIG: &str = "approval_policy=\"never\"";
static CODEX_HOME_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct TemporaryCodexHome {
    path: std::path::PathBuf,
}

impl TemporaryCodexHome {
    fn create_in(root: &std::path::Path, credential: &std::path::Path) -> std::io::Result<Self> {
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
                        let auth_path = path.join("auth.json");
                        fs::copy(credential, &auth_path)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
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
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => self.terminate(),
        }
    }
}

fn configure_exec_command(
    command: &mut Command,
    executable: &std::path::Path,
    session_path: &std::path::Path,
) {
    let mcp_command = format!(
        "mcp_servers.daw_ai.command={:?}",
        executable.to_string_lossy()
    );
    let mcp_arguments = format!(
        "mcp_servers.daw_ai.args=[\"--codex-mcp\",{:?}]",
        session_path.to_string_lossy()
    );
    let mcp_environment = [
        "DAW_AI_SURGE_PRESET_DIR",
        "SURGE_DATA_HOME",
        "XDG_DATA_HOME",
    ]
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
        .arg("--sandbox")
        .arg("workspace-write")
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
        .arg(session_path)
        .arg("-");
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
        ) -> Result<crate::gemini_tools::AudioRender, String>,
        mut on_update: impl FnMut(GeminiEdit) -> Result<Project, PlannerError>,
    ) -> Result<GeminiEdit, PlannerError> {
        let session = EditSession::create_in(session_root, project, prompt, start, end)
            .map_err(PlannerError::Io)?;
        session
            .identify_provider("Codex CLI", "Codex session started")
            .map_err(PlannerError::Io)?;
        let result = (|| {
            let listening = prepare_audio_render(
                session.path(),
                &serde_json::json!({"tracks":"all","start":start,"end":end.min(start + 16.0)}),
            )
            .and_then(&mut render_audio)
            .map_err(|message| {
                PlannerError::Unavailable(format!("Codex listening render failed: {message}"))
            })?;
            let listening_path = session.path().join("codex-listening.wav");
            fs::write(&listening_path, listening.wav).map_err(PlannerError::Io)?;
            let reference_path = if let Some(reference) = reference_audio {
                Some(
                    reference
                        .materialize_in(session.path())
                        .map_err(PlannerError::Io)?,
                )
            } else {
                None
            };
            let initial_end = end.min(start + 16.0);
            let instructions = format!(
                "You are the autonomous sound-graph producer inside DAW-AI. Work only in this directory. \
Read request.json and the contract below. Form a musical arrangement plan from the request, genre, \
selected region, and existing composition. Use the registered daw_ai MCP tools for every graph read, \
mutation, preset/control lookup, undo, and listening render. The render_audio_region tool saves its \
WAV locally and returns its directly accessible absolute path, identifying the exact tracks and time \
section requested. The initial Surge XT WAV at {} is the all-tracks render of the requested section \
from {start:.3} to {initial_end:.3} seconds.{} \
Analyze local WAV files when useful. Finish only after the registered tools have completed the edit.\n\n{}",
                listening_path.display(),
                reference_path
                    .as_ref()
                    .map_or_else(String::new, |path| format!(
                        " The user's reference audio is at {}.",
                        path.display()
                    )),
                STUDIO_CONTRACT
            );
            let mut command = Command::new("codex");
            let codex_home = std::env::var_os("CREDENTIALS_DIRECTORY")
                .map(std::path::PathBuf::from)
                .map(|directory| directory.join("codex-auth"))
                .filter(|path| path.is_file())
                .map(|credential| TemporaryCodexHome::create_in(&std::env::temp_dir(), &credential))
                .transpose()
                .map_err(PlannerError::Io)?;
            if let Some(home) = codex_home.as_ref() {
                command.env("CODEX_HOME", &home.path);
            }
            let executable = std::env::current_exe().map_err(PlannerError::Io)?;
            configure_exec_command(&mut command, &executable, session.path());
            #[cfg(unix)]
            command.process_group(0);
            let stdout_path = session.path().join("codex-stdout.log");
            let stderr_path = session.path().join("codex-stderr.log");
            let child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::from(
                    fs::File::create(&stdout_path).map_err(PlannerError::Io)?,
                ))
                .stderr(Stdio::from(
                    fs::File::create(&stderr_path).map_err(PlannerError::Io)?,
                ))
                .spawn()
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        PlannerError::Unavailable(
                            "Codex CLI is required; install it and authenticate with `codex login`"
                                .to_owned(),
                        )
                    } else {
                        PlannerError::Io(error)
                    }
                })?;
            let mut child = ChildGuard::new(child);
            child
                .take_stdin()
                .expect("piped Codex stdin")
                .write_all(instructions.as_bytes())
                .map_err(PlannerError::Io)?;
            let started = Instant::now();
            let mut plans = Vec::new();
            let mut committed_project = project.clone();
            let status = loop {
                if cancellation.load(Ordering::SeqCst) {
                    child.terminate();
                    return Err(PlannerError::Interrupted);
                }
                if started.elapsed() >= CODEX_TIMEOUT {
                    child.terminate();
                    return Err(PlannerError::TimedOut);
                }
                if let Some((plan, update)) =
                    session.take_update().map_err(PlannerError::InvalidOutput)?
                {
                    committed_project = on_update(GeminiEdit {
                        plan: plan.clone(),
                        project: update,
                    })?;
                    session
                        .synchronize_project(&committed_project)
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
                return Err(PlannerError::Failed {
                    message: fs::read_to_string(stderr_path)
                        .unwrap_or_else(|error| {
                            format!("could not read Codex error output: {error}")
                        })
                        .trim()
                        .to_owned(),
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
        if let Err(error) = session.update_status(status, &detail, applied_steps, audio_listens) {
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
    use super::{ChildGuard, TemporaryCodexHome, configure_exec_command};
    use std::fs;
    use std::process::Command;

    #[test]
    fn codex_exec_uses_supported_approval_configuration_and_safe_default_sandbox() {
        let mut command = Command::new("codex");
        configure_exec_command(
            &mut command,
            std::path::Path::new("/usr/local/bin/daw-ai"),
            std::path::Path::new("/tmp/edit-session"),
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
        let sandbox = arguments
            .windows(2)
            .find(|arguments| arguments[0] == "--sandbox")
            .map(|arguments| arguments[1].as_str());
        assert_eq!(sandbox, Some("workspace-write"));
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

    #[test]
    fn temporary_codex_home_removes_its_credential_copy_on_drop() {
        let root =
            std::env::temp_dir().join(format!("daw-ai-codex-home-test-{}", std::process::id()));
        fs::create_dir_all(&root).expect("temporary root");
        let credential = root.join("source-auth.json");
        fs::write(&credential, b"secret").expect("source credential");

        let home_path = {
            let home =
                TemporaryCodexHome::create_in(&root, &credential).expect("temporary Codex home");
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
