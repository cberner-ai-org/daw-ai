use std::collections::HashMap;
use std::io::{self, Read, Write};

const MAX_REQUEST_BYTES: usize = 6 * 1024 * 1024;
pub(crate) const MAX_REQUEST_HEADER_BYTES: usize = 32 * 1024;
pub(crate) const AUDIO_REQUEST_HEADER: &str = "x-daw-ai-audio";

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: String,
}

impl Request {
    pub(crate) fn user_id(&self) -> Option<&str> {
        self.headers.get("cookie")?.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "daw_ai_user" && valid_user_id(value)).then_some(value)
        })
    }

    pub(crate) fn read(stream: &mut impl Read) -> Result<Self, String> {
        let mut bytes = Vec::with_capacity(2048);
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
                break position + 4;
            }
            if bytes.len() > MAX_REQUEST_HEADER_BYTES {
                return Err("request headers are too large".to_owned());
            }
        };
        if header_end > MAX_REQUEST_HEADER_BYTES {
            return Err("request headers are too large".to_owned());
        }

        let headers = std::str::from_utf8(&bytes[..header_end])
            .map_err(|_| "request headers must be UTF-8".to_owned())?;
        let mut lines = headers.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "missing request line".to_owned())?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts
            .next()
            .ok_or_else(|| "missing method".to_owned())?
            .to_owned();
        let target = request_parts
            .next()
            .ok_or_else(|| "missing path".to_owned())?;
        let version = request_parts
            .next()
            .ok_or_else(|| "missing HTTP version".to_owned())?;
        if request_parts.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
            return Err("invalid request line".to_owned());
        }
        if !target.starts_with('/') {
            return Err("request path must be origin-form".to_owned());
        }
        let path = target.split('?').next().unwrap_or(target).to_owned();

        let mut parsed_headers = HashMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| "invalid request header".to_owned())?;
            if name.is_empty()
                || name.trim() != name
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err("invalid request header name".to_owned());
            }
            let name = name.to_ascii_lowercase();
            if parsed_headers
                .insert(name, value.trim().to_owned())
                .is_some()
            {
                return Err("duplicate request header".to_owned());
            }
        }
        let headers = parsed_headers;
        if headers.contains_key("transfer-encoding") {
            return Err("transfer encoding is not supported".to_owned());
        }
        let content_length = headers.get("content-length").map_or(Ok(0_usize), |value| {
            value
                .parse::<usize>()
                .map_err(|_| "invalid content length".to_owned())
        })?;
        let body_end = header_end
            .checked_add(content_length)
            .ok_or_else(|| "request is too large".to_owned())?;
        if body_end > MAX_REQUEST_BYTES {
            return Err("request is too large".to_owned());
        }

        while bytes.len() < body_end {
            let remaining = body_end - bytes.len();
            let mut chunk = [0_u8; 2048];
            let count = stream
                .read(&mut chunk[..remaining.min(2048)])
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request body".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
        }

        let body = std::str::from_utf8(&bytes[header_end..body_end])
            .map_err(|_| "request body must be UTF-8".to_owned())?
            .to_owned();
        Ok(Self {
            method,
            path,
            headers,
            body,
        })
    }

    pub(crate) fn is_mutation(&self) -> bool {
        self.method == "POST"
            && (matches!(
                self.path.as_str(),
                "/api/edits"
                    | "/api/duration"
                    | "/api/mix"
                    | "/api/sound-tools"
                    | "/api/channels"
                    | "/api/logs"
                    | "/api/undo"
                    | "/api/reset"
            ) || self.path.starts_with("/api/edits/"))
    }

    pub(crate) fn public_host(&self) -> Option<&str> {
        let transport_host = self.headers.get("host")?;
        parse_authority(transport_host)?;
        let forwarded = match self.headers.get("x-forwarded-host") {
            Some(value) => Some(forwarded_host(value)?),
            None => None,
        };
        forwarded.or(Some(transport_host))
    }

    pub(crate) fn is_trusted_mutation(&self, host: &str) -> bool {
        self.is_trusted_request(host)
    }

    pub(crate) fn is_trusted_audio(&self, host: &str) -> bool {
        self.headers
            .get(AUDIO_REQUEST_HEADER)
            .is_some_and(|value| value == "1")
            && self.is_trusted_request(host)
    }

    pub(crate) fn is_trusted_request(&self, host: &str) -> bool {
        if self
            .headers
            .get("sec-fetch-site")
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
        {
            return false;
        }
        self.headers
            .get("origin")
            .is_none_or(|origin| origin_matches_host(origin, host))
    }
}

pub(crate) fn valid_user_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) struct Response {
    pub(crate) status: u16,
    content_type: &'static str,
    pub(crate) body: String,
    pub(crate) headers: Vec<(&'static str, &'static str)>,
    pub(crate) set_cookie: Option<String>,
}

impl Response {
    pub(crate) fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
            headers: vec![("Cache-Control", "no-store")],
            set_cookie: None,
        }
    }

    pub(crate) fn static_asset(content_type: &'static str, body: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_owned(),
            headers: vec![(
                "Cache-Control",
                "no-store, no-cache, must-revalidate, max-age=0",
            )],
            set_cookie: None,
        }
    }

    pub(crate) fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    pub(crate) fn write(&self, stream: &mut impl Write) -> io::Result<()> {
        write_response_head(
            stream,
            self.status,
            self.content_type,
            self.body.len(),
            &self.headers,
            self.set_cookie.as_deref(),
        )?;
        stream.write_all(self.body.as_bytes())
    }
}

pub(crate) fn write_response_head(
    stream: &mut impl Write,
    status: u16,
    content_type: &str,
    content_length: usize,
    headers: &[(&str, &str)],
    set_cookie: Option<&str>,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Error",
    };
    let mut head = format!(
        concat!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            "Connection: close\r\nX-Content-Type-Options: nosniff\r\n",
            "Content-Security-Policy: default-src 'self'; script-src 'self'; ",
            "style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; ",
            "media-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none';\r\n",
            "Referrer-Policy: no-referrer\r\n"
        ),
        status, reason, content_type, content_length
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(cookie) = set_cookie {
        head.push_str("Set-Cookie: ");
        head.push_str(cookie);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn forwarded_host(value: &str) -> Option<&str> {
    let host = value.split(',').next()?.trim();
    parse_authority(host).map(|_| host)
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let (authority, default_port) = origin
        .strip_prefix("http://")
        .map(|authority| (authority, 80))
        .or_else(|| {
            origin
                .strip_prefix("https://")
                .map(|authority| (authority, 443))
        })
        .unwrap_or(("", 0));
    if default_port == 0 {
        return false;
    }
    let Some((origin_host, origin_port)) = parse_authority(authority) else {
        return false;
    };
    let Some((request_host, request_port)) = parse_authority(host) else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(request_host)
        && origin_port.unwrap_or(default_port) == request_port.unwrap_or(default_port)
}

pub(crate) fn parse_authority(value: &str) -> Option<(&str, Option<u16>)> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['/', '\\', '?', '#', '@', ','])
    {
        return None;
    }
    if value.starts_with('[') {
        let end = value.find(']')?;
        let hostname = &value[..=end];
        if hostname.len() <= 2 {
            return None;
        }
        let remainder = &value[end + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            Some(parse_port(remainder.strip_prefix(':')?)?)
        };
        return Some((hostname, port));
    }
    let (hostname, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(hostname, port)| (hostname, Some(port)));
    if hostname.is_empty()
        || hostname.contains(':')
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return None;
    }
    let port = match port {
        Some(port) => Some(parse_port(port)?),
        None => None,
    };
    Some((hostname, port))
}

fn parse_port(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|port| *port > 0)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
