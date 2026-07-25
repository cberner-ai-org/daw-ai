use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn drop_process_capabilities() -> std::io::Result<()> {
    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    let result = unsafe {
        libc::syscall(
            libc::SYS_capset,
            std::ptr::from_mut(&mut header),
            data.as_mut_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn drop_process_capabilities() -> std::io::Result<()> {
    Ok(())
}

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
    if let Err(error) = drop_process_capabilities() {
        eprintln!("DAW-AI could not drop process capabilities: {error}");
        return ExitCode::FAILURE;
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
