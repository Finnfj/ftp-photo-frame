//! Browsing and changing the photo directory on behalf of the web app.
//!
//! This module holds the web app's own FTP session, and it is where every path the browser sends
//! is checked before it reaches a command. Nothing above this line may build an FTP path itself.

use std::{
    collections::HashSet,
    fmt::{self, Formatter},
    io::Read,
    sync::{Mutex, MutexGuard, PoisonError},
};

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use percent_encoding::percent_decode_str;
use rand::{RngExt, rng};

use crate::{
    api_client::ftp_client::{EXCLUDED_DIRECTORIES, PHOTO_EXTENSIONS, is_photo},
    ftp::{EntryKind, FtpSession, FtpTransport, validate_path, with_reconnect},
    slideshow::Refresh,
};

/// Bounds a path so that a request cannot make the program build an arbitrarily long FTP command
const MAX_PATH_LENGTH: usize = 1024;

/// Bounds how many parts a path coming from the browser may be made of.
///
/// Deliberately a constant of its own rather than the traversal's `MAX_DEPTH`, which the two
/// shared until it became unclear which of the two meanings a given use carried. They are
/// different measurements that happen to agree on a number: this counts the parts of one path,
/// the file name among them, while `MAX_DEPTH` counts the levels the album traversal has
/// descended below the root. Changing one should not silently change the other.
const MAX_PATH_SEGMENTS: usize = 16;

/// Most servers stop at 255 bytes, and a name that long is a mistake rather than a wish
const MAX_NAME_LENGTH: usize = 255;

/// How many `name (2).jpg`, `name (3).jpg` … are tried before an upload is refused
const MAX_NAME_ATTEMPTS: u32 = 99;

/// Prefix of the name an upload is stored under until it is complete. The leading dot is what
/// matters: [crate::api_client::ftp_client::FtpApiClient] skips hidden entries, so the slideshow
/// never sees a half-transferred photo.
const TEMPORARY_PREFIX: &str = ".upload-";

/// A path or name supplied by the browser that this program refuses to use
#[derive(Debug)]
pub struct PathRejected(pub String);

impl fmt::Display for PathRejected {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for PathRejected {}

/// The server cannot carry out the request as asked, because a name is taken or a directory still
/// holds something
#[derive(Debug)]
pub struct Conflict(pub String);

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Conflict {}

/// What a single directory holds, as far as the web app is concerned
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Listing {
    pub directories: Vec<String>,
    pub photos: Vec<Photo>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Photo {
    pub name: String,
    /// [None] when the server did not report a usable modification time
    pub modified: Option<DateTime<Utc>>,
}

/// The web app's view of the photo directory, with its own FTP session.
///
/// The session is separate from the slideshow's because an FTP session is stateful and the
/// slideshow's is moved into the photo fetching thread. It connects lazily, so a frame nobody
/// manages still holds exactly one session.
pub struct FtpFiles<T> {
    session: FtpSession,
    /// A [Mutex] rather than the [std::cell::RefCell] [crate::api_client::ftp_client::FtpApiClient]
    /// uses, so that giving the accept loop more than one worker stays a one-line change.
    /// Uncontended it costs nothing.
    transport: Mutex<T>,
    refresh: Refresh,
}

impl<T: FtpTransport> FtpFiles<T> {
    pub const fn new(session: FtpSession, transport: T, refresh: Refresh) -> Self {
        Self {
            session,
            transport: Mutex::new(transport),
            refresh,
        }
    }

    /// Lets a test hold on to the handle the slideshow would be watching
    #[cfg(test)]
    fn with_refresh(mut self, refresh: Refresh) -> Self {
        self.refresh = refresh;
        self
    }

    /// Directories and photos of one directory, sorted by name.
    ///
    /// Symbolic links are left out for the same reason
    /// [crate::api_client::ftp_client::FtpApiClient] does not follow them, and here it is what
    /// keeps [resolve] meaningful: the server, not this program, resolves a link, so a link that
    /// never appears in the listing can never become a path the browser asks for.
    pub fn list(&self, directory: &str) -> Result<Listing> {
        let path = resolve(&self.session.root, directory)?;
        let entries = self.retrying(|transport| transport.list_dir(&path))?;

        let mut listing = Listing::default();
        for entry in entries {
            if entry.name.starts_with('.') || EXCLUDED_DIRECTORIES.contains(&entry.name.as_str()) {
                continue;
            }
            match entry.kind {
                EntryKind::Directory => listing.directories.push(entry.name),
                EntryKind::Symlink => log::debug!("Skipping symbolic link {}", entry.name),
                EntryKind::File if is_photo(&entry.name) => listing.photos.push(Photo {
                    name: entry.name,
                    modified: entry.modified,
                }),
                EntryKind::File => (),
            }
        }
        listing.directories.sort();
        listing.photos.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(listing)
    }

    /// Downloads a photo, for the thumbnail to be rendered from
    pub fn read(&self, photo: &str) -> Result<Bytes> {
        let path = resolve_photo(&self.session.root, photo)?;
        self.retrying(|transport| transport.retrieve(&path))
    }

    /// Downloads only the beginning of a photo, which is where the embedded preview lives
    pub fn read_head(&self, photo: &str, max_bytes: usize) -> Result<Bytes> {
        let path = resolve_photo(&self.session.root, photo)?;
        self.retrying(|transport| transport.retrieve_head(&path, max_bytes))
    }

    /// Stores a photo, and answers with the name it ended up under, which differs from the one
    /// asked for when that was already taken.
    ///
    /// The photo goes to a hidden temporary name first and is renamed only once the server
    /// confirms it accepted every announced byte. Without that, a browser losing the network
    /// halfway through would leave a truncated JPEG behind under its final name, and the slideshow
    /// would show an error screen for it on every round, forever.
    pub fn upload(
        &self,
        directory: &str,
        name: &str,
        announced_length: u64,
        source: &mut dyn Read,
    ) -> Result<String> {
        let path = resolve(&self.session.root, directory)?;
        let name = sanitize_file_name(name)?;

        /* Listing the directory also establishes the session, so that the upload below, which
         * cannot be retried, does not have to open one. */
        let taken = self
            .retrying(|transport| transport.list_dir(&path))?
            .into_iter()
            /* Many servers treat names case-insensitively, so a name differing only in case is
             * treated as taken rather than silently overwriting */
            .map(|entry| entry.name.to_lowercase())
            .collect();
        let name = free_name(&name, &taken).ok_or_else(|| {
            anyhow!(Conflict(format!(
                "{name} and the first {MAX_NAME_ATTEMPTS} alternative names are all taken"
            )))
        })?;
        /* Nothing else can add a file between the listing and the upload: there is a single worker
         * thread, and the only other writer would be somebody using a separate FTP client at that
         * very moment */

        let temporary = join(&path, &temporary_name());
        let stored = {
            /* Deliberately not through with_reconnect: `source` has been consumed by the time a
             * first attempt fails, so a retry would store an empty file and report success */
            let mut transport = self.lock();
            transport.store(&temporary, source)
        };
        let stored = match stored {
            Ok(stored) if stored == announced_length => stored,
            Ok(stored) => {
                self.discard(&temporary);
                bail!("The upload ended after {stored} of {announced_length} bytes");
            }
            Err(error) => {
                self.discard(&temporary);
                return Err(error);
            }
        };
        log::info!("Uploaded {stored} bytes to {path}");

        let target = join(&path, &name);
        if let Err(error) = self.retrying(|transport| transport.rename(&temporary, &target)) {
            self.discard(&temporary);
            return Err(error);
        }

        self.refresh.request();
        Ok(name)
    }

    pub fn delete_photo(&self, photo: &str) -> Result<()> {
        let path = resolve_photo(&self.session.root, photo)?;
        self.retrying(|transport| transport.delete_file(&path))?;
        /* The rest of the round still lists the deleted photo, and the slideshow burns a second on
         * PhotoNotFound every time it reaches it. Rebuilding costs one listing and ends that. */
        self.refresh.request();
        Ok(())
    }

    /// Creates a directory and answers with its name, which is the one asked for or an error
    pub fn create_directory(&self, parent: &str, name: &str) -> Result<String> {
        let path = resolve(&self.session.root, parent)?;
        let name = sanitize_directory_name(name)?;
        self.retrying(|transport| transport.create_directory(&join(&path, &name)))
            .map_err(name_taken)?;
        /* A new directory cannot hold photos yet, but it may be the target of the upload that
         * follows, and refreshing now is cheaper than reasoning about whether it was */
        self.refresh.request();
        Ok(name)
    }

    pub fn delete_directory(&self, directory: &str) -> Result<()> {
        if directory.is_empty() {
            bail!(PathRejected(
                "The directory the share link points at cannot be deleted".to_string()
            ))
        }
        let path = resolve(&self.session.root, directory)?;
        self.retrying(|transport| transport.delete_directory(&path))
            .map_err(directory_not_empty)?;
        /* Only empty directories can go, so nothing the slideshow is showing disappeared with it.
         * The listing the web app shows did change, but that is the browser's copy, not ours. */
        Ok(())
    }

    /// Runs an operation that can be repeated without consequence, reconnecting once if the
    /// session turned out to be dead
    fn retrying<R>(&self, operation: impl FnMut(&mut T) -> Result<R>) -> Result<R> {
        let mut transport = self.lock();
        with_reconnect(
            &mut *transport,
            &self.session.address,
            &self.session.credentials,
            operation,
        )
    }

    /// A panicking request must not make the web app useless until the frame is restarted, so a
    /// poisoned lock is taken over rather than propagated
    fn lock(&self) -> MutexGuard<'_, T> {
        self.transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Removes a temporary file after a failed upload. Failing to do so leaves a hidden file
    /// behind, which the slideshow ignores, so it is only worth a log line.
    fn discard(&self, temporary: &str) {
        if let Err(error) = self.retrying(|transport| transport.delete_file(temporary)) {
            log::warn!("Could not remove the incomplete upload {temporary}: {error}");
        }
    }
}

/// Turns a directory path supplied by the browser into an absolute path on the server, confined to
/// the directory the share link points at.
///
/// The web app is reachable by anyone on the local network, so this is the only thing between a
/// crafted request and the rest of the file system. It therefore rejects anything that is not
/// plainly a sequence of ordinary names, rather than trying to normalise a path into something
/// safe: what a path means is the FTP server's decision, not this program's to guess.
fn resolve(root: &str, relative: &str) -> Result<String> {
    /* Decoded exactly once, and before anything is checked. Checking first and decoding afterwards
     * would accept "%2e%2e" and then turn it into "..". */
    let relative = percent_decode_str(relative)
        .decode_utf8()
        .map_err(|_| anyhow!(PathRejected("Paths must be valid UTF-8".to_string())))?;

    if relative.is_empty() {
        return Ok(root.to_string());
    }
    if relative.len() > MAX_PATH_LENGTH {
        bail!(PathRejected(format!(
            "Paths must not be longer than {MAX_PATH_LENGTH} characters"
        )))
    }
    if relative.starts_with('/') {
        bail!(PathRejected(
            "Paths must be relative to the shared directory".to_string()
        ))
    }
    if relative.contains('\\') {
        /* Some servers accept a backslash as a separator, so it would be a second way to spell
         * "..", invisible to the segment checks below */
        bail!(PathRejected(
            "Paths must not contain backslashes".to_string()
        ))
    }
    if relative.contains(char::is_control) {
        /* Covers the CR and LF that would otherwise let a path append a second FTP command */
        bail!(PathRejected(
            "Paths must not contain control characters".to_string()
        ))
    }

    let segments: Vec<&str> = relative.split('/').collect();
    if segments.len() > MAX_PATH_SEGMENTS {
        bail!(PathRejected(format!(
            "Paths must not be made of more than {MAX_PATH_SEGMENTS} parts"
        )))
    }
    for segment in &segments {
        check_segment(segment)?;
    }

    let absolute = segments
        .iter()
        .fold(root.to_string(), |path, segment| join(&path, segment));
    /* Belt and braces: the checks above already rule out a line break, and this is what every
     * other FTP path in this program goes through */
    validate_path(&absolute)?;
    Ok(absolute)
}

/// Like [resolve], for the path of a photo rather than of a directory. Splitting the last segment
/// off and checking it as a file name is what stops a request from asking for a directory, or for
/// a file whose extension says it is not a photo.
fn resolve_photo(root: &str, relative: &str) -> Result<String> {
    let relative = percent_decode_str(relative)
        .decode_utf8()
        .map_err(|_| anyhow!(PathRejected("Paths must be valid UTF-8".to_string())))?;
    let (directory, name) = match relative.rsplit_once('/') {
        Some((directory, name)) => (directory, name),
        None => ("", relative.as_ref()),
    };
    /* Both halves are already decoded, so they must not be decoded a second time. Percent-encoding
     * them again keeps resolve and sanitize_file_name at their single decode. */
    let directory = resolve(root, &encode(directory))?;
    let name = sanitize_file_name(&encode(name))?;
    Ok(join(&directory, &name))
}

/// Checks one component of a path
fn check_segment(segment: &str) -> Result<()> {
    if segment.is_empty() {
        bail!(PathRejected(
            "Paths must not contain empty directory names".to_string()
        ))
    }
    if segment.starts_with('.') {
        /* Rejects "." and ".." along with every hidden entry. The slideshow skips hidden entries,
         * and the web app has no business reaching what the slideshow cannot see. */
        bail!(PathRejected(
            "Directory names must not start with a dot".to_string()
        ))
    }
    if segment.starts_with('~') {
        /* Some servers expand this to a home directory, which would leave the shared directory */
        bail!(PathRejected(
            "Directory names must not start with a tilde".to_string()
        ))
    }
    if EXCLUDED_DIRECTORIES.contains(&segment) {
        bail!(PathRejected(format!(
            "{segment} holds files generated by the server, not photos"
        )))
    }
    Ok(())
}

/// Checks the name of a photo to be uploaded.
///
/// Never repairs a name, always rejects it: a photo silently ending up under a different name than
/// the one the browser showed is worse than an error message.
fn sanitize_file_name(name: &str) -> Result<String> {
    let name = sanitize_name(name)?;
    if !is_photo(&name) {
        bail!(PathRejected(format!(
            "Only {} files can be displayed. Photos taken as HEIC have to be converted first, \
             which a phone usually offers to do while sharing them.",
            PHOTO_EXTENSIONS.join(", ")
        )))
    }
    Ok(name)
}

/// Checks the name of a directory to be created
fn sanitize_directory_name(name: &str) -> Result<String> {
    let name = sanitize_name(name)?;
    if EXCLUDED_DIRECTORIES.contains(&name.as_str()) {
        bail!(PathRejected(format!(
            "{name} is the name of a directory the server generates"
        )))
    }
    Ok(name)
}

fn sanitize_name(name: &str) -> Result<String> {
    let name = percent_decode_str(name)
        .decode_utf8()
        .map_err(|_| anyhow!(PathRejected("Names must be valid UTF-8".to_string())))?;

    if name.is_empty() {
        bail!(PathRejected("Names must not be empty".to_string()))
    }
    if name.len() > MAX_NAME_LENGTH {
        bail!(PathRejected(format!(
            "Names must not be longer than {MAX_NAME_LENGTH} bytes"
        )))
    }
    if name.contains(['/', '\\']) {
        bail!(PathRejected(
            "Names must not contain a directory separator".to_string()
        ))
    }
    if name.contains(char::is_control) {
        bail!(PathRejected(
            "Names must not contain control characters".to_string()
        ))
    }
    if name.starts_with('.') {
        bail!(PathRejected("Names must not start with a dot".to_string()))
    }
    if name.ends_with('.') || name.ends_with(' ') {
        /* Windows servers drop these silently, which would change the extension and leave a file
         * the slideshow does not recognise */
        bail!(PathRejected(
            "Names must not end with a dot or a space".to_string()
        ))
    }
    Ok(name.to_string())
}

/// The first of `name`, `name (2)`, `name (3)` … that is still free, or [None] when the browser
/// has to be told to pick a different one
fn free_name(name: &str, taken: &HashSet<String>) -> Option<String> {
    let is_free = |candidate: &String| !taken.contains(&candidate.to_lowercase());
    if is_free(&name.to_string()) {
        return Some(name.to_string());
    }
    let (stem, extension) = name
        .rsplit_once('.')
        .expect("a photo name has an extension by now");
    (2..=MAX_NAME_ATTEMPTS)
        .map(|attempt| format!("{stem} ({attempt}).{extension}"))
        .find(is_free)
}

/// Name an upload is stored under while it is in flight. Random rather than counted, so that an
/// interrupted upload whose temporary file was left behind cannot collide with the next one.
fn temporary_name() -> String {
    format!("{TEMPORARY_PREFIX}{:016x}", rng().random::<u64>())
}

/// Appends a single name to a path, allowing for a root that is just `/`
fn join(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Percent-encodes a path component, so that it survives the single decode
/// [resolve]/[sanitize_name] perform unchanged
fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// A server refusing to create a directory almost always means the name is taken, and that is
/// worth telling the browser apart from a real failure
fn name_taken(error: anyhow::Error) -> anyhow::Error {
    match refusal(&error) {
        Some(message) => anyhow!(Conflict(format!("The name is already taken ({message})"))),
        None => error,
    }
}

/// Every server refuses to remove a directory that still holds something
fn directory_not_empty(error: anyhow::Error) -> anyhow::Error {
    match refusal(&error) {
        Some(message) => anyhow!(Conflict(format!(
            "The folder is not empty, or cannot be removed ({message})"
        ))),
        None => error,
    }
}

/// The server's own wording of a refused command, as opposed to a failure of the connection
fn refusal(error: &anyhow::Error) -> Option<String> {
    use suppaftp::Status::{ActionAborted, FileUnavailable, RequestFileActionIgnored};
    match error.downcast_ref::<crate::ftp::InvalidFtpResponse>() {
        Some(response)
            if matches!(
                response.status,
                FileUnavailable | RequestFileActionIgnored | ActionAborted
            ) =>
        {
            Some(response.message.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use chrono::TimeZone;
    use mockall::predicate::eq;

    use crate::ftp::{Credentials, DirEntry, FtpAddress, MockFtpTransport};

    const ROOT: &str = "/Photos";

    /* resolve */

    #[test]
    fn resolve_returns_the_root_for_an_empty_path() {
        assert_eq!(resolve(ROOT, "").unwrap(), "/Photos");
    }

    #[test]
    fn resolve_joins_onto_a_root_that_is_just_a_slash() {
        assert_eq!(resolve("/", "summer").unwrap(), "/summer");
        assert_eq!(resolve("/", "").unwrap(), "/");
    }

    #[test]
    fn resolve_joins_nested_directories() {
        assert_eq!(
            resolve(ROOT, "summer/beach").unwrap(),
            "/Photos/summer/beach"
        );
    }

    #[test]
    fn resolve_accepts_names_with_spaces_and_non_ascii_characters() {
        assert_eq!(
            resolve(ROOT, "Sommerurlaub%20%C3%96land").unwrap(),
            "/Photos/Sommerurlaub Öland"
        );
    }

    #[test]
    fn resolve_percent_decodes_before_validating() {
        /* Checking first and decoding afterwards would let these through */
        for encoded in ["%2E%2E", "a/%2e%2e/b", "%2e%2e%2fetc", "%2fetc", "a%5Cb"] {
            assert!(resolve(ROOT, encoded).is_err(), "path was {encoded}");
        }
    }

    #[test]
    fn resolve_rejects_parent_directory_segments() {
        for path in ["..", "../etc", "a/../..", "a/../b", "."] {
            assert!(resolve(ROOT, path).is_err(), "path was {path}");
        }
    }

    #[test]
    fn resolve_rejects_an_absolute_path() {
        for path in ["/etc/passwd", "//host/share", "/"] {
            assert!(resolve(ROOT, path).is_err(), "path was {path}");
        }
    }

    #[test]
    fn resolve_rejects_backslashes() {
        for path in ["..\\..\\etc", "a\\b"] {
            assert!(resolve(ROOT, path).is_err(), "path was {path}");
        }
    }

    #[test]
    fn resolve_rejects_empty_segments() {
        for path in ["a//b", "a/", "summer//"] {
            assert!(resolve(ROOT, path).is_err(), "path was {path}");
        }
    }

    #[test]
    fn resolve_rejects_hidden_and_generated_directories() {
        for path in [".ssh", "a/.git/b", "@eaDir", "a/@eaDir"] {
            assert!(resolve(ROOT, path).is_err(), "path was {path}");
        }
    }

    #[test]
    fn resolve_rejects_a_leading_tilde() {
        assert!(resolve(ROOT, "~root").is_err());
        assert!(resolve(ROOT, "a/~root").is_err());
    }

    #[test]
    fn resolve_rejects_control_characters() {
        for path in ["a\r\nDELE important.jpg", "a\nb", "a\0b"] {
            assert!(resolve(ROOT, path).is_err(), "path was {path:?}");
        }
    }

    #[test]
    fn resolve_rejects_a_path_made_of_too_many_parts() {
        let deep = vec!["a"; MAX_PATH_SEGMENTS + 1].join("/");

        assert!(resolve(ROOT, &deep).is_err());
        assert!(resolve(ROOT, &vec!["a"; MAX_PATH_SEGMENTS].join("/")).is_ok());
    }

    #[test]
    fn resolve_rejects_an_overlong_path() {
        assert!(resolve(ROOT, &"a".repeat(MAX_PATH_LENGTH + 1)).is_err());
    }

    /* resolve_photo */

    #[test]
    fn resolve_photo_joins_the_directory_and_the_file_name() {
        assert_eq!(
            resolve_photo(ROOT, "summer/IMG_1234.jpg").unwrap(),
            "/Photos/summer/IMG_1234.jpg"
        );
        assert_eq!(
            resolve_photo(ROOT, "IMG_1234.jpg").unwrap(),
            "/Photos/IMG_1234.jpg"
        );
    }

    #[test]
    fn resolve_photo_accepts_a_path_whose_separators_are_encoded() {
        /* The page encodes a whole path with encodeURIComponent, so its slashes arrive as %2F */
        assert_eq!(
            resolve_photo(ROOT, "summer%2FIMG_1234.jpg").unwrap(),
            "/Photos/summer/IMG_1234.jpg"
        );
    }

    #[test]
    fn resolve_photo_rejects_anything_that_is_not_a_photo() {
        for path in ["summer", "summer/notes.txt", "summer/../etc/passwd", ""] {
            assert!(resolve_photo(ROOT, path).is_err(), "path was {path}");
        }
    }

    /* sanitize_file_name */

    #[test]
    fn sanitize_file_name_accepts_an_ordinary_photo_name() {
        assert_eq!(
            sanitize_file_name("Sommer%20am%20See.jpg").unwrap(),
            "Sommer am See.jpg"
        );
    }

    #[test]
    fn sanitize_file_name_accepts_any_case_of_a_supported_extension() {
        for name in ["IMG.JPG", "shot.PnG", "scan.JPEG"] {
            assert!(sanitize_file_name(name).is_ok(), "name was {name}");
        }
    }

    #[test]
    fn sanitize_file_name_rejects_path_components() {
        for name in ["a/b.jpg", "..\\b.jpg", "../b.jpg", "%2Fetc%2Fpasswd.jpg"] {
            assert!(sanitize_file_name(name).is_err(), "name was {name}");
        }
    }

    #[test]
    fn sanitize_file_name_rejects_an_unsupported_extension() {
        for name in ["x.heic", "x.txt", "photo.jpg.txt", "jpg", "noextension"] {
            assert!(sanitize_file_name(name).is_err(), "name was {name}");
        }
    }

    #[test]
    fn sanitize_file_name_rejects_a_hidden_or_empty_name() {
        for name in ["", ".hidden.jpg", ".", ".."] {
            assert!(sanitize_file_name(name).is_err(), "name was {name}");
        }
    }

    #[test]
    fn sanitize_file_name_rejects_a_trailing_dot_or_space() {
        for name in ["photo.jpg.", "photo.jpg "] {
            assert!(sanitize_file_name(name).is_err(), "name was {name}");
        }
    }

    #[test]
    fn sanitize_file_name_rejects_control_characters() {
        assert!(sanitize_file_name("a%0D%0ADELE%20x.jpg").is_err());
    }

    #[test]
    fn sanitize_directory_name_rejects_a_generated_directory_name() {
        assert!(sanitize_directory_name("@eaDir").is_err());
        assert!(sanitize_directory_name("summer").is_ok());
    }

    /* free_name */

    #[test]
    fn free_name_keeps_a_name_that_is_available() {
        assert_eq!(
            free_name("a.jpg", &HashSet::new()),
            Some("a.jpg".to_string())
        );
    }

    #[test]
    fn free_name_appends_a_counter_to_a_name_that_is_taken() {
        let taken = HashSet::from(["a.jpg".to_string(), "a (2).jpg".to_string()]);

        assert_eq!(free_name("a.jpg", &taken), Some("a (3).jpg".to_string()));
    }

    #[test]
    fn free_name_compares_names_case_insensitively() {
        /* Many servers do, and overwriting a photo that differs only in case would be a surprise */
        let taken = HashSet::from(["a.jpg".to_string()]);

        assert_eq!(free_name("A.JPG", &taken), Some("A (2).JPG".to_string()));
    }

    #[test]
    fn free_name_gives_up_when_every_alternative_is_taken() {
        let mut taken = HashSet::from(["a.jpg".to_string()]);
        taken.extend((2..=MAX_NAME_ATTEMPTS).map(|n| format!("a ({n}).jpg")));

        assert_eq!(free_name("a.jpg", &taken), None);
    }

    /* FtpFiles */

    fn files(transport: MockFtpTransport) -> FtpFiles<MockFtpTransport> {
        FtpFiles::new(
            FtpSession {
                address: FtpAddress {
                    host: "fake.ftp.addr".to_string(),
                    port: 21,
                    tls: false,
                },
                credentials: Credentials {
                    user: "joe".to_string(),
                    password: "hunter2".to_string(),
                },
                root: ROOT.to_string(),
            },
            transport,
            Refresh::default(),
        )
    }

    fn connected() -> MockFtpTransport {
        let mut transport = MockFtpTransport::new();
        transport.expect_is_connected().return_const(true);
        transport
    }

    fn entry(name: &str, kind: EntryKind) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            kind,
            modified: None,
        }
    }

    #[test]
    fn list_reports_directories_and_photos_separately_and_sorted() {
        let mut transport = connected();
        transport
            .expect_list_dir()
            .with(eq("/Photos/summer"))
            .return_once(|_| {
                Ok(vec![
                    entry("IMG_2.jpg", EntryKind::File),
                    entry("beach", EntryKind::Directory),
                    DirEntry {
                        name: "IMG_1.jpg".to_string(),
                        kind: EntryKind::File,
                        modified: Some(Utc.with_ymd_and_hms(2025, 10, 18, 14, 33, 7).unwrap()),
                    },
                    entry("alps", EntryKind::Directory),
                ])
            });

        let listing = files(transport).list("summer").unwrap();

        assert_eq!(listing.directories, ["alps", "beach"]);
        assert_eq!(
            listing.photos,
            [
                Photo {
                    name: "IMG_1.jpg".to_string(),
                    modified: Some(Utc.with_ymd_and_hms(2025, 10, 18, 14, 33, 7).unwrap()),
                },
                Photo {
                    name: "IMG_2.jpg".to_string(),
                    modified: None,
                },
            ]
        );
    }

    #[test]
    fn list_skips_hidden_generated_symlink_and_non_photo_entries() {
        let mut transport = connected();
        transport.expect_list_dir().return_once(|_| {
            Ok(vec![
                entry(".", EntryKind::Directory),
                entry("..", EntryKind::Directory),
                entry(".hidden", EntryKind::Directory),
                entry("@eaDir", EntryKind::Directory),
                /* The security-relevant one: the server, not this program, resolves a link, so a
                 * link that never appears cannot become a path the browser asks for */
                entry("elsewhere", EntryKind::Symlink),
                entry("notes.txt", EntryKind::File),
                entry("keep.jpg", EntryKind::File),
                entry("keep", EntryKind::Directory),
            ])
        });

        let listing = files(transport).list("").unwrap();

        assert_eq!(listing.directories, ["keep"]);
        assert_eq!(
            listing.photos.iter().map(|p| &p.name).collect::<Vec<_>>(),
            ["keep.jpg"]
        );
    }

    #[test]
    fn list_refuses_a_path_that_escapes_the_root() {
        let mut transport = MockFtpTransport::new();
        transport.expect_list_dir().never();

        let result = files(transport).list("../../etc");

        assert!(result.is_err_and(|error| error.is::<PathRejected>()));
    }

    #[test]
    fn read_downloads_the_resolved_photo() {
        let mut transport = connected();
        transport
            .expect_retrieve()
            .with(eq("/Photos/summer/IMG_1234.jpg"))
            .return_once(|_| Ok(Bytes::from_static(b"photo")));

        let bytes = files(transport).read("summer/IMG_1234.jpg").unwrap();

        assert_eq!(bytes, Bytes::from_static(b"photo"));
    }

    #[test]
    fn upload_stores_under_a_hidden_temporary_name_and_renames_it_afterwards() {
        let mut transport = connected();
        transport
            .expect_list_dir()
            .with(eq("/Photos/summer"))
            .return_once(|_| Ok(vec![]));
        transport
            /* Stored hidden, so that the slideshow cannot pick up a half-transferred photo */
            .expect_store()
            .withf(|path, _| path.starts_with("/Photos/summer/.upload-"))
            .return_once(|_, source| {
                let mut buffer = vec![];
                source.read_to_end(&mut buffer).unwrap();
                Ok(buffer.len() as u64)
            });
        transport
            .expect_rename()
            .withf(|from, to| {
                from.starts_with("/Photos/summer/.upload-") && to == "/Photos/summer/IMG_1234.jpg"
            })
            .return_once(|_, _| Ok(()));

        let name = files(transport)
            .upload("summer", "IMG_1234.jpg", 5, &mut Cursor::new(b"photo"))
            .unwrap();

        assert_eq!(name, "IMG_1234.jpg");
    }

    #[test]
    fn upload_removes_the_temporary_file_when_fewer_bytes_arrived_than_announced() {
        let mut transport = connected();
        transport.expect_list_dir().return_once(|_| Ok(vec![]));
        transport.expect_store().return_once(|_, _| Ok(3));
        transport.expect_rename().never();
        transport
            .expect_delete_file()
            .withf(|path| path.starts_with("/Photos/.upload-"))
            .times(1)
            .return_once(|_| Ok(()));

        let result = files(transport).upload("", "IMG_1234.jpg", 5, &mut Cursor::new(b"pho"));

        assert!(result.is_err(), "a truncated upload must not be accepted");
    }

    #[test]
    fn upload_renames_a_file_whose_name_is_already_taken() {
        let mut transport = connected();
        transport
            .expect_list_dir()
            .return_once(|_| Ok(vec![entry("IMG_1234.jpg", EntryKind::File)]));
        transport.expect_store().return_once(|_, _| Ok(5));
        transport
            .expect_rename()
            .withf(|_, to| to == "/Photos/IMG_1234 (2).jpg")
            .return_once(|_, _| Ok(()));

        let name = files(transport)
            .upload("", "IMG_1234.jpg", 5, &mut Cursor::new(b"photo"))
            .unwrap();

        assert_eq!(name, "IMG_1234 (2).jpg");
    }

    #[test]
    fn upload_reports_a_conflict_when_no_free_name_is_left() {
        let mut transport = connected();
        transport.expect_list_dir().return_once(|_| {
            let mut entries = vec![entry("a.jpg", EntryKind::File)];
            entries.extend(
                (2..=MAX_NAME_ATTEMPTS).map(|n| entry(&format!("a ({n}).jpg"), EntryKind::File)),
            );
            Ok(entries)
        });
        transport.expect_store().never();

        let result = files(transport).upload("", "a.jpg", 1, &mut Cursor::new(b"x"));

        assert!(result.is_err_and(|error| error.is::<Conflict>()));
    }

    #[test]
    fn upload_rejects_a_name_that_is_not_a_photo() {
        let mut transport = MockFtpTransport::new();
        transport.expect_store().never();

        let result = files(transport).upload("", "notes.txt", 1, &mut Cursor::new(b"x"));

        assert!(result.is_err_and(|error| error.is::<PathRejected>()));
    }

    #[test]
    fn upload_requests_a_slideshow_refresh() {
        let mut transport = connected();
        transport.expect_list_dir().return_once(|_| Ok(vec![]));
        transport.expect_store().return_once(|_, _| Ok(5));
        transport.expect_rename().return_once(|_, _| Ok(()));
        let refresh = Refresh::default();
        let files = files(transport).with_refresh(refresh.clone());

        files
            .upload("", "IMG_1234.jpg", 5, &mut Cursor::new(b"photo"))
            .unwrap();

        assert!(
            refresh.take(),
            "an uploaded photo has to reach the slideshow before its round ends"
        );
    }

    #[test]
    fn a_failed_upload_does_not_request_a_refresh() {
        let mut transport = connected();
        transport.expect_list_dir().return_once(|_| Ok(vec![]));
        transport.expect_store().return_once(|_, _| Ok(3));
        transport.expect_delete_file().return_once(|_| Ok(()));
        let refresh = Refresh::default();
        let files = files(transport).with_refresh(refresh.clone());

        let _ = files.upload("", "IMG_1234.jpg", 5, &mut Cursor::new(b"pho"));

        assert!(!refresh.take());
    }

    #[test]
    fn delete_photo_refuses_a_path_that_escapes_the_root() {
        let mut transport = MockFtpTransport::new();
        transport.expect_delete_file().never();

        let result = files(transport).delete_photo("../../etc/passwd.jpg");

        assert!(result.is_err_and(|error| error.is::<PathRejected>()));
    }

    #[test]
    fn delete_photo_removes_the_resolved_photo() {
        let mut transport = connected();
        transport
            .expect_delete_file()
            .with(eq("/Photos/summer/IMG_1234.jpg"))
            .return_once(|_| Ok(()));

        assert!(files(transport).delete_photo("summer/IMG_1234.jpg").is_ok());
    }

    #[test]
    fn delete_photo_requests_a_slideshow_refresh() {
        let mut transport = connected();
        transport.expect_delete_file().return_once(|_| Ok(()));
        let refresh = Refresh::default();
        let files = files(transport).with_refresh(refresh.clone());

        files.delete_photo("summer/IMG_1234.jpg").unwrap();

        assert!(
            refresh.take(),
            "the rest of the round still lists the deleted photo, and stalls a second on each"
        );
    }

    #[test]
    fn a_failed_delete_does_not_request_a_refresh() {
        let mut transport = connected();
        transport
            .expect_delete_file()
            .return_once(|_| Err(anyhow!("no")));
        let refresh = Refresh::default();
        let files = files(transport).with_refresh(refresh.clone());

        let _ = files.delete_photo("summer/IMG_1234.jpg");

        assert!(!refresh.take());
    }

    #[test]
    fn create_directory_creates_it_below_the_resolved_parent() {
        let mut transport = connected();
        transport
            .expect_create_directory()
            .with(eq("/Photos/summer/beach"))
            .return_once(|_| Ok(()));

        let name = files(transport)
            .create_directory("summer", "beach")
            .unwrap();

        assert_eq!(name, "beach");
    }

    #[test]
    fn delete_directory_refuses_the_shared_directory_itself() {
        let mut transport = MockFtpTransport::new();
        transport.expect_delete_directory().never();

        let result = files(transport).delete_directory("");

        assert!(result.is_err_and(|error| error.is::<PathRejected>()));
    }

    #[test]
    fn delete_directory_does_not_request_a_refresh() {
        let mut transport = connected();
        transport.expect_delete_directory().return_once(|_| Ok(()));
        let refresh = Refresh::default();
        let files = files(transport).with_refresh(refresh.clone());

        files.delete_directory("summer").unwrap();

        assert!(
            !refresh.take(),
            "only empty directories can go, so the display sequence cannot have held anything in it"
        );
    }

    #[test]
    fn delete_directory_reports_a_non_empty_directory_as_a_conflict() {
        let mut transport = connected();
        transport.expect_delete_directory().return_once(|_| {
            Err(anyhow!(crate::ftp::InvalidFtpResponse {
                status: suppaftp::Status::FileUnavailable,
                message: "Directory not empty".to_string(),
            }))
        });

        let result = files(transport).delete_directory("summer");

        assert!(result.is_err_and(|error| error.is::<Conflict>()));
    }
}
