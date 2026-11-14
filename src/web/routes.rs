//! What a request means, and what to answer.
//!
//! Everything here works on plain values rather than on [tiny_http] types, so that the whole route
//! table can be exercised without opening a socket.

use std::{io::Read, net::SocketAddr, thread::sleep, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::{
    ftp::FtpTransport,
    http::Url,
    web::{
        files::{Conflict, FtpFiles, PathRejected},
        thumbnails::Thumbnails,
    },
};

/// How much of a photo to fetch when looking for the preview a camera embedded in it.
///
/// The preview sits in the EXIF block near the start of the file. 128 KiB is generous for that and
/// still a fortieth of a typical photo from a drone or a decent camera.
const PREVIEW_HEAD_BYTES: usize = 128 * 1024;

/// Required on every request that changes something.
///
/// This is what stops another site from having the browser delete photos: HTTP Basic credentials
/// are attached to a cross-site form submission automatically, and unlike a cookie there is no
/// `SameSite` to lean on. A form cannot set a header at all, and a `fetch` that sets one has to
/// pass a preflight this server never answers.
pub const CUSTOM_HEADER: &str = "X-Photo-Frame";

/// Only what a browser needs; the hand-written dispatch is not worth widening for verbs the page
/// never sends
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Post,
}

/// A request, minus its body
#[derive(Debug, Default)]
pub struct Request {
    pub method: Option<Method>,
    /// Path and query as sent, for example `/api/list?path=summer`
    pub target: String,
    pub authorization: Option<String>,
    pub origin: Option<String>,
    pub host: Option<String>,
    pub custom_header: Option<String>,
    /// `Content-Length`, which an upload needs so that a transfer ending early can be told from
    /// one that finished
    pub announced_length: Option<u64>,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            /* Nothing the web app answers is worth keeping, except a thumbnail, which says so for
             * itself */
            headers: vec![("Cache-Control", "no-store".to_string())],
            body,
        }
    }

    fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(
            status,
            "application/json; charset=utf-8",
            value.to_string().into_bytes(),
        )
    }

    /// The one shape every failure takes, so that the page has a single thing to display
    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, &json!({ "error": message }))
    }

    fn no_content() -> Self {
        Self::new(204, "application/json; charset=utf-8", vec![])
    }

    fn page(html: &str) -> Self {
        let mut response = Self::new(200, "text/html; charset=utf-8", html.as_bytes().to_vec());
        /* The page loads nothing from anywhere else, so it can afford to forbid all of it. This is
         * also what contains a photo whose file name was crafted to look like markup. */
        response.headers.push((
            "Content-Security-Policy",
            /* connect-src has to be stated: it falls back to default-src, which would leave the
             * page unable to call this very server */
            "default-src 'none'; connect-src 'self'; img-src 'self'; style-src 'unsafe-inline'; \
             script-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'"
                .to_string(),
        ));
        response
    }

    fn thumbnail(body: Vec<u8>) -> Self {
        let mut response = Self::new(200, "image/jpeg", body);
        /* A photo never changes under a given path: an upload picks a free name rather than
         * overwriting, so the browser may keep what it has */
        response.headers = vec![("Cache-Control", "private, max-age=86400".to_string())];
        response
    }

    /// The photo itself, as it is stored. Nothing is re-encoded: the frame is on the same network
    /// as whatever is asking, and rendering a second size would mean a second cache and a second
    /// chance to get EXIF orientation wrong.
    fn photo(body: Vec<u8>, content_type: &'static str) -> Self {
        let mut response = Self::new(200, content_type, body);
        /* Same reasoning as a preview: the bytes behind a path do not change */
        response.headers = vec![("Cache-Control", "private, max-age=86400".to_string())];
        response
    }

    fn unauthorized() -> Self {
        let mut response = Self::error(401, "Log in to manage the photos on this frame");
        response.headers.push((
            "WWW-Authenticate",
            r#"Basic realm="Photo frame", charset="UTF-8""#.to_string(),
        ));
        response
    }
}

/// How the web app was started
#[derive(Clone, Debug)]
pub struct WebConfig {
    pub user: String,
    pub password: String,
    pub bind: SocketAddr,
}

/// Everything a request may need
pub struct AppState<T> {
    pub config: WebConfig,
    pub files: FtpFiles<T>,
    pub thumbnails: Thumbnails,
}

#[cfg(not(target_os = "windows"))]
const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
#[cfg(target_os = "windows")]
const INDEX_HTML: &str = include_str!("..\\..\\assets\\web\\index.html");

/// Answers one request.
///
/// `body` is only read by the upload route, and is capped by the caller.
pub fn handle<T: FtpTransport>(
    state: &AppState<T>,
    request: &Request,
    body: &mut dyn Read,
) -> Response {
    if !is_allowed_host(request.host.as_deref(), state.config.bind.port()) {
        /* A name that resolves to this frame but belongs to somebody else is how a page on the
         * internet turns itself into this origin and reads the album */
        return Response::error(421, "This frame does not answer to that name");
    }
    if !authorize(&state.config, request.authorization.as_deref()) {
        /* On a single-threaded accept loop this throttles guessing by construction */
        sleep(FAILED_LOGIN_DELAY);
        return Response::unauthorized();
    }

    let Some(url) = parse_target(&request.target) else {
        return Response::error(400, "Malformed request");
    };
    let parameter = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };

    match (request.method, url.path()) {
        (Some(Method::Get), "/") => Response::page(INDEX_HTML),

        (Some(Method::Get), "/api/list") => match state.files.list(&raw(&url, "path")) {
            Ok(listing) => Response::json(
                200,
                &json!({
                    "path": parameter("path").unwrap_or_default(),
                    "directories": listing
                        .directories
                        .iter()
                        .map(|name| json!({ "name": name }))
                        .collect::<Vec<_>>(),
                    "photos": listing
                        .photos
                        .iter()
                        .map(|photo| json!({
                            "name": photo.name,
                            "modified": photo.modified.map(|at| at.to_rfc3339()),
                        }))
                        .collect::<Vec<_>>(),
                }),
            ),
            Err(error) => failed(error),
        },

        (Some(Method::Get), "/api/thumbnail") => {
            let path = raw(&url, "path");
            match state.thumbnails.get(
                &path,
                || state.files.read_head(&path, PREVIEW_HEAD_BYTES),
                || state.files.read(&path),
            ) {
                Ok(bytes) => Response::thumbnail(bytes.to_vec()),
                Err(error) => failed(error),
            }
        }

        (Some(Method::Get), "/api/photo") => {
            let path = raw(&url, "path");
            match state.files.read(&path) {
                Ok(bytes) => Response::photo(bytes.to_vec(), content_type_of(&path)),
                Err(error) => failed(error),
            }
        }

        (Some(Method::Post), "/api/upload") => guarded(request, || {
            let Some(announced_length) = request.announced_length else {
                /* Without it a transfer that stops early is indistinguishable from one that
                 * finished, and a truncated photo would reach the album */
                return Response::error(411, "The upload must announce its size");
            };
            match state.files.upload(
                &raw(&url, "path"),
                &raw(&url, "name"),
                announced_length,
                body,
            ) {
                Ok(name) => Response::json(201, &json!({ "name": name })),
                Err(error) => failed(error),
            }
        }),

        (Some(Method::Post), "/api/delete") => guarded(request, || {
            let path = raw(&url, "path");
            match state.files.delete_photo(&path) {
                Ok(()) => {
                    state.thumbnails.forget(&path);
                    Response::no_content()
                }
                Err(error) => failed(error),
            }
        }),

        (Some(Method::Post), "/api/mkdir") => guarded(request, || {
            match state
                .files
                .create_directory(&raw(&url, "path"), &raw(&url, "name"))
            {
                Ok(name) => Response::json(201, &json!({ "name": name })),
                Err(error) => failed(error),
            }
        }),

        (Some(Method::Post), "/api/rmdir") => guarded(request, || {
            let path = raw(&url, "path");
            match state.files.delete_directory(&path) {
                Ok(()) => {
                    /* The directory was empty, but its photos' previews may still be cached from
                     * before they were deleted, and the name can come back */
                    state.thumbnails.forget_under(&path);
                    Response::no_content()
                }
                Err(error) => failed(error),
            }
        }),

        /* A known route reached with the wrong verb, which for the mutating ones is what an <img>
         * pointed at the delete route would look like */
        (_, "/" | "/api/list" | "/api/thumbnail" | "/api/photo") => {
            Response::error(405, "That address does not accept this kind of request")
        }
        (_, "/api/upload" | "/api/delete" | "/api/mkdir" | "/api/rmdir") => {
            Response::error(405, "That address only accepts POST")
        }
        _ => Response::error(404, "No such address"),
    }
}

/// Which of the two formats a photo is in.
///
/// `files` has already refused anything that is not one of them, so the choice is only between
/// these two and the extension is all it takes.
fn content_type_of(path: &str) -> &'static str {
    if path.to_ascii_lowercase().ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    }
}

/// Slows down guessing without making an honest login feel broken
const FAILED_LOGIN_DELAY: Duration = Duration::from_millis(500);

/// Runs a handler that changes something, once it is clear the request came from the page this
/// server serves rather than from another site the browser happened to have open
fn guarded(request: &Request, handler: impl FnOnce() -> Response) -> Response {
    if request.custom_header.is_none() {
        return Response::error(
            403,
            "Requests that change something must come from this page",
        );
    }
    if let Some(origin) = &request.origin
        && !is_own_origin(origin, request.host.as_deref())
    {
        return Response::error(403, "Requests from another site are not accepted");
    }
    handler()
}

/// Checks the credentials, in constant time so that neither the user name nor the password can be
/// guessed a character at a time
fn authorize(config: &WebConfig, authorization: Option<&str>) -> bool {
    let Some(encoded) = authorization.and_then(|value| value.strip_prefix("Basic ")) else {
        return false;
    };
    let Ok(decoded) = BASE64.decode(encoded.trim()) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, password)) = decoded.split_once(':') else {
        return false;
    };
    let matches = user.as_bytes().ct_eq(config.user.as_bytes())
        & password.as_bytes().ct_eq(config.password.as_bytes());
    matches.into()
}

/// Whether the name the browser used may reach this frame.
///
/// Only addresses and names that nobody on the internet can claim: an attacker can point
/// `evil.example` at this frame's address, but cannot make it be an IP literal, `localhost`, or an
/// mDNS name, which is how a phone on the same network finds a Raspberry Pi.
fn is_allowed_host(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        /* HTTP/1.1 requires it, so its absence is not a browser */
        return false;
    };
    /* An IPv6 literal is bracketed, so only a colon after the closing bracket separates the port */
    let (name, given_port) = match host.rfind(']') {
        Some(bracket) => match host[bracket + 1..].strip_prefix(':') {
            Some(given) => (&host[..=bracket], Some(given)),
            None => (host, None),
        },
        None => match host.rsplit_once(':') {
            Some((name, given)) => (name, Some(given)),
            None => (host, None),
        },
    };
    if let Some(given) = given_port
        && given.parse::<u16>() != Ok(port)
    {
        return false;
    }
    let name = name.trim_start_matches('[').trim_end_matches(']');
    /* A browser may spell a name in its fully qualified form, with the root label at the end */
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name.parse::<std::net::IpAddr>().is_ok() || name == "localhost" || name.ends_with(".local")
}

/// Whether an `Origin` names this very server
fn is_own_origin(origin: &str, host: Option<&str>) -> bool {
    match (Url::parse(origin).ok(), host) {
        (Some(origin), Some(host)) => {
            let authority = match origin.port() {
                Some(port) => format!("{}:{port}", origin.host_str().unwrap_or_default()),
                None => origin.host_str().unwrap_or_default().to_string(),
            };
            authority.eq_ignore_ascii_case(host)
        }
        _ => false,
    }
}

/// Parses the request target against a placeholder origin, since it carries only a path and a
/// query. Writing a query parser by hand is how `+` and percent-encoding get mishandled.
fn parse_target(target: &str) -> Option<Url> {
    if !target.starts_with('/') {
        return None;
    }
    Url::parse(&format!("http://frame.invalid{target}")).ok()
}

/// A query parameter still percent-encoded.
///
/// [crate::web::files] decodes exactly once, on the far side of its checks, so handing it a value
/// that has already been decoded would let `%252e%252e` through as `..`.
fn raw(url: &Url, name: &str) -> String {
    url.query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.replace('+', "%20"))
        .unwrap_or_default()
}

/// Turns a failure into the status that describes it, so the page can tell the user something
/// better than "it did not work"
fn failed(error: anyhow::Error) -> Response {
    if error.is::<PathRejected>() {
        Response::error(400, &error.to_string())
    } else if error.is::<Conflict>() {
        Response::error(409, &error.to_string())
    } else {
        /* The server's own wording is more useful than anything invented here, and the full chain
         * belongs in the log rather than in a browser */
        log::error!("{error:#}");
        Response::error(502, &format!("The FTP server refused: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::empty;

    use bytes::Bytes;
    use mockall::predicate::eq;

    use crate::{
        ftp::{Credentials, DirEntry, EntryKind, FtpAddress, FtpSession, MockFtpTransport},
        web::Refresh,
    };

    const USER: &str = "frame";
    const PASSWORD: &str = "hunter2hunter2";
    const HOST: &str = "192.168.1.50:8080";

    fn config() -> WebConfig {
        WebConfig {
            user: USER.to_string(),
            password: PASSWORD.to_string(),
            bind: "0.0.0.0:8080".parse().unwrap(),
        }
    }

    fn state(transport: MockFtpTransport) -> AppState<MockFtpTransport> {
        AppState {
            config: config(),
            files: FtpFiles::new(
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
                },
                transport,
                Refresh::default(),
            ),
            thumbnails: Thumbnails::new(1024 * 1024),
        }
    }

    fn credentials(user: &str, password: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{user}:{password}")))
    }

    /// A request the way the page sends it: logged in, from this frame, with the header that says
    /// it did not come from another site
    fn request(method: Method, target: &str) -> Request {
        Request {
            method: Some(method),
            target: target.to_string(),
            authorization: Some(credentials(USER, PASSWORD)),
            origin: Some(format!("http://{HOST}")),
            host: Some(HOST.to_string()),
            custom_header: Some("1".to_string()),
            announced_length: None,
        }
    }

    fn answer(state: &AppState<MockFtpTransport>, request: &Request) -> Response {
        handle(state, request, &mut empty())
    }

    fn body(response: &Response) -> serde_json::Value {
        serde_json::from_slice(&response.body).expect("responses carry JSON")
    }

    /* Authorization */

    #[test]
    fn authorize_accepts_the_configured_credentials() {
        assert!(authorize(&config(), Some(&credentials(USER, PASSWORD))));
    }

    #[test]
    fn authorize_rejects_wrong_credentials() {
        for (user, password) in [(USER, "wrong"), ("wrong", PASSWORD), ("", "")] {
            assert!(
                !authorize(&config(), Some(&credentials(user, password))),
                "credentials were {user}:{password}"
            );
        }
    }

    #[test]
    fn authorize_rejects_a_malformed_or_missing_header() {
        for header in [
            None,
            Some("Basic"),
            Some("Bearer abc"),
            Some("Basic !!!not base64!!!"),
            /* Valid base64, but no colon separating user and password */
            Some("Basic ZnJhbWU="),
        ] {
            assert!(!authorize(&config(), header), "header was {header:?}");
        }
    }

    #[test]
    fn handle_rejects_every_route_without_credentials() {
        for (method, target) in [
            (Method::Get, "/"),
            (Method::Get, "/api/list"),
            (Method::Get, "/api/thumbnail?path=a.jpg"),
            (Method::Post, "/api/upload?path=&name=a.jpg"),
            (Method::Post, "/api/delete?path=a.jpg"),
            (Method::Post, "/api/mkdir?path=&name=a"),
            (Method::Post, "/api/rmdir?path=a"),
        ] {
            let mut without = request(method, target);
            without.authorization = None;

            let response = answer(&state(MockFtpTransport::new()), &without);

            assert_eq!(response.status, 401, "route was {target}");
            assert!(
                response
                    .headers
                    .iter()
                    .any(|(name, _)| *name == "WWW-Authenticate")
            );
        }
    }

    /* Cross-site protection */

    #[test]
    fn handle_rejects_a_mutating_request_without_the_custom_header() {
        /* This is the whole defence against another site having the browser delete photos: Basic
         * credentials ride along on a cross-site form submission, and a form cannot set a header */
        for target in [
            "/api/upload?path=&name=a.jpg",
            "/api/delete?path=a.jpg",
            "/api/mkdir?path=&name=a",
            "/api/rmdir?path=a",
        ] {
            let mut forged = request(Method::Post, target);
            forged.custom_header = None;
            let mut transport = MockFtpTransport::new();
            transport.expect_delete_file().never();
            transport.expect_store().never();
            transport.expect_create_directory().never();
            transport.expect_delete_directory().never();

            let response = answer(&state(transport), &forged);

            assert_eq!(response.status, 403, "route was {target}");
        }
    }

    #[test]
    fn handle_rejects_a_mutating_request_from_a_foreign_origin() {
        let mut forged = request(Method::Post, "/api/delete?path=a.jpg");
        forged.origin = Some("http://evil.example".to_string());
        let mut transport = MockFtpTransport::new();
        transport.expect_delete_file().never();

        assert_eq!(answer(&state(transport), &forged).status, 403);
    }

    #[test]
    fn handle_accepts_a_mutating_request_that_states_this_frame_as_its_origin() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport.expect_delete_file().return_once(|_| Ok(()));

        let response = answer(
            &state(transport),
            &request(Method::Post, "/api/delete?path=a.jpg"),
        );

        assert_eq!(response.status, 204);
    }

    #[test]
    fn handle_rejects_a_request_with_an_unexpected_host() {
        /* A name somebody else controls, pointed at this frame, would otherwise become this origin
         * and be able to read the album */
        let mut foreign = request(Method::Get, "/api/list");
        foreign.host = Some("evil.example:8080".to_string());

        assert_eq!(
            answer(&state(MockFtpTransport::new()), &foreign).status,
            421
        );
    }

    #[test]
    fn is_allowed_host_accepts_the_ways_a_phone_reaches_the_frame() {
        for host in [
            "192.168.1.50:8080",
            "localhost:8080",
            "photoframe.local:8080",
            "PhotoFrame.Local:8080",
            /* The fully qualified spelling of the same name */
            "photoframe.local.:8080",
            "[fe80::1]:8080",
            "[fe80::1]",
        ] {
            assert!(is_allowed_host(Some(host), 8080), "host was {host}");
        }
    }

    #[test]
    fn is_allowed_host_rejects_a_registered_name_or_another_port() {
        for host in [
            "evil.example:8080",
            "frame.example.com:8080",
            /* A name that merely starts with an address, which is how a careless check is fooled */
            "192.168.1.50.evil.example:8080",
            "192.168.1.50:9999",
            "[fe80::1]:9999",
        ] {
            assert!(!is_allowed_host(Some(host), 8080), "host was {host}");
        }
        assert!(!is_allowed_host(None, 8080));
    }

    /* Routing */

    #[test]
    fn handle_serves_the_page_at_the_root() {
        let response = answer(&state(MockFtpTransport::new()), &request(Method::Get, "/"));

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(
            response
                .headers
                .iter()
                .any(|(name, _)| *name == "Content-Security-Policy")
        );
    }

    #[test]
    fn handle_reports_an_unknown_route_as_not_found() {
        let response = answer(
            &state(MockFtpTransport::new()),
            &request(Method::Get, "/wp-admin"),
        );

        assert_eq!(response.status, 404);
    }

    #[test]
    fn handle_reports_a_get_on_a_mutating_route_as_method_not_allowed() {
        /* What an <img src="/api/delete?path=..."> would be */
        let mut transport = MockFtpTransport::new();
        transport.expect_delete_file().never();

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/delete?path=a.jpg"),
        );

        assert_eq!(response.status, 405);
    }

    #[test]
    fn handle_lists_a_directory_as_json() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport
            .expect_list_dir()
            .with(eq("/Photos/summer"))
            .return_once(|_| {
                Ok(vec![
                    DirEntry {
                        name: "beach".to_string(),
                        kind: EntryKind::Directory,
                        modified: None,
                    },
                    DirEntry {
                        name: "IMG_1234.jpg".to_string(),
                        kind: EntryKind::File,
                        modified: None,
                    },
                ])
            });

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/list?path=summer"),
        );

        assert_eq!(response.status, 200);
        assert_eq!(
            body(&response),
            json!({
                "path": "summer",
                "directories": [{ "name": "beach" }],
                "photos": [{ "name": "IMG_1234.jpg", "modified": null }],
            })
        );
    }

    #[test]
    fn handle_reports_a_rejected_path_as_a_bad_request() {
        let mut transport = MockFtpTransport::new();
        transport.expect_list_dir().never();

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/list?path=..%2F..%2Fetc"),
        );

        assert_eq!(response.status, 400);
    }

    #[test]
    fn handle_decodes_a_parameter_exactly_once() {
        /* Decoding here as well as in files would turn "%252e%252e" into "..", so what has to
         * arrive at the server is a directory literally named "%2e%2e" */
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport
            .expect_list_dir()
            .with(eq("/Photos/%2e%2e"))
            .times(1)
            .return_once(|_| Ok(vec![]));

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/list?path=%252e%252e"),
        );

        assert_eq!(response.status, 200);
    }

    #[test]
    fn handle_reports_an_ftp_failure_as_a_bad_gateway() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport
            .expect_list_dir()
            .return_once(|_| Err(anyhow::anyhow!("550 Permission denied")));

        let response = answer(&state(transport), &request(Method::Get, "/api/list"));

        assert_eq!(response.status, 502);
    }

    #[test]
    fn handle_reports_an_upload_without_a_content_length_as_length_required() {
        let mut transport = MockFtpTransport::new();
        transport.expect_store().never();

        let response = answer(
            &state(transport),
            &request(Method::Post, "/api/upload?path=&name=a.jpg"),
        );

        assert_eq!(response.status, 411);
    }

    #[test]
    fn handle_returns_the_uploaded_name_when_the_file_was_renamed() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport.expect_list_dir().return_once(|_| {
            Ok(vec![DirEntry {
                name: "a.jpg".to_string(),
                kind: EntryKind::File,
                modified: None,
            }])
        });
        transport.expect_store().return_once(|_, source| {
            let mut buffer = vec![];
            source.read_to_end(&mut buffer).unwrap();
            Ok(buffer.len() as u64)
        });
        transport.expect_rename().return_once(|_, _| Ok(()));
        let mut upload = request(Method::Post, "/api/upload?path=&name=a.jpg");
        upload.announced_length = Some(5);

        let response = handle(
            &state(transport),
            &upload,
            &mut std::io::Cursor::new(b"photo"),
        );

        assert_eq!(response.status, 201);
        assert_eq!(body(&response), json!({ "name": "a (2).jpg" }));
    }

    #[test]
    fn handle_reports_a_taken_directory_name_as_a_conflict() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport.expect_create_directory().return_once(|_| {
            Err(anyhow::anyhow!(crate::ftp::InvalidFtpResponse {
                status: suppaftp::Status::FileUnavailable,
                message: "File exists".to_string(),
            }))
        });

        let response = answer(
            &state(transport),
            &request(Method::Post, "/api/mkdir?path=&name=summer"),
        );

        assert_eq!(response.status, 409);
    }

    #[test]
    fn handle_serves_the_photo_itself_untouched() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport
            .expect_retrieve()
            .return_once(|_| Ok(Bytes::from_static(crate::web::thumbnails::TEST_JPEG)));

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/photo?path=a.jpg"),
        );

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "image/jpeg");
        assert_eq!(
            response.body,
            crate::web::thumbnails::TEST_JPEG,
            "the photo has to arrive as it is stored, not re-encoded"
        );
    }

    #[test]
    fn handle_serves_a_png_as_a_png() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport
            .expect_retrieve()
            .return_once(|_| Ok(Bytes::from_static(b"not really a png")));

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/photo?path=Holiday%2Fa.PNG"),
        );

        assert_eq!(
            response.content_type, "image/png",
            "the extension decides, and it is matched without regard to case"
        );
    }

    #[test]
    fn handle_serves_a_thumbnail_as_jpeg_that_the_browser_may_keep() {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        /* The start of the file is asked for first, every time. This test photo carries no
         * embedded preview, so the whole of it is fetched after that; a photo that has one never
         * reaches the second call, which is the point of the arrangement. */
        transport
            .expect_retrieve_head()
            .withf(|_, max| *max == PREVIEW_HEAD_BYTES)
            .return_once(|_, _| Ok(Bytes::from_static(crate::web::thumbnails::TEST_JPEG)));
        transport
            .expect_retrieve()
            .return_once(|_| Ok(Bytes::from_static(crate::web::thumbnails::TEST_JPEG)));

        let response = answer(
            &state(transport),
            &request(Method::Get, "/api/thumbnail?path=a.jpg"),
        );

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "image/jpeg");
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| *name == "Cache-Control" && value.contains("max-age"))
        );
    }
}
