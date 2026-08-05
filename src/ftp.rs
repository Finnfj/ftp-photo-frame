//! FTP request-response handling

use std::{
    fmt::{self, Formatter},
    net::ToSocketAddrs,
    time::{Duration, SystemTime},
};

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use suppaftp::{
    FtpError, FtpStream, Mode, Status, list::ListParser, types::FileType as TransferType,
};

/// Isolates the FTP protocol for testing, mirroring the role of [crate::http::HttpClient].
///
/// The methods take `&mut self` because an FTP session is stateful. Everything protocol-specific
/// belongs in the implementation; deciding which files to use and in what order is the job of
/// [crate::api_client::ftp_client::FtpApiClient].
#[cfg_attr(test, mockall::automock)]
pub trait FtpTransport {
    /// Opens the control connection, authenticates and switches to binary transfers. Must be
    /// callable again after [FtpTransport::disconnect].
    fn connect(&mut self, address: &FtpAddress, credentials: &Credentials) -> Result<()>;

    /// False before the first successful [FtpTransport::connect] and after
    /// [FtpTransport::disconnect].
    fn is_connected(&self) -> bool;

    /// Entries of a single directory, without descending into it. `path` is absolute on the server.
    /// The `.` and `..` entries are reported like any other, and it is up to the caller to skip
    /// them.
    fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>>;

    /// Downloads a file in binary mode
    fn retrieve(&mut self, path: &str) -> Result<Bytes>;

    /// Closes the connection, ignoring any error while doing so
    fn disconnect(&mut self);
}

/// A single entry of a directory listing
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// Entry name without any path component
    pub name: String,
    pub kind: EntryKind,
    /// [None] when the server did not report a usable modification time
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// Address of an FTP server
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtpAddress {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    /// Deliberately hand-written to keep the password out of logs and test output
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"[redacted]")
            .finish()
    }
}

/// The control connection broke. The operation may succeed after reconnecting, so
/// [crate::api_client::ftp_client::FtpApiClient] retries it once.
#[derive(Debug)]
pub struct ConnectionLost(pub anyhow::Error);

impl fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ConnectionLost {}

/// The server refused a command. Plays the same role for FTP that
/// [crate::http::InvalidHttpResponse] plays for HTTP.
#[derive(Debug)]
pub struct InvalidFtpResponse {
    pub status: Status,
    pub message: String,
}

impl fmt::Display for InvalidFtpResponse {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Invalid FTP response code: {} {}",
            self.status.code(),
            self.message
        )
    }
}

impl std::error::Error for InvalidFtpResponse {}

/// Directory listing command used by a server
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Listing {
    /// Machine-readable listing (RFC 3659), which reports modification times
    Mlsd,
    /// Human-readable listing, available on every server
    List,
}

/// Isolates [suppaftp]
pub struct SuppaFtpTransport {
    timeout: Duration,
    stream: Option<FtpStream>,
    /// Negotiated on first use and remembered, so that a server without `MLSD` support is not
    /// probed again for every directory
    listing: Option<Listing>,
}

impl SuppaFtpTransport {
    pub const fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            stream: None,
            listing: None,
        }
    }

    fn stream_mut(&mut self) -> Result<&mut FtpStream> {
        self.stream.as_mut().ok_or_else(|| {
            /* Reported as a lost connection so that a caller which retries recovers by reconnecting
             * instead of failing outright */
            anyhow!(ConnectionLost(anyhow!("Not connected to the FTP server")))
        })
    }
}

impl FtpTransport for SuppaFtpTransport {
    fn connect(&mut self, address: &FtpAddress, credentials: &Credentials) -> Result<()> {
        self.disconnect();

        let FtpAddress { host, port } = address;
        let socket_address = (host.as_str(), *port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("Cannot resolve FTP server address {host}:{port}"))?;
        log::info!("Connecting to FTP server {host}:{port}");
        let mut stream =
            FtpStream::connect_timeout(socket_address, self.timeout).map_err(map_error)?;
        /* FTP itself defines no timeouts, so without these a server that stops responding would
         * block the photo fetching thread indefinitely */
        let socket = stream.get_ref();
        socket.set_read_timeout(Some(self.timeout))?;
        socket.set_write_timeout(Some(self.timeout))?;
        /* Stated explicitly although it is the default: a photo frame is behind NAT, and active
         * mode would require the server to connect back to it */
        stream.set_mode(Mode::Passive);
        stream
            .login(&credentials.user, &credentials.password)
            .map_err(map_error)?;
        /* Transfers default to ASCII, which would corrupt image data */
        stream
            .transfer_type(TransferType::Binary)
            .map_err(map_error)?;

        self.stream = Some(stream);
        self.listing = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let remembered_listing = self.listing;
        let stream = self.stream_mut()?;
        /* Changing directory and then listing without an argument avoids two problems with passing
         * the path to the listing command: servers that interpret it as a glob pattern, and servers
         * that split it on spaces. */
        stream.cwd(path).map_err(map_error)?;
        let (listing, lines) = match remembered_listing {
            Some(Listing::Mlsd) => (Listing::Mlsd, stream.mlsd(None).map_err(map_error)?),
            Some(Listing::List) => (Listing::List, stream.list(None).map_err(map_error)?),
            None => match stream.mlsd(None) {
                Ok(lines) => (Listing::Mlsd, lines),
                Err(error) if is_command_unsupported(&error) => {
                    log::info!(
                        "Server does not support MLSD ({error}), falling back to LIST. \
                         Photo modification times will not be available."
                    );
                    (Listing::List, stream.list(None).map_err(map_error)?)
                }
                Err(error) => return Err(map_error(error)),
            },
        };
        self.listing = Some(listing);

        Ok(lines
            .iter()
            .filter_map(|line| parse_entry(line, listing))
            .collect())
    }

    fn retrieve(&mut self, path: &str) -> Result<Bytes> {
        /* Unlike the listing commands, RETR takes the remainder of the command line as the file
         * name, so an absolute path can be passed directly. */
        let file = self.stream_mut()?.retr_as_buffer(path).map_err(map_error)?;
        Ok(Bytes::from(file.into_inner()))
    }

    fn disconnect(&mut self) {
        if let Some(mut stream) = self.stream.take()
            && let Err(error) = stream.quit()
        {
            log::debug!("Ignoring an error while closing the FTP connection: {error}");
        }
        self.listing = None;
    }
}

/// Converts a line of a directory listing into a [DirEntry], returning [None] for lines that cannot
/// be parsed
fn parse_entry(line: &str, listing: Listing) -> Option<DirEntry> {
    let file = match listing {
        Listing::Mlsd => ListParser::parse_mlsd(line).ok(),
        /* Most servers produce POSIX output, Windows ones the DOS format */
        Listing::List => ListParser::parse_posix(line)
            .or_else(|_| ListParser::parse_dos(line))
            .ok(),
    };
    let file = match file {
        Some(file) if !file.name().is_empty() => file,
        _ => {
            log::debug!("Ignoring unparsable directory listing line: {line}");
            return None;
        }
    };

    let kind = if file.is_symlink() {
        EntryKind::Symlink
    } else if file.is_directory() {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    Some(DirEntry {
        name: file.name().to_string(),
        kind,
        modified: match listing {
            Listing::Mlsd => modification_time(file.modified()),
            /* LIST reports the server's local time and omits the year for older files, so its
             * timestamps are dropped rather than reported as more accurate than they are */
            Listing::List => None,
        },
    })
}

/// [suppaftp] reports the epoch for an entry whose modification time is missing
fn modification_time(modified: SystemTime) -> Option<DateTime<Utc>> {
    if modified == SystemTime::UNIX_EPOCH {
        return None;
    }
    Some(DateTime::<Utc>::from(modified))
}

/// A server that does not implement a command answers with one of these
fn is_command_unsupported(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response)
    if matches!(
        response.status,
        Status::BadCommand
            | Status::BadArguments
            | Status::NotImplemented
            | Status::NotImplementedParameter
            | Status::BadSequence
    ))
}

/// Classifies an [FtpError] so that callers can tell a recoverable connection failure and a refused
/// command apart from anything else
fn map_error(error: FtpError) -> anyhow::Error {
    enum Kind {
        ConnectionLost,
        InvalidResponse(Status, String),
        Other,
    }
    let kind = match &error {
        FtpError::ConnectionError(_) | FtpError::BadResponse => Kind::ConnectionLost,
        FtpError::UnexpectedResponse(response) => match response.status {
            /* The server is shutting down or timed the session out */
            Status::NotAvailable => Kind::ConnectionLost,
            status => Kind::InvalidResponse(
                status,
                String::from_utf8_lossy(&response.body).trim().to_string(),
            ),
        },
        _ => Kind::Other,
    };
    match kind {
        Kind::ConnectionLost => anyhow!(ConnectionLost(anyhow!(error))),
        Kind::InvalidResponse(status, message) => anyhow!(InvalidFtpResponse { status, message }),
        Kind::Other => anyhow!(error),
    }
}

/// Fails when a path cannot be used in an FTP command
pub fn validate_path(path: &str) -> Result<()> {
    /* FTP commands are terminated by CRLF, so a path containing either would let a crafted share
     * link inject additional commands */
    if path.contains('\r') || path.contains('\n') {
        bail!("FTP paths must not contain line breaks");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    #[test]
    fn parse_entry_reads_mlsd_file_with_modification_time() {
        let line = "type=file;size=4096;modify=20251018143307; IMG_1234.jpg";

        let result = parse_entry(line, Listing::Mlsd);

        assert_eq!(
            result,
            Some(DirEntry {
                name: "IMG_1234.jpg".to_string(),
                kind: EntryKind::File,
                modified: Some(Utc.with_ymd_and_hms(2025, 10, 18, 14, 33, 7).unwrap()),
            })
        );
    }

    #[test]
    fn parse_entry_reads_mlsd_directory() {
        let line = "type=dir;modify=20201019151930;UNIX.mode=0755; summer";

        let result = parse_entry(line, Listing::Mlsd);

        assert_eq!(result.map(|entry| (entry.name, entry.kind)), {
            Some(("summer".to_string(), EntryKind::Directory))
        });
    }

    #[test]
    fn parse_entry_reads_mlsd_symlink() {
        let line = "type=link;modify=20201019151930; elsewhere";

        let result = parse_entry(line, Listing::Mlsd);

        assert_eq!(
            result.map(|entry| entry.kind),
            Some(EntryKind::Symlink),
            "a symlink must be distinguishable, so that it is not descended into"
        );
    }

    #[test]
    fn parse_entry_reads_mlsd_entries_of_the_listed_directory_itself() {
        /* These are reported as directories named "." and "..", which the caller skips like any
         * other hidden entry */
        for line in [
            "type=cdir;modify=20201019151930; .",
            "type=pdir;modify=20201019151930; ..",
        ] {
            let result = parse_entry(line, Listing::Mlsd);

            let entry = result.expect("should parse");
            assert_eq!(entry.kind, EntryKind::Directory);
            assert!(entry.name.starts_with('.'), "was {}", entry.name);
        }
    }

    #[test]
    fn parse_entry_reports_no_modification_time_when_mlsd_omits_it() {
        let line = "type=file;size=4096; IMG_1234.jpg";

        let result = parse_entry(line, Listing::Mlsd);

        assert_eq!(result.and_then(|entry| entry.modified), None);
    }

    #[test]
    fn parse_entry_reads_posix_list_line_without_modification_time() {
        let line = "-rw-r--r-- 1 user group 1234 Nov 5 13:46 IMG_1234.jpg";

        let result = parse_entry(line, Listing::List);

        assert_eq!(
            result,
            Some(DirEntry {
                name: "IMG_1234.jpg".to_string(),
                kind: EntryKind::File,
                /* LIST timestamps are ambiguous, so they are discarded */
                modified: None,
            })
        );
    }

    #[test]
    fn parse_entry_reads_posix_list_directory() {
        let line = "drwxr-xr-x 2 user group 4096 Nov 5 13:46 summer";

        let result = parse_entry(line, Listing::List);

        assert_eq!(
            result.map(|entry| (entry.name, entry.kind)),
            Some(("summer".to_string(), EntryKind::Directory))
        );
    }

    #[test]
    fn parse_entry_returns_none_for_an_unparsable_line() {
        for line in ["", "total 8", "!?"] {
            assert_eq!(parse_entry(line, Listing::List), None, "line was {line:?}");
        }
    }

    #[test]
    fn credentials_debug_does_not_reveal_the_password() {
        let credentials = Credentials {
            user: "joe".to_string(),
            password: "hunter2".to_string(),
        };

        let debug = format!("{credentials:?}");

        assert!(!debug.contains("hunter2"), "was {debug}");
        assert!(debug.contains("joe"), "was {debug}");
    }

    #[test]
    fn validate_path_rejects_line_breaks() {
        assert!(validate_path("/Photos/summer").is_ok());
        assert!(validate_path("/Photos\r\nDELE important").is_err());
        assert!(validate_path("/Photos\nsummer").is_err());
    }
}
