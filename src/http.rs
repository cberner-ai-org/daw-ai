use std::collections::HashMap;
use std::io;

use http::request::Parts;
use http::uri::Authority;
use http::{HeaderMap, HeaderName, HeaderValue, Response as HttpResponse, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use url::Url;

pub(crate) const MAX_REQUEST_BYTES: usize = 6 * 1024 * 1024;
pub(crate) const MAX_REQUEST_HEADER_BYTES: usize = 32 * 1024;
pub(crate) const AUDIO_REQUEST_HEADER: &str = "x-daw-ai-audio";
pub(crate) type HttpBody = UnsyncBoxBody<Bytes, io::Error>;

pub(crate) fn full_body(body: impl Into<Bytes>) -> HttpBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

pub(crate) fn empty_body() -> HttpBody {
    full_body(Bytes::new())
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: String,
}

impl Request {
    pub(crate) fn from_http(parts: Parts, body: &[u8]) -> Result<Self, String> {
        if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
            return Err("request path must be origin-form".to_owned());
        }
        let mut header_bytes = 0_usize;
        let mut headers = HashMap::new();
        for (name, value) in &parts.headers {
            header_bytes = header_bytes
                .checked_add(name.as_str().len())
                .and_then(|length| length.checked_add(value.as_bytes().len() + 4))
                .ok_or_else(|| "request headers are too large".to_owned())?;
            if header_bytes > MAX_REQUEST_HEADER_BYTES {
                return Err("request headers are too large".to_owned());
            }
            let value = value
                .to_str()
                .map_err(|_| "request headers must be UTF-8".to_owned())?;
            if headers
                .insert(name.as_str().to_owned(), value.to_owned())
                .is_some()
            {
                return Err("duplicate request header".to_owned());
            }
        }
        let body = std::str::from_utf8(body)
            .map_err(|_| "request body must be UTF-8".to_owned())?
            .to_owned();
        Ok(Self {
            method: parts.method.as_str().to_owned(),
            path: parts.uri.path().to_owned(),
            headers,
            body,
        })
    }

    pub(crate) fn user_id(&self) -> Option<&str> {
        self.headers.get("cookie")?.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "daw_ai_user" && valid_user_id(value)).then_some(value)
        })
    }

    pub(crate) fn is_mutation(&self) -> bool {
        self.method == "POST"
            && (matches!(
                self.path.as_str(),
                "/api/edits"
                    | "/api/duration"
                    | "/api/mix"
                    | "/api/logs"
                    | "/api/undo"
                    | "/api/reset"
            ) || self.path.starts_with("/api/edits/"))
    }

    pub(crate) fn public_host(&self) -> Option<&str> {
        let transport_host = self.headers.get("host")?;
        authority(transport_host)?;
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

#[derive(Debug)]
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

    pub(crate) fn into_http(self) -> HttpResponse<HttpBody> {
        response_with_body(
            self.status,
            self.content_type,
            self.body.len(),
            &self.headers,
            self.set_cookie.as_deref(),
            full_body(self.body),
        )
    }
}

pub(crate) fn response_with_body(
    status: u16,
    content_type: &str,
    content_length: usize,
    headers: &[(&str, &str)],
    set_cookie: Option<&str>,
    body: HttpBody,
) -> HttpResponse<HttpBody> {
    let mut response = HttpResponse::new(body);
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let response_headers = response.headers_mut();
    insert_header(response_headers, "content-type", content_type);
    insert_header(
        response_headers,
        "content-length",
        &content_length.to_string(),
    );
    insert_header(response_headers, "x-content-type-options", "nosniff");
    insert_header(
        response_headers,
        "content-security-policy",
        concat!(
            "default-src 'self'; script-src 'self'; ",
            "style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; ",
            "media-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none';"
        ),
    );
    insert_header(response_headers, "referrer-policy", "no-referrer");
    for (name, value) in headers {
        insert_header(response_headers, name, value);
    }
    if let Some(cookie) = set_cookie {
        insert_header(response_headers, "set-cookie", cookie);
    }
    response
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let name = HeaderName::from_bytes(name.as_bytes()).expect("response header name is valid");
    let value = HeaderValue::from_str(value).expect("response header value is valid");
    headers.insert(name, value);
}

fn forwarded_host(value: &str) -> Option<&str> {
    let host = value.split(',').next()?.trim();
    authority(host).map(|_| host)
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let Ok(origin) = Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let Some(request) = authority(host) else {
        return false;
    };
    let default_port = if origin.scheme() == "http" { 80 } else { 443 };
    origin
        .host_str()
        .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(request.host()))
        && origin.port_or_known_default() == Some(request.port_u16().unwrap_or(default_port))
}

pub(crate) fn authority(value: &str) -> Option<Authority> {
    if value.contains('@') {
        return None;
    }
    value
        .parse::<Authority>()
        .ok()
        .filter(|authority| authority.port_u16() != Some(0))
}
