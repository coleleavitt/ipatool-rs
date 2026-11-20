use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use super::*;

#[test]
fn parses_six_byte_macs_like_go() {
    assert_eq!(
        parse_mac("00:00:5e:00:53:01"),
        Some([0, 0, 0x5e, 0, 0x53, 1])
    );
    assert_eq!(
        parse_mac("00-00-5E-00-53-01"),
        Some([0, 0, 0x5e, 0, 0x53, 1])
    );
    assert_eq!(parse_mac("0000.5e00.5301"), Some([0, 0, 0x5e, 0, 0x53, 1]));
    for bad in [
        "",
        "00:00:5e:00:53",
        "00:00-5e:00:53:01",
        "0g:00:5e:00:53:01",
        "02:00:5e:10:00:00:00:01",
        "+0:00:5e:00:53:01",
    ] {
        assert_eq!(parse_mac(bad), None, "{bad}");
    }
}

/// Go's `httptest.Server` handler from `backend_test.go`, over HTTP/1.1 keep-alive.
pub(super) struct Server {
    pub url: String,
}

fn serve_connection(stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| String::new(), |a| a.to_string());
    let mut reader = BufReader::new(stream.try_clone().expect("clone the test socket"));
    let mut stream = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
        let (mut cookie, mut close, mut length) = (String::new(), false, 0usize);
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap_or((header, ""));
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "cookie" => value.clone_into(&mut cookie),
                "connection" => close = value.eq_ignore_ascii_case("close"),
                "content-length" => length = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        let mut body = vec![0; length];
        if std::io::Read::read_exact(&mut reader, &mut body).is_err() {
            return;
        }
        let common = format!("X-Connection: {peer}\r\nX-Received-Cookie: {cookie}\r\n");
        let path = path.split('?').next().unwrap_or("/");
        let response = match path {
            "/slow" => {
                thread::sleep(Duration::from_secs(2));
                return;
            }
            "/stream" => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\n{common}Transfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n"
                );
                let _ = stream.flush();
                thread::sleep(Duration::from_secs(5));
                return;
            }
            "/cookie" => format!(
                "HTTP/1.1 200 OK\r\n{common}Set-Cookie: session=fixture; Path=/\r\nContent-Length: 16\r\n\r\nfixture response"
            ),
            "/redirect" => format!(
                "HTTP/1.1 302 Found\r\n{common}Location: /target\r\nContent-Length: 0\r\n\r\n"
            ),
            _ => format!("HTTP/1.1 200 OK\r\n{common}Content-Length: 16\r\n\r\nfixture response"),
        };
        if stream.write_all(response.as_bytes()).is_err() || close {
            return;
        }
    }
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test server");
        let url = format!("http://{}", listener.local_addr().expect("test address"));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || serve_connection(stream));
            }
        });
        Self { url }
    }
}

fn backend() -> Backend {
    let jar: Arc<dyn CookieStore> = Arc::new(reqwest::cookie::Jar::default());
    Backend::new(Some(jar), None).expect("backend")
}

fn request(server: &Server, path: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".to_owned(),
        url: format!("{}{path}", server.url),
        ..HttpRequest::default()
    }
}

fn get(backend: &Backend, server: &Server, operation: &str, path: &str) -> HttpResponse {
    let mut response = (backend.request_transport(operation, None))(request(server, path))
        .expect("transport response");
    response.read_body().expect("read the body");
    response
}

#[test]
fn reuses_a_consumed_connection_across_api_and_download_clients() {
    let (backend, server) = (backend(), Server::start());
    let first = get(&backend, &server, "request", "/cookie");
    let second = get(&backend, &server, "transfer", "/");
    assert_eq!(
        second.headers.get("X-Connection"),
        first.headers.get("X-Connection")
    );
    assert_eq!(second.headers.get("X-Received-Cookie"), "session=fixture");
    let third = get(&backend, &server, "bag", "/");
    assert_eq!(
        third.headers.get("X-Connection"),
        first.headers.get("X-Connection")
    );
}

#[test]
fn keeps_authentication_attempts_separate_without_closing_the_api_pool() {
    let (backend, server) = (backend(), Server::start());
    let api = get(&backend, &server, "request", "/");
    let first = get(&backend, &server, "authenticate", "/");
    let second = get(&backend, &server, "authenticate", "/");
    assert_ne!(
        first.headers.get("X-Connection"),
        api.headers.get("X-Connection")
    );
    assert_ne!(
        second.headers.get("X-Connection"),
        first.headers.get("X-Connection")
    );
    let again = get(&backend, &server, "request", "/");
    assert_eq!(
        again.headers.get("X-Connection"),
        api.headers.get("X-Connection")
    );
}

#[test]
fn returns_redirects_to_the_framework() {
    let (backend, server) = (backend(), Server::start());
    let response = get(&backend, &server, "request", "/redirect");
    assert_eq!(response.status, 302);
    assert_eq!(
        response.location().map(|u| u.to_string()).ok(),
        Some(format!("{}/target", server.url))
    );
}

#[test]
fn preserves_explicit_cookie_headers() {
    let (backend, server) = (backend(), Server::start());
    get(&backend, &server, "request", "/cookie");
    let mut explicit = request(&server, "/");
    explicit.headers.set("Cookie", "explicit=value");
    let response = (backend.request_transport("", None))(explicit).expect("response");
    assert_eq!(response.headers.get("X-Received-Cookie"), "explicit=value");
}

#[test]
fn applies_timeouts_per_request_without_poisoning_subsequent_requests() {
    let (backend, server) = (backend(), Server::start());
    let err =
        (backend.request_transport("", Some(Duration::from_millis(20))))(request(&server, "/slow"))
            .expect_err("the slow request times out");
    assert!(
        err.to_string().starts_with("store HTTP transport: "),
        "{err}"
    );
    assert_eq!(get(&backend, &server, "request", "/").status, 200);
}

#[test]
fn dropping_an_active_stream_keeps_unrelated_pooled_connections() {
    let (backend, server) = (backend(), Server::start());
    let api = get(&backend, &server, "request", "/");
    let stream = (backend.request_transport("transfer", None))(request(&server, "/stream"))
        .expect("stream response");
    let other = get(&backend, &server, "request", "/");
    assert_ne!(
        other.headers.get("X-Connection"),
        api.headers.get("X-Connection")
    );
    drop(stream);
    let again = get(&backend, &server, "request", "/");
    assert_eq!(
        again.headers.get("X-Connection"),
        other.headers.get("X-Connection")
    );
}

#[test]
fn sends_go_client_defaults_and_decodes_gzip_transparently() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read");
            if line.trim_end().is_empty() {
                break;
            }
            head.push(line.trim_end().to_ascii_lowercase());
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"decoded").expect("gzip");
        let body = encoder.finish().expect("gzip");
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .expect("write");
        stream.write_all(&body).expect("write");
        head
    });
    let backend = Backend::new(None, None).expect("backend");
    let mut response = (backend.request_transport("authenticate", None))(HttpRequest {
        method: "GET".to_owned(),
        url,
        ..HttpRequest::default()
    })
    .expect("response");
    assert_eq!(response.read_body().expect("body"), b"decoded");
    assert_eq!(response.headers.get("Content-Encoding"), "");
    assert_eq!(response.headers.get("Content-Length"), "");
    assert_eq!(response.content_length, -1);
    let head = handle.join().expect("server");
    for expected in [
        "user-agent: go-http-client/1.1",
        "accept-encoding: gzip",
        "connection: close",
    ] {
        assert!(
            head.iter().any(|line| line == expected),
            "{expected} in {head:?}"
        );
    }
}

#[test]
fn sends_a_zero_content_length_for_an_empty_post_like_go() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let handle = thread::spawn(move || {
        let mut heads = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut head = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read");
                if line.trim_end().is_empty() {
                    break;
                }
                head.push(line.trim_end().to_ascii_lowercase());
            }
            let mut stream = stream;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").expect("write");
            heads.push(head);
        }
        heads
    });
    let backend = Backend::new(None, None).expect("backend");
    for method in ["POST", "GET"] {
        (backend.request_transport("authenticate", None))(HttpRequest {
            method: method.to_owned(),
            url: url.clone(),
            ..HttpRequest::default()
        })
        .expect("response");
    }
    let heads = handle.join().expect("server");
    let length = |head: &[String]| {
        head.iter()
            .find_map(|line| line.strip_prefix("content-length: "))
            .map(str::to_owned)
    };
    assert_eq!(length(&heads[0]).as_deref(), Some("0"), "{:?}", heads[0]);
    assert_eq!(length(&heads[1]), None, "{:?}", heads[1]);
}

#[test]
fn honors_the_public_bag_request_timeout() {
    let (backend, server) = (Arc::new(backend()), Server::start());
    let err = backend
        .client("bag-direct")
        .send(
            StoreRequest {
                url: format!("{}/slow", server.url),
                method: "GET".to_owned(),
                timeout: Some(Duration::from_millis(20)),
                ..StoreRequest::default()
            },
            Vec::new(),
        )
        .expect_err("the slow bag request times out");
    assert!(
        err.to_string().starts_with("store HTTP transport: "),
        "{err}"
    );
}

#[test]
fn rejects_bad_store_identities() {
    let backend = Backend::new(None, None).expect("backend");
    let guid = |value: &str| BTreeMap::from([("guid".to_owned(), Value::String(value.to_owned()))]);
    assert_eq!(
        backend.hardware(Some(&guid("0A0b0C0d0E0f"))).ok(),
        Some(vec![10, 11, 12, 13, 14, 15])
    );
    for bad in ["0a0b", "+a0b0c0d0e0f", "zz0b0c0d0e0f"] {
        let err = backend.hardware(Some(&guid(bad))).expect_err(bad);
        assert_eq!(err.to_string(), "invalid Store hardware identity");
    }
    let err = backend.hardware(None).expect_err("no machine identity");
    assert_eq!(err.to_string(), "store machine identity is not configured");
    let backend =
        Backend::new(None, Some(Box::new(|| Ok("not-a-mac".to_owned())))).expect("backend");
    assert_eq!(
        backend.hardware(None).expect_err("bad mac").to_string(),
        "invalid Store machine address"
    );
}

/// A download through a fresh guest and the backend's real HTTP client. Opt-in:
/// `IPATOOL_TEST_SAP_CACHE=1` (needs the SAP asset cache filled).
#[test]
fn transfers_through_the_guest() {
    if std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_none() {
        return;
    }
    let server = Server::start();
    let backend = Arc::new(
        Backend::new(None, Some(Box::new(|| Ok("00:01:02:03:04:05".to_owned())))).expect("backend"),
    );
    let mut response = backend
        .client("transfer")
        .execute_http(request(&server, "/package"))
        .expect("streamed response");
    assert_eq!(response.status, 200);
    assert_eq!(response.read_body().expect("body"), b"fixture response");
    let err = backend
        .client("request")
        .execute_http(request(&server, "/"))
        .expect_err("API requests must go to Apple over HTTPS");
    assert!(
        err.to_string().contains("unsupported transport URL"),
        "{err}"
    );
}
