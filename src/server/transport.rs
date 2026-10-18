use std::convert::Infallible;
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request as HttpRequest, Response as HttpResponse};
use http_body_util::BodyExt;
use hyper::body::{Body as HyperBody, Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::AsyncWriteExt;

use super::{Router, dispatch_request, error_json, single_line};
use crate::concurrency::{Limiter, Permit};
use crate::http::{HttpBody, MAX_REQUEST_BYTES, Request, Response};

const MAX_ACTIVE_HTTP_CONNECTIONS: usize = 64;
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE_TIMEOUT: Duration = Duration::from_secs(2);
const OVERLOAD_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_BUFFER_BYTES: usize = 64 * 1024;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct BodyReadLimits {
    idle_timeout: Duration,
    timeout: Duration,
}

#[derive(Clone, Copy)]
struct HttpServerConfig {
    max_connections: usize,
    header_timeout: Duration,
    body_limits: BodyReadLimits,
    shutdown_grace_timeout: Duration,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            max_connections: MAX_ACTIVE_HTTP_CONNECTIONS,
            header_timeout: REQUEST_HEADER_TIMEOUT,
            body_limits: BodyReadLimits {
                idle_timeout: REQUEST_BODY_IDLE_TIMEOUT,
                timeout: REQUEST_BODY_TIMEOUT,
            },
            shutdown_grace_timeout: SHUTDOWN_GRACE_TIMEOUT,
        }
    }
}

#[derive(Clone)]
struct HttpState {
    router: Router,
    body_limits: BodyReadLimits,
}

pub async fn run(port: u16) -> io::Result<()> {
    let router = Router::new()?;
    let address = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("DAW-AI is ready at http://{address}");
    println!("Sound graph: {}", router.project_path().display());
    serve_http(
        listener,
        router,
        shutdown_signal(),
        HttpServerConfig::default(),
    )
    .await
}

async fn serve_http(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send,
    config: HttpServerConfig,
) -> io::Result<()> {
    let connection_limiter = Limiter::new(config.max_connections);
    serve_http_with_limiter(listener, router, shutdown, config, connection_limiter).await
}

async fn serve_http_with_limiter(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send,
    config: HttpServerConfig,
    connection_limiter: Arc<Limiter>,
) -> io::Result<()> {
    let state = HttpState {
        router,
        body_limits: config.body_limits,
    };
    let mut connection_tasks = tokio::task::JoinSet::new();
    let (connection_shutdown, _) = tokio::sync::watch::channel(false);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        eprintln!("warning: failed to accept HTTP connection: {error}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let Some(connection_permit) = connection_limiter.acquire() else {
                    reject_overloaded_connection(stream).await;
                    continue;
                };
                let state = state.clone();
                let header_timeout = config.header_timeout;
                let mut shutdown = connection_shutdown.subscribe();
                connection_tasks.spawn(async move {
                    let _connection_permit = connection_permit;
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move {
                            Ok::<_, Infallible>(serve_request(state, request).await)
                        }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(header_timeout)
                        .keep_alive(false)
                        .max_buf_size(HTTP_BUFFER_BYTES);
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = shutdown.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                        _ = connection.as_mut() => {}
                    }
                });
            }
            completed = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    eprintln!("error: HTTP connection worker failed: {error}");
                }
            }
        }
    }

    let _ = connection_shutdown.send(true);
    let drain = async {
        while let Some(completed) = connection_tasks.join_next().await {
            if let Err(error) = completed {
                eprintln!("error: HTTP connection worker failed during shutdown: {error}");
            }
        }
    };
    if tokio::time::timeout(config.shutdown_grace_timeout, drain)
        .await
        .is_err()
    {
        connection_tasks.abort_all();
        while let Some(completed) = connection_tasks.join_next().await {
            if let Err(error) = completed
                && error.is_panic()
            {
                eprintln!("error: HTTP connection worker panicked during shutdown: {error}");
            }
        }
    }
    Ok(())
}

async fn reject_overloaded_connection(mut stream: tokio::net::TcpStream) {
    const BODY: &str = "{\"error\":\"server connection limit reached\"}";
    let response = format!(
        concat!(
            "HTTP/1.1 503 Service Unavailable\r\n",
            "Content-Type: application/json; charset=utf-8\r\n",
            "Content-Length: {}\r\n",
            "Cache-Control: no-store\r\n",
            "Connection: close\r\n",
            "\r\n",
            "{}"
        ),
        BODY.len(),
        BODY
    );
    let write = async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    };
    let _ = tokio::time::timeout(OVERLOAD_WRITE_TIMEOUT, write).await;
}

#[cfg(unix)]
async fn shutdown_signal() {
    let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn serve_request(state: HttpState, request: HttpRequest<Incoming>) -> HttpResponse<HttpBody> {
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let body = match read_request_body(body, state.body_limits).await {
        Ok(body) => body,
        Err(error) => {
            eprintln!(
                "warning: http request_id={request_id} outcome=rejected latency_ms={} error={}",
                started.elapsed().as_millis(),
                single_line(error.message())
            );
            return Response::json(error.status(), error_json(error.message())).into_http();
        }
    };
    let request = match Request::from_http(parts, &body) {
        Ok(request) => request,
        Err(error) => {
            eprintln!(
                "warning: http request_id={request_id} outcome=rejected latency_ms={} error={}",
                started.elapsed().as_millis(),
                single_line(&error)
            );
            return Response::json(400, error_json(&error)).into_http();
        }
    };
    match tokio::task::spawn_blocking(move || {
        dispatch_request(state.router, request, request_id, started)
    })
    .await
    {
        Ok(response) => response,
        Err(error) => {
            eprintln!("error: HTTP request worker failed: {error}");
            Response::json(500, error_json("request processing failed")).into_http()
        }
    }
}

enum RequestBodyReadError {
    TimedOut,
    TooLarge,
    Read,
}

impl RequestBodyReadError {
    fn status(&self) -> u16 {
        match self {
            Self::TimedOut => 408,
            Self::TooLarge => 413,
            Self::Read => 400,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::TimedOut => "request body timed out",
            Self::TooLarge => "request is too large",
            Self::Read => "could not read request body",
        }
    }
}

async fn read_request_body<B>(
    mut body: B,
    limits: BodyReadLimits,
) -> Result<Bytes, RequestBodyReadError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let deadline = tokio::time::Instant::now() + limits.timeout;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(RequestBodyReadError::TimedOut);
        }
        let next = tokio::time::timeout(limits.idle_timeout.min(remaining), body.frame())
            .await
            .map_err(|_| RequestBodyReadError::TimedOut)?;
        let Some(frame) = next else {
            return Ok(Bytes::from(bytes));
        };
        let frame = frame.map_err(|_| RequestBodyReadError::Read)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let Some(length) = bytes.len().checked_add(data.len()) else {
            return Err(RequestBodyReadError::TooLarge);
        };
        if length > MAX_REQUEST_BYTES {
            return Err(RequestBodyReadError::TooLarge);
        }
        bytes.extend_from_slice(&data);
    }
}

pub(super) struct ChannelWriter {
    sender: tokio::sync::mpsc::Sender<Result<Bytes, io::Error>>,
}

struct ReceiverBody {
    receiver: tokio::sync::mpsc::Receiver<Result<Bytes, io::Error>>,
    permit: Option<Permit>,
}

impl HyperBody for ReceiverBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let body = self.get_mut();
        match body.receiver.poll_recv(context) {
            Poll::Ready(Some(result)) => Poll::Ready(Some(result.map(Frame::data))),
            Poll::Ready(None) => {
                body.permit.take();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        const STREAM_CHUNK_BYTES: usize = 64 * 1024;
        let length = buffer.len().min(STREAM_CHUNK_BYTES);
        if length == 0 {
            return Ok(0);
        }
        self.sender
            .blocking_send(Ok(Bytes::copy_from_slice(&buffer[..length])))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "HTTP client disconnected"))?;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn streaming_body(
    permit: Permit,
    produce: impl FnOnce(&mut ChannelWriter, &dyn Fn() -> bool) -> io::Result<()> + Send + 'static,
) -> HttpBody {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    let cancellation = sender.clone();
    tokio::task::spawn_blocking(move || {
        let mut output = ChannelWriter { sender };
        let cancelled = || cancellation.is_closed();
        if let Err(error) = produce(&mut output, &cancelled) {
            if error.kind() != io::ErrorKind::BrokenPipe {
                eprintln!("streaming response failed: {error}");
            }
            let kind = error.kind();
            let message = error.to_string();
            let _ = output
                .sender
                .blocking_send(Err(io::Error::new(kind, message)));
        }
    });
    ReceiverBody {
        receiver,
        permit: Some(permit),
    }
    .boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::request;
    use std::thread;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn total_body_deadline_bounds_clients_that_keep_trickling_data() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let producer = tokio::spawn(async move {
            loop {
                if sender
                    .send(Ok::<_, io::Error>(Bytes::from_static(b"x")))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let body = ReceiverBody {
            receiver,
            permit: None,
        }
        .boxed_unsync();
        let result = read_request_body(
            body,
            BodyReadLimits {
                idle_timeout: Duration::from_millis(50),
                timeout: Duration::from_millis(75),
            },
        )
        .await;
        assert!(matches!(result, Err(RequestBodyReadError::TimedOut)));
        producer.await.expect("body producer");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_header_and_body_timeouts_preserve_capacity() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let connection_limiter = Limiter::new(1);
        let server = tokio::spawn(serve_http_with_limiter(
            listener,
            Router::demo(),
            async move {
                let _ = stopped.await;
            },
            HttpServerConfig {
                max_connections: 1,
                header_timeout: Duration::from_millis(75),
                body_limits: BodyReadLimits {
                    idle_timeout: Duration::from_millis(75),
                    timeout: Duration::from_millis(250),
                },
                shutdown_grace_timeout: Duration::from_millis(75),
            },
            Arc::clone(&connection_limiter),
        ));

        let mut missing_headers = tokio::net::TcpStream::connect(address)
            .await
            .expect("header-stalled connection");
        tokio::time::sleep(Duration::from_millis(15)).await;
        let mut header_overload = tokio::net::TcpStream::connect(address)
            .await
            .expect("header overload connection");
        let header_overload = read_test_connection(&mut header_overload).await;
        assert!(header_overload.starts_with("HTTP/1.1 503"));
        assert!(
            read_test_connection(&mut missing_headers).await.is_empty(),
            "an incomplete header cannot receive an HTTP response"
        );
        wait_for_test_capacity(&connection_limiter).await;

        let mut missing_body = tokio::net::TcpStream::connect(address)
            .await
            .expect("body-stalled connection");
        missing_body
            .write_all(b"POST /api/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\n\r\n")
            .await
            .expect("partial request");
        tokio::time::sleep(Duration::from_millis(15)).await;
        let mut body_overload = tokio::net::TcpStream::connect(address)
            .await
            .expect("body overload connection");
        let body_overload = read_test_connection(&mut body_overload).await;
        assert!(body_overload.starts_with("HTTP/1.1 503"));
        let timed_out = read_test_connection(&mut missing_body).await;
        assert!(timed_out.starts_with("HTTP/1.1 408"));
        wait_for_test_capacity(&connection_limiter).await;

        let mut health = tokio::net::TcpStream::connect(address)
            .await
            .expect("health connection");
        health
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("health request");
        let health = read_test_connection(&mut health).await;
        assert!(health.starts_with("HTTP/1.1 200"));

        shutdown.send(()).expect("stop test server");
        server
            .await
            .expect("test server task")
            .expect("test server result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_a_long_lived_stream_after_the_grace_period() {
        let mut router = Router::demo();
        let limiter = Limiter::new(1);
        router.stream_limiter = Arc::clone(&limiter);
        let mut project = router.lock_project().project().clone();
        project.duration = 300.0;
        let version = project.version;
        router.lock_project().replace_project_for_test(project);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_http(
            listener,
            router,
            async move {
                let _ = stopped.await;
            },
            HttpServerConfig {
                max_connections: 1,
                header_timeout: Duration::from_secs(1),
                body_limits: BodyReadLimits {
                    idle_timeout: Duration::from_secs(1),
                    timeout: Duration::from_secs(2),
                },
                shutdown_grace_timeout: Duration::from_millis(75),
            },
        ));

        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("playback connection");
        stream
            .write_all(
                format!(
                    "GET /api/audio-stream/test-audio-token/{version}/0 HTTP/1.1\r\n\
                     Host: 127.0.0.1\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("playback request");
        let mut response = [0_u8; 1024];
        let response_length =
            tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
                .await
                .expect("playback response timed out")
                .expect("playback response");
        assert!(response[..response_length].starts_with(b"HTTP/1.1 200"));
        assert!(limiter.acquire().is_none());

        let started = tokio::time::Instant::now();
        shutdown.send(()).expect("stop test server");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server shutdown exceeded its bound")
            .expect("test server task")
            .expect("test server result");
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(limiter.acquire().is_some());
    }

    async fn read_test_connection(stream: &mut tokio::net::TcpStream) -> String {
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("HTTP connection timed out")
            .expect("read HTTP connection");
        String::from_utf8(response).expect("ASCII HTTP response")
    }

    async fn wait_for_test_capacity(limiter: &Arc<Limiter>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(permit) = limiter.acquire() {
                    drop(permit);
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("HTTP connection capacity was not released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_streaming_response_cancels_its_producer() {
        let (events, received) = std::sync::mpsc::channel();
        let limiter = Limiter::new(1);
        let permit = limiter.acquire().expect("stream permit");
        let body = streaming_body(permit, move |_, is_cancelled| {
            events.send("started").expect("event receiver");
            while !is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            events.send("cancelled").expect("event receiver");
            Ok(())
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(1)), Ok("started"));
        assert!(limiter.acquire().is_none());
        drop(body);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)),
            Ok("cancelled")
        );
        assert!(limiter.acquire().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_capacity_rejects_overload_and_releases_on_drop() {
        let mut router = Router::demo();
        let limiter = Limiter::new(1);
        let held = limiter.acquire().expect("held stream permit");
        router.stream_limiter = Arc::clone(&limiter);
        let version = router.lock_project().project().version;
        let mut stream_request = request(
            "GET",
            &format!("/api/audio-stream/test-audio-token/{version}/0"),
            "",
        );
        stream_request
            .headers
            .insert("range".to_owned(), "bytes=0-43".to_owned());

        let overloaded = router.playback_response(&stream_request, None);
        assert_eq!(overloaded.status(), 503);
        drop(held);

        let response = router.playback_response(&stream_request, None);
        assert_eq!(response.status(), 206);
        assert!(limiter.acquire().is_none());
        drop(response);
        assert!(limiter.acquire().is_some());
    }
}
