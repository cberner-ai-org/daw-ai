#[allow(dead_code)]
mod audio_analysis;
mod audio_stream;
mod concurrency;
pub mod gemini;
mod gemini_tools;
pub mod model;
mod project_file;
mod project_history;
pub mod prompt;
pub mod server;
mod storage;
mod surge;
mod surge_presets;

pub const DEFAULT_PORT: u16 = 8888;

pub fn parse_port<I, S>(arguments: I) -> Result<u16, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments: Vec<S> = arguments.into_iter().collect();
    match arguments.as_slice() {
        [] => Ok(DEFAULT_PORT),
        [port] => parse_port_number(port.as_ref()),
        [flag, port] if flag.as_ref() == "--port" => parse_port_number(port.as_ref()),
        _ => Err("usage: daw-ai [--port] [PORT]".to_owned()),
    }
}

fn parse_port_number(value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("invalid port: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_charter_port() {
        assert_eq!(parse_port(Vec::<String>::new()), Ok(8888));
    }

    #[test]
    fn accepts_positional_and_flagged_ports() {
        assert_eq!(parse_port(["9000"]), Ok(9000));
        assert_eq!(parse_port(["--port", "9001"]), Ok(9001));
    }

    #[test]
    fn rejects_invalid_arguments() {
        assert!(parse_port(["0"]).is_err());
        assert!(parse_port(["--port", "nope"]).is_err());
        assert!(parse_port(["--unknown", "9000"]).is_err());
    }
}
