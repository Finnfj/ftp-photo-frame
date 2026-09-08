//! A small web app for managing the photos on the FTP server from a phone or a laptop on the same
//! network.
//!
//! Everything protocol-specific lives here; [routes] decides what a request means, and [files] is
//! the only place that turns what a browser sent into an FTP path. The split is deliberate:
//! [routes] and [files] are plain functions over values and are unit-tested without a socket,
//! while this module is the part that cannot be.

pub mod files;
pub mod routes;
pub mod thumbnails;

use std::{io::Read, net::SocketAddr, time::Duration};

use anyhow::{Result, anyhow};
use tiny_http::{Header, Request, Response, Server};

pub use crate::slideshow::Refresh;

use crate::{
    ftp::{FtpSession, FtpTransport, SuppaFtpTransport},
    web::{
        files::FtpFiles,
        routes::{AppState, Method, WebConfig},
        thumbnails::Thumbnails,
    },
};

/// Refused before an FTP data connection is opened. Generous enough for a photo straight off a
/// camera, small enough that a stray request cannot tie the frame up for minutes.
pub const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// How much memory rendered thumbnails may take up together
const THUMBNAIL_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Everything the serving thread needs, built before it is spawned so that nothing borrows the
/// command line
pub fn state(
    config: WebConfig,
    session: FtpSession,
    timeout: Duration,
    refresh: Refresh,
) -> AppState<SuppaFtpTransport> {
    AppState {
        config,
        files: FtpFiles::new(session, SuppaFtpTransport::new(timeout), refresh),
        thumbnails: Thumbnails::new(THUMBNAIL_CACHE_BYTES),
    }
}

/// Binds the listening socket.
///
/// Called before SDL takes over the screen, so that a port already in use is reported on the
/// console rather than silently leaving the frame unmanageable.
pub fn bind(address: SocketAddr) -> Result<Server> {
    let server = Server::http(address).map_err(|error| anyhow!("{error}"))?;
    log::warn!(
        "The web app is listening on http://{address}/ . The connection is not encrypted, so \
         anyone on this network can read the password and the photos."
    );
    Ok(server)
}

/// Serves requests until the process ends.
///
/// Runs on a detached thread rather than one of the scoped threads in [crate::run], which are
/// joined on the way out: a thread parked in [Server::recv] would never return and the frame would
/// hang on quit with the screen still taken over. There is nothing to clean up that ending the
/// process does not do — the server times the abandoned session out exactly as it does today when
/// the frame is switched off.
///
/// Single-threaded on purpose. One FTP session can carry one transfer at a time, so handling
/// requests one after another is not a limitation but the shape of the problem; it also means the
/// gallery cannot start ten downloads while an upload is running.
pub fn serve<T: FtpTransport>(server: Server, state: AppState<T>) {
    for mut request in server.incoming_requests() {
        let description = format!("{} {}", request.method(), request.url());
        let response = match head_of(&request) {
            Err(response) => response,
            Ok(head) => {
                /* Capped here rather than in a handler, so that there is exactly one place a
                 * request body can be read and exactly one place the limit is enforced */
                let mut body = request.as_reader().take(MAX_UPLOAD_BYTES);
                routes::handle(&state, &head, &mut body)
            }
        };
        log::debug!("{description} -> {}", response.status);
        respond(request, response);
    }
}

/// Turns what [tiny_http] hands over into the plain value [routes::handle] works on
fn head_of(request: &Request) -> Result<routes::Request, routes::Response> {
    let method = match request.method() {
        tiny_http::Method::Get => Some(Method::Get),
        tiny_http::Method::Post => Some(Method::Post),
        /* Left for the route table to answer, so that a verb nobody supports on a route that does
         * not exist is still reported as a missing route */
        _ => None,
    };
    let header = |name: &'static str| header_of(request, name);

    let announced_length = request.body_length().map(|length| length as u64);
    if announced_length.is_some_and(|length| length > MAX_UPLOAD_BYTES) {
        return Err(routes::Response::error(
            413,
            &format!(
                "Photos larger than {} MB cannot be uploaded",
                MAX_UPLOAD_BYTES / 1024 / 1024
            ),
        ));
    }

    Ok(routes::Request {
        method,
        /* tiny_http reports the request target, which is a path with an optional query rather than
         * a whole URL. routes::Request::query parses it against a placeholder origin. */
        target: request.url().to_string(),
        authorization: header("Authorization"),
        origin: header("Origin"),
        host: header("Host"),
        custom_header: header(routes::CUSTOM_HEADER),
        announced_length,
    })
}

fn header_of(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

fn respond(request: Request, response: routes::Response) {
    let mut headers = vec![header("Content-Type", response.content_type)];
    headers.push(header("X-Content-Type-Options", "nosniff"));
    for (name, value) in &response.headers {
        headers.push(header(name, value));
    }
    let http_response = Response::new(
        response.status.into(),
        headers,
        std::io::Cursor::new(response.body),
        None,
        None,
    );
    if let Err(error) = request.respond(http_response) {
        /* A browser that navigated away mid-response is ordinary, not worth more than a debug line */
        log::debug!("Could not send the response: {error}");
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("header names and values are literals in this crate")
}

/// Covers what only a real socket can: that the server binds, that headers and the body survive
/// the trip through [tiny_http], and that a response comes back the way it was built. The route
/// table itself is covered without a socket in [routes].
#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::net::TcpStream;

    use crate::{
        ftp::{Credentials, DirEntry, EntryKind, FtpAddress, MockFtpTransport},
        web::routes::CUSTOM_HEADER,
    };

    const USER: &str = "frame";
    const PASSWORD: &str = "hunter2hunter2";

    /// Starts a server on a port the operating system picks, so that the test cannot collide with
    /// anything else running on the machine
    fn start(transport: MockFtpTransport) -> (SocketAddr, Refresh) {
        let server = Server::http("127.0.0.1:0").expect("binding to an ephemeral port");
        let address = server
            .server_addr()
            .to_ip()
            .expect("the server was asked for an IP address");
        let refresh = Refresh::default();
        let state = AppState {
            config: WebConfig {
                user: USER.to_string(),
                password: PASSWORD.to_string(),
                bind: address,
            },
            files: FtpFiles::new(fake_session(), transport, refresh.clone()),
            thumbnails: Thumbnails::new(1024 * 1024),
        };
        std::thread::spawn(move || serve(server, state));
        (address, refresh)
    }

    fn fake_session() -> FtpSession {
        FtpSession {
            address: FtpAddress {
                host: "fake.ftp.addr".to_string(),
                port: 21,
                tls: false,
            },
            credentials: Credentials {
                user: "joe".to_string(),
                password: "secret".to_string(),
            },
            root: "/Photos".to_string(),
        }
    }

    /// A minimal HTTP client, so that the test does not depend on how `reqwest` decides to phrase
    /// a request. Answers with the status line and the body.
    fn send(address: SocketAddr, request: &str, body: &[u8]) -> (String, String) {
        let mut stream = TcpStream::connect(address).expect("connecting to the web app");
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        let mut answer = String::new();
        std::io::Read::read_to_string(&mut stream, &mut answer).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
        (head.to_string(), body.to_string())
    }

    fn authorization() -> String {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
        format!("Basic {}", BASE64.encode(format!("{USER}:{PASSWORD}")))
    }

    /// Serves a made-up album on a fixed port, so that the page can be opened in a browser while
    /// working on it. Run with
    /// `cargo test --all-features -- --ignored --nocapture serve_a_fake_album`
    #[test]
    #[ignore = "runs until interrupted, for looking at the page"]
    fn serve_a_fake_album() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport.expect_list_dir().returning(|path| {
            let directory = |name: &str| DirEntry {
                name: name.to_string(),
                kind: EntryKind::Directory,
                modified: None,
            };
            let photo = |name: &str| DirEntry {
                name: name.to_string(),
                kind: EntryKind::File,
                modified: None,
            };
            Ok(match path {
                "/Photos" => vec![
                    directory("summer"),
                    directory("empty"),
                    photo("IMG_0001.jpg"),
                    photo("IMG_0002.jpg"),
                    photo("A very long file name that has to be cut off somewhere.jpg"),
                ],
                "/Photos/summer" => (1..=8).map(|n| photo(&format!("beach_{n}.jpg"))).collect(),
                _ => vec![],
            })
        });
        transport
            .expect_retrieve()
            .returning(|_| Ok(bytes::Bytes::from_static(crate::web::thumbnails::TEST_JPEG)));

        let server = Server::http("127.0.0.1:8099").expect("port 8099 has to be free");
        let state = AppState {
            config: WebConfig {
                user: USER.to_string(),
                password: PASSWORD.to_string(),
                bind: "127.0.0.1:8099".parse().unwrap(),
            },
            files: FtpFiles::new(fake_session(), transport, Refresh::default()),
            thumbnails: Thumbnails::new(1024 * 1024),
        };
        println!("Open http://127.0.0.1:8099/ and log in as {USER} / {PASSWORD}");
        serve(server, state);
    }

    #[test]
    fn the_server_answers_a_request_without_credentials_with_a_login_prompt() {
        let (address, _) = start(MockFtpTransport::new());

        let (head, _) = send(
            address,
            &format!("GET /api/list HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
            b"",
        );

        assert!(head.starts_with("HTTP/1.1 401"), "was {head}");
        assert!(head.contains("WWW-Authenticate"), "was {head}");
    }

    #[test]
    fn the_server_carries_headers_and_a_body_through_to_a_handler() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport.expect_list_dir().returning(|_| Ok(vec![]));
        transport.expect_store().returning(|_, source| {
            let mut buffer = vec![];
            source.read_to_end(&mut buffer).unwrap();
            assert_eq!(buffer, b"photo bytes");
            Ok(buffer.len() as u64)
        });
        transport.expect_rename().returning(|_, _| Ok(()));
        let (address, refresh) = start(transport);
        let body = b"photo bytes";

        let (head, answer) = send(
            address,
            &format!(
                "POST /api/upload?path=&name=IMG_1234.jpg HTTP/1.1\r\n\
                 Host: {address}\r\n\
                 Authorization: {}\r\n\
                 {CUSTOM_HEADER}: 1\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                authorization(),
                body.len(),
            ),
            body,
        );

        assert!(head.starts_with("HTTP/1.1 201"), "was {head}");
        assert!(answer.contains("IMG_1234.jpg"), "was {answer}");
        assert!(refresh.take(), "an upload has to reach the slideshow");
    }

    #[test]
    fn the_server_serves_the_page_it_was_built_with() {
        let (address, _) = start(MockFtpTransport::new());

        let (head, body) = send(
            address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {address}\r\nAuthorization: {}\r\n\
                 Connection: close\r\n\r\n",
                authorization()
            ),
            b"",
        );

        assert!(head.starts_with("HTTP/1.1 200"), "was {head}");
        assert!(head.contains("Content-Security-Policy"), "was {head}");
        assert!(head.contains("X-Content-Type-Options"), "was {head}");
        assert!(body.contains("<title>Photo frame</title>"), "was {body}");
    }

    #[test]
    fn the_server_refuses_an_upload_that_announces_more_than_the_limit() {
        let mut transport = MockFtpTransport::new();
        /* Refused before an FTP data connection is opened, rather than after the bytes arrived */
        transport.expect_store().never();
        let (address, _) = start(transport);

        let (head, _) = send(
            address,
            &format!(
                "POST /api/upload?path=&name=a.jpg HTTP/1.1\r\n\
                 Host: {address}\r\n\
                 Authorization: {}\r\n\
                 {CUSTOM_HEADER}: 1\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                authorization(),
                MAX_UPLOAD_BYTES + 1,
            ),
            b"",
        );

        assert!(head.starts_with("HTTP/1.1 413"), "was {head}");
    }

    #[test]
    fn the_server_keeps_answering_after_a_request_it_could_not_handle() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport.expect_list_dir().returning(|_| {
            Ok(vec![DirEntry {
                name: "a.jpg".to_string(),
                kind: EntryKind::File,
                modified: None,
            }])
        });
        let (address, _) = start(transport);

        send(
            address,
            &format!("DELETE /api/list HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
            b"",
        );
        let (head, body) = send(
            address,
            &format!(
                "GET /api/list HTTP/1.1\r\nHost: {address}\r\nAuthorization: {}\r\n\
                 Connection: close\r\n\r\n",
                authorization()
            ),
            b"",
        );

        assert!(head.starts_with("HTTP/1.1 200"), "was {head}");
        assert!(body.contains("a.jpg"), "was {body}");
    }
}
