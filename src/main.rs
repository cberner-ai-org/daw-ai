use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if let [flag, session_path] = arguments.as_slice() {
        if flag == "--codex-mcp" {
            return match daw_ai::gemini_tools::run_codex_mcp(std::path::Path::new(session_path)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("DAW-AI Codex tool server stopped: {error}");
                    ExitCode::FAILURE
                }
            };
        }
    }
    let port = match daw_ai::parse_port(&arguments) {
        Ok(port) => port,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    match daw_ai::server::run(port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("DAW-AI stopped: {error}");
            ExitCode::FAILURE
        }
    }
}
