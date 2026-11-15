//! The album kept on disk, and the only place the slideshow reads photos from.
//!
//! See [ADR 0002](../docs/adr/0002-mirror-the-album-locally.md). The short version: a photo frame
//! whose server is busy has nothing to show, so the album is copied to the device and the display
//! reads the copy. Fetching stops being a step on the way to showing a photo and becomes a
//! background errand that keeps the copy in step with the server.
//!
//! This is deliberately not a fallback. [Mirror] wraps the real backend and *is* the backend as far
//! as everything above it is concerned, so there is no path from the display to the network and
//! therefore no rarely-exercised branch that only runs during an outage. The offline case and the
//! online case are the same case.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use mock_instant::thread_local::{Instant, MockClock};
#[cfg(not(test))]
use std::time::Instant;

use crate::{
    api_client::{ApiClient, LoginError, PhotoFacts, PhotoNotFound, SortBy},
    cli::SourceSize,
    diagnostics::{Diagnostics, MirrorReport},
    metadata::{Location, Metadata},
};

/// Characters that cannot be part of a file name, or that would make the escaping ambiguous. The
/// photo's key is otherwise left alone so that the directory can be read by a human wondering what
/// is in it -- and, more importantly, so that the index can be rebuilt from the file names alone.
const ESCAPED_IN_FILE_NAMES: &percent_encoding::AsciiSet =
    &percent_encoding::CONTROLS.add(b'%').add(b'/');

/// A key longer than this cannot be a file name on some filesystems. Such a photo is simply not
/// mirrored; it is a photo like any other, only unavailable while the server is away.
const LONGEST_FILE_NAME: usize = 200;

/// No wall clock reading before this is believable. The device has no real-time clock, so an
/// unsynchronised one reads as the epoch or as whatever was last saved to disk.
const NOT_BEFORE: &str = "2026-01-01T00:00:00Z";

/// Written by systemd-timesyncd once it has actually heard from a time server. Its presence is the
/// only cheap, local proof that the clock means anything.
const TIMESYNC_MARKER: &str = "/run/systemd/timesync/synchronized";
const TIMESYNC_DIRECTORY: &str = "/run/systemd/timesync";

/// How the mirror decides when to fetch, when to forget and when to stop.
#[derive(Clone, Debug)]
pub struct Policy {
    /// How long between listings of the album
    pub sync_interval: Duration,
    /// How long one call may spend downloading before it returns and lets the slideshow breathe
    pub budget: Duration,
    /// Successful listings a photo must be missing from before its copy is removed
    pub missing_listings_before_forgetting: u32,
    /// Days that must also have passed, measured on a clock that is known to be right
    pub missing_days_before_forgetting: i64,
    /// A listing losing more than this fraction of the album at once is not believed
    pub bulk_disappearance: f64,
    /// Consecutive failures before a photo is only retried occasionally
    pub failures_before_quarantine: u32,
    /// How long a quarantined photo is left alone
    pub quarantine_for: chrono::Duration,
    /// The share of the free space the mirror may occupy, as a blast radius rather than a budget
    pub free_space_share: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(6 * 60 * 60),
            budget: Duration::from_secs(30),
            missing_listings_before_forgetting: 2,
            missing_days_before_forgetting: 3,
            bulk_disappearance: 0.25,
            failures_before_quarantine: 3,
            quarantine_for: chrono::Duration::days(1),
            free_space_share: 0.5,
        }
    }
}

/// What the mirror holds about one photo. Everything here can be rebuilt by downloading the photo
/// again, which is what makes losing the index survivable.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Entry {
    /// The backend's change signal as of the last download. Compared only with a later value of
    /// itself.
    fingerprint: Option<String>,
    /// Size of the mirrored file, so the index can report what it occupies without stat-ing
    bytes: u64,
    /// Taken from the backend at download time, so that captions work with the server away
    date: Option<DateTime<Utc>>,
    area: Option<String>,
    country: Option<String>,
    /// Successful listings in a row this photo has not appeared in
    missing_listings: u32,
    /// When it first went missing, on a clock that was trusted at the time
    first_missing: Option<DateTime<Utc>>,
    /// Consecutive failures to download or verify
    failures: u32,
    /// When it was quarantined, if it is
    quarantined_at: Option<DateTime<Utc>>,
}

#[derive(Default, Deserialize, Serialize)]
struct Index {
    entries: BTreeMap<String, Entry>,
}

impl Index {
    /// Photos the mirror actually holds.
    ///
    /// An entry is what is *known* about a photo, which includes knowing that it has failed to
    /// download three times running. Only an entry with bytes behind it is a photo the frame can
    /// show, and conflating the two is how a frame ends up offering a photo that never arrived.
    fn held(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.entries.iter().filter(|(_, entry)| entry.bytes > 0)
    }

    fn holds(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|entry| entry.bytes > 0)
    }
}

/// What the slideshow is handed instead of a backend photo. It carries nothing but the key,
/// because everything else is already on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirroredPhoto {
    key: String,
}

/// The caption of a mirrored photo, answered from the index rather than the server
struct MirroredMetadata {
    date: DateTime<Utc>,
    area: Option<String>,
    country: Option<String>,
}

impl Metadata for MirroredMetadata {
    fn date(&self) -> DateTime<Utc> {
        self.date
    }

    fn location(&self) -> Location {
        Location::new(self.area.clone(), self.country.clone())
    }
}

/// What the mirror can say about itself, for the status page
#[derive(Clone, Debug, Default)]
pub struct MirrorStatus {
    pub mirrored: usize,
    pub listed: Option<usize>,
    pub bytes: u64,
    pub quarantined: usize,
    pub last_sync_seconds_ago: Option<u64>,
    pub bulk_guard_tripped: bool,
    pub out_of_space: bool,
}

struct State<P> {
    index: Index,
    /// The last successful listing, kept so that filling can continue after the listing that
    /// discovered the work has finished. Behind an [Arc] per photo so that one can be taken out to
    /// download without holding the lock across the transfer, and without asking the backends'
    /// data types to be cloneable.
    listing: Vec<Arc<P>>,
    /// Keys of that listing, in the order the backend returned them
    order: Vec<String>,
    last_listing: Option<Instant>,
    last_sync: Option<Instant>,
    bulk_guard_tripped: bool,
    out_of_space: bool,
}

/// A local copy of the album that answers as though it were the backend
pub struct Mirror<A: ApiClient> {
    inner: A,
    photos: PathBuf,
    index_file: PathBuf,
    policy: Policy,
    /// The most the mirror may occupy, fixed when it was opened. A blast radius rather than a
    /// budget: at about a gigabyte against nine free it should never be reached.
    cap: Option<u64>,
    /// The wall clock, as far as the mirror is concerned. A function rather than a direct call so
    /// that a test can say what the time is, and can say that nobody knows.
    clock: fn() -> Option<DateTime<Utc>>,
    diagnostics: Option<std::sync::Arc<Diagnostics>>,
    state: Mutex<State<A::Photo>>,
}

impl<A: ApiClient> Mirror<A> {
    /// Opens, or creates, a mirror in `directory`.
    ///
    /// The index is read if it is there and rebuilt from the directory if it is not, so a lost or
    /// corrupt index costs one round of revalidation rather than the whole copy. Anything an
    /// interrupted pass left behind is swept at the same time, which is what makes the mirror
    /// converge after the power has been cut mid-download.
    pub fn open(inner: A, directory: &Path, policy: Policy) -> Result<Self> {
        let photos = directory.join("photos");
        fs::create_dir_all(&photos)
            .with_context(|| format!("Could not make {}", photos.display()))?;
        let index_file = directory.join("index.json");

        sweep_temporary_files(&photos);
        let index = read_index(&index_file).unwrap_or_else(|error| {
            log::warn!("Rebuilding the mirror index: {error:#}");
            Index::default()
        });
        let index = adopt_unknown_files(index, &photos);
        log::info!(
            "Mirroring the album in {} ({} photos already here)",
            directory.display(),
            index.entries.len()
        );

        let cap = free_space(&photos).map(|free| (free as f64 * policy.free_space_share) as u64);
        Ok(Self {
            inner,
            photos,
            index_file,
            policy,
            cap,
            clock: trusted_now,
            diagnostics: None,
            state: Mutex::new(State {
                index,
                listing: Vec::new(),
                order: Vec::new(),
                last_listing: None,
                last_sync: None,
                bulk_guard_tripped: false,
                out_of_space: false,
            }),
        })
    }

    /// Reports what the mirror holds to the status page after every pass.
    pub fn with_diagnostics(mut self, diagnostics: Option<std::sync::Arc<Diagnostics>>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Replaces the clock, for tests that need a known date or a clock nobody trusts.
    #[cfg(test)]
    fn with_clock(mut self, clock: fn() -> Option<DateTime<Utc>>) -> Self {
        self.clock = clock;
        self
    }

    pub fn status(&self) -> MirrorStatus {
        let state = self.lock();
        MirrorStatus {
            mirrored: state.index.held().count(),
            listed: (!state.order.is_empty()).then(|| state.order.len()),
            bytes: state.index.entries.values().map(|entry| entry.bytes).sum(),
            quarantined: state
                .index
                .entries
                .values()
                .filter(|entry| entry.quarantined_at.is_some())
                .count(),
            last_sync_seconds_ago: state.last_sync.map(|at| (Instant::now() - at).as_secs()),
            bulk_guard_tripped: state.bulk_guard_tripped,
            out_of_space: state.out_of_space,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<A::Photo>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn file_for(&self, key: &str) -> Option<PathBuf> {
        let name = percent_encoding::utf8_percent_encode(key, ESCAPED_IN_FILE_NAMES).to_string();
        (name.len() <= LONGEST_FILE_NAME).then(|| self.photos.join(name))
    }

    /// Lists the album if a listing is due, then spends up to the budget downloading what is
    /// missing. Every failure here is survivable: the mirror simply stays as it was.
    fn sync(&self, sort_by: SortBy) {
        if self.listing_is_due() {
            match self.inner.get_photo_metadata(sort_by) {
                Ok(listing) => self.accept_listing(listing),
                /* A failed listing must never be read as an empty album. This is the invariant the
                 * whole deletion policy rests on, and it belongs to the backend: it returns an
                 * error rather than a short list. Nothing is forgotten on this path. */
                Err(error) => log::warn!("Could not list the album: {error:#}"),
            }
        }
        self.fill();
        self.report();
    }

    fn report(&self) {
        let Some(diagnostics) = &self.diagnostics else {
            return;
        };
        let status = self.status();
        diagnostics.mirror(MirrorReport {
            held: status.mirrored,
            listed: status.listed,
            bytes: status.bytes,
            quarantined: status.quarantined,
            last_sync_seconds_ago: status.last_sync_seconds_ago,
            bulk_guard_tripped: status.bulk_guard_tripped,
            out_of_space: status.out_of_space,
        });
    }

    fn listing_is_due(&self) -> bool {
        let state = self.lock();
        state
            .last_listing
            .is_none_or(|at| (Instant::now() - at) >= self.policy.sync_interval)
    }

    /// Records a listing, decides what has gone, and keeps it for filling from.
    fn accept_listing(&self, listing: Vec<A::Photo>) {
        let facts: Vec<PhotoFacts> = listing
            .iter()
            .map(|photo| self.inner.photo_facts(photo))
            .collect();
        let listed: std::collections::BTreeSet<&str> =
            facts.iter().map(|fact| fact.key.as_str()).collect();

        let mut state = self.lock();
        let known = state.index.entries.len();
        let missing = state
            .index
            .entries
            .keys()
            .filter(|key| !listed.contains(key.as_str()))
            .count();

        /* A listing that loses a large part of the album at once is not believed. RFC 3659 gives a
         * client no way to tell a complete listing from one truncated while the server is busy, and
         * this mirror is the last copy standing precisely when the server is behaving badly. */
        let bulk = known > 0 && (missing as f64 / known as f64) > self.policy.bulk_disappearance;
        state.bulk_guard_tripped = bulk;
        if bulk {
            log::error!(
                "{missing} of {known} mirrored photos are absent from this listing, which is more \
                 than the mirror will believe at once. Nothing has been removed."
            );
        } else {
            self.forget_what_is_gone(&mut state, &listed);
        }

        /* Only photos actually on disk have entries: an entry is the record of a copy, not of
         * a sighting. Inventing one here would make the mirror claim a photo it does not have. */
        for fact in &facts {
            if let Some(entry) = state.index.entries.get_mut(&fact.key) {
                entry.missing_listings = 0;
                entry.first_missing = None;
            }
        }
        state.order = facts.into_iter().map(|fact| fact.key).collect();
        state.listing = listing.into_iter().map(Arc::new).collect();
        state.last_listing = Some(Instant::now());
        state.last_sync = Some(Instant::now());
    }

    /// Removes the copies of photos that have been absent long enough to believe they are gone.
    fn forget_what_is_gone(
        &self,
        state: &mut State<A::Photo>,
        listed: &std::collections::BTreeSet<&str>,
    ) {
        let now = (self.clock)();
        let mut forget = Vec::new();
        for (key, entry) in &mut state.index.entries {
            if listed.contains(key.as_str()) {
                continue;
            }
            entry.missing_listings += 1;
            if entry.first_missing.is_none() {
                entry.first_missing = now;
            }
            /* Nothing is aged out while the clock is untrusted. After a power cut this device's
             * time is restored from a saved stamp and is wrong until a time server answers -- over
             * the network whose absence is the reason this mirror exists. */
            let Some(now) = now else { continue };
            let Some(since) = entry.first_missing else {
                continue;
            };
            if entry.missing_listings >= self.policy.missing_listings_before_forgetting
                && (now - since).num_days() >= self.policy.missing_days_before_forgetting
            {
                forget.push(key.clone());
            }
        }
        for key in forget {
            if let Some(file) = self.file_for(&key) {
                let _ = fs::remove_file(file);
            }
            state.index.entries.remove(&key);
            log::info!("{key} has left the album, so its copy has been removed");
        }
    }

    /// Downloads what is missing or stale, for as long as the budget allows.
    fn fill(&self) {
        let deadline = Instant::now() + self.policy.budget;
        let mut wrote_anything = false;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            let Some((photo, fact)) = self.next_to_fetch() else {
                break;
            };
            if self.out_of_room() {
                let mut state = self.lock();
                if !state.out_of_space {
                    log::error!(
                        "The mirror has reached its share of the free space and has stopped \
                         downloading. Photos already here are unaffected."
                    );
                }
                state.out_of_space = true;
                break;
            }
            match self.fetch_and_store(photo.as_ref(), &fact) {
                Ok(()) => wrote_anything = true,
                Err(error) => {
                    log::warn!("Could not mirror {}: {error:#}", fact.key);
                    self.record_failure(&fact.key);
                }
            }
        }
        if wrote_anything {
            /* One directory sync for the whole pass rather than one per photo. Losing a name costs
             * a re-download; losing the bytes behind a name would serve a broken photo for ever,
             * and that is synced separately, per photo, in store(). */
            sync_directory(&self.photos);
            self.save_index();
        }
    }

    /// The next photo worth fetching, in listing order, or [None] when the mirror is up to date.
    fn next_to_fetch(&self) -> Option<(Arc<A::Photo>, PhotoFacts)> {
        let now = (self.clock)();
        let state = self.lock();
        for (position, key) in state.order.iter().enumerate() {
            let entry = state.index.entries.get(key);
            if let Some(entry) = entry
                && let Some(quarantined_at) = entry.quarantined_at
                && let Some(now) = now
                && now - quarantined_at < self.policy.quarantine_for
            {
                continue;
            }
            let photo = state.listing.get(position)?;
            let fact = self.inner.photo_facts(photo);
            let photo = Arc::clone(photo);
            let have = entry.is_some_and(|entry| entry.bytes > 0);
            let current = entry.is_some_and(|entry| {
                fact.fingerprint.is_none() || entry.fingerprint == fact.fingerprint
            });
            if have && current {
                continue;
            }
            return Some((photo, fact));
        }
        None
    }

    /// Fetches one photo, verifies it, and publishes it only once it is whole.
    fn fetch_and_store(&self, photo: &A::Photo, fact: &PhotoFacts) -> Result<()> {
        let Some(file) = self.file_for(&fact.key) else {
            return Err(anyhow!("its name is too long to keep on disk"));
        };
        let bytes = self.inner.get_photo_bytes(photo, SourceSize::L)?;
        /* Verified here rather than trusted from the wire. A transfer can end early -- this
         * project has a server that answers an abort with two replies and drops the session
         * mid-file -- and a short JPEG that is never decoded would be published as a photo and
         * shown as a torn image. */
        if bytes.is_empty() {
            return Err(anyhow!("the server sent no bytes"));
        }
        image::load_from_memory(&bytes).context("what arrived is not a decodable image")?;

        let (date, area, country) = match self.inner.get_exif(photo) {
            Ok(metadata) => {
                let location = metadata.location();
                (Some(metadata.date()), location.area, location.country)
            }
            Err(error) => {
                log::debug!("No caption for {}: {error:#}", fact.key);
                (None, None, None)
            }
        };

        store(&file, &bytes)?;

        let mut state = self.lock();
        state.index.entries.insert(
            fact.key.clone(),
            Entry {
                fingerprint: fact.fingerprint.clone(),
                bytes: bytes.len() as u64,
                date,
                area,
                country,
                missing_listings: 0,
                first_missing: None,
                failures: 0,
                quarantined_at: None,
            },
        );
        log::debug!("Mirrored {} ({} bytes)", fact.key, bytes.len());
        Ok(())
    }

    fn record_failure(&self, key: &str) {
        let now = (self.clock)();
        let mut state = self.lock();
        let entry = state.index.entries.entry(key.to_string()).or_default();
        entry.failures += 1;
        if entry.failures >= self.policy.failures_before_quarantine
            && entry.quarantined_at.is_none()
        {
            entry.quarantined_at = now;
            log::warn!(
                "{key} has failed {} times and will only be retried occasionally",
                entry.failures
            );
        }
    }

    fn out_of_room(&self) -> bool {
        let used: u64 = self
            .lock()
            .index
            .entries
            .values()
            .map(|entry| entry.bytes)
            .sum();
        self.cap.is_some_and(|cap| used >= cap)
    }

    fn save_index(&self) {
        let state = self.lock();
        match serde_json::to_vec(&state.index) {
            Ok(serialised) => {
                if let Err(error) = store(&self.index_file, &serialised) {
                    log::warn!("Could not write the mirror index: {error:#}");
                } else if let Some(directory) = self.index_file.parent() {
                    sync_directory(directory);
                }
            }
            Err(error) => log::warn!("Could not serialise the mirror index: {error:#}"),
        }
    }
}

impl<A: ApiClient> ApiClient for Mirror<A> {
    type Photo = MirroredPhoto;

    fn is_logged_in(&self) -> bool {
        self.inner.is_logged_in()
    }

    /// A server that will not let us in is fatal only while there is nothing to show. Once the
    /// album is mirrored the frame has a job it can do without the server, and refusing to start
    /// would throw that away.
    fn login(&self) -> Result<(), LoginError> {
        match self.inner.login() {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.lock().index.held().next().is_none() {
                    Err(error)
                } else {
                    log::warn!(
                        "Could not log in to the server, carrying on with the mirrored album: \
                         {error}"
                    );
                    Ok(())
                }
            }
        }
    }

    fn get_photo_metadata(&self, sort_by: SortBy) -> Result<Vec<Self::Photo>> {
        self.sync(sort_by);
        let state = self.lock();
        /* In the order the backend last gave, so that --order by name or time still means what it
         * says, with anything not yet mirrored left out rather than waited for. */
        let mut photos: Vec<MirroredPhoto> = state
            .order
            .iter()
            .filter(|key| state.index.holds(key))
            .map(|key| MirroredPhoto { key: key.clone() })
            .collect();
        if photos.is_empty() {
            /* No listing has succeeded yet this run -- a frame that has just started with the
             * server away. Everything held, in whatever order the index gives. */
            photos = state
                .index
                .held()
                .map(|(key, _)| MirroredPhoto { key: key.clone() })
                .collect();
        }
        Ok(photos)
    }

    fn get_exif(&self, photo: &Self::Photo) -> Result<impl Metadata> {
        let state = self.lock();
        let entry = state
            .index
            .entries
            .get(&photo.key)
            .ok_or_else(|| anyhow!("{} is not mirrored", photo.key))?;
        let date = entry
            .date
            .ok_or_else(|| anyhow!("No date is known for {}", photo.key))?;
        Ok(MirroredMetadata {
            date,
            area: entry.area.clone(),
            country: entry.country.clone(),
        })
    }

    /// Always from disk. There is deliberately no route to the network here.
    fn get_photo_bytes(&self, photo: &Self::Photo, _: SourceSize) -> Result<Bytes> {
        let file = self
            .file_for(&photo.key)
            .ok_or_else(|| anyhow!("{} cannot be kept on disk", photo.key))?;
        match fs::read(&file) {
            Ok(bytes) if !bytes.is_empty() => Ok(Bytes::from(bytes)),
            /* Not mirrored, or mirrored as nothing. Reported the way a photo that has left the
             * album is reported, so the slideshow skips it instead of showing an error. */
            Ok(_) | Err(_) => {
                Err(PhotoNotFound(anyhow!("{} is not mirrored yet", photo.key)).into())
            }
        }
    }

    fn photo_facts(&self, photo: &Self::Photo) -> PhotoFacts {
        let state = self.lock();
        PhotoFacts {
            key: photo.key.clone(),
            fingerprint: state
                .index
                .entries
                .get(&photo.key)
                .and_then(|entry| entry.fingerprint.clone()),
        }
    }
}

/// Writes `bytes` to `file`, and does not publish the name until the bytes are on the card.
///
/// A rename is atomic against data already written, not against data still in the page cache, so
/// without the flush a power cut can leave the new name in the directory with nothing behind it.
/// This frame is only ever switched off by cutting the power, so that is the ordinary ending.
fn store(file: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = file.with_extension(format!("{}.partial", std::process::id()));
    let written = File::create(&temporary).and_then(|mut handle| {
        handle.write_all(bytes)?;
        handle.sync_all()
    });
    if let Err(error) = written.and_then(|()| fs::rename(&temporary, file)) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("Could not write {}", file.display()));
    }
    Ok(())
}

/// Makes the names created in `directory` durable. Called once per pass: losing a name costs a
/// re-download, which is not worth a sync per photo.
fn sync_directory(directory: &Path) {
    match File::open(directory) {
        Ok(handle) => {
            if let Err(error) = handle.sync_all() {
                log::debug!("Could not sync {}: {error}", directory.display());
            }
        }
        Err(error) => log::debug!("Could not open {}: {error}", directory.display()),
    }
}

fn read_index(file: &Path) -> Result<Index> {
    let bytes = fs::read(file).with_context(|| format!("Could not read {}", file.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("Could not parse {}", file.display()))
}

/// Deletes what an interrupted pass left behind.
fn sweep_temporary_files(photos: &Path) {
    let Ok(entries) = fs::read_dir(photos) else {
        return;
    };
    for file in entries.flatten() {
        let path = file.path();
        if path.extension().is_some_and(|end| end == "partial") {
            let _ = fs::remove_file(path);
        }
    }
}

/// Takes ownership of any mirrored photo the index does not mention.
///
/// This is what converges the mirror after a power cut: the index is written once per pass, so a
/// cut between a download and the index being saved leaves a perfectly good photo that nothing
/// knows about. Its fingerprint is unknown, so it is revalidated on the next listing rather than
/// trusted.
fn adopt_unknown_files(mut index: Index, photos: &Path) -> Index {
    let Ok(entries) = fs::read_dir(photos) else {
        return index;
    };
    let mut adopted = 0;
    let mut present = std::collections::BTreeSet::new();
    for file in entries.flatten() {
        let path = file.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(key) = percent_encoding::percent_decode_str(name).decode_utf8() else {
            continue;
        };
        let key = key.into_owned();
        let bytes = file.metadata().map(|data| data.len()).unwrap_or_default();
        if bytes == 0 {
            let _ = fs::remove_file(&path);
            continue;
        }
        present.insert(key.clone());
        let entry = index.entries.entry(key).or_insert_with(|| {
            adopted += 1;
            Entry::default()
        });
        entry.bytes = bytes;
    }
    /* An index entry with no file behind it is a photo the mirror does not actually have. Dropping
     * it is what makes the next pass fetch it again. */
    index.entries.retain(|key, _| present.contains(key));
    if adopted > 0 {
        log::info!("Adopted {adopted} mirrored photos the index did not mention");
    }
    index
}

/// The wall clock, if there is any reason to believe it.
///
/// The device has no real-time clock. After a power cut the time is restored from a stamp saved
/// periodically to disk, so it is plausible-looking and wrong until a time server is reached. Where
/// systemd-timesyncd is in use its marker file is the proof; otherwise a reading implausibly far in
/// the past is all that can be checked.
fn trusted_now() -> Option<DateTime<Utc>> {
    let now: DateTime<Utc> = SystemTime::now().into();
    if Path::new(TIMESYNC_DIRECTORY).is_dir() && !Path::new(TIMESYNC_MARKER).exists() {
        return None;
    }
    let floor = DateTime::parse_from_rfc3339(NOT_BEFORE).ok()?.to_utc();
    (now >= floor).then_some(now)
}

/// Bytes available on the filesystem holding `path`, where that can be found out.
#[cfg(unix)]
fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } == 0)
        .then(|| stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_space(_: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use chrono::TimeZone;

    use super::*;
    use crate::test_helpers::api::{FakeApiClient, FakePhoto, PHOTO};

    /* Two clocks a test can rely on: one that knows the date, and one that admits it does not --
     * which is what this device's clock is after a power cut until a time server answers. */
    fn a_known_monday() -> Option<DateTime<Utc>> {
        Some(Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap())
    }

    fn a_week_later() -> Option<DateTime<Utc>> {
        Some(Utc.with_ymd_and_hms(2026, 6, 8, 12, 0, 0).unwrap())
    }

    fn nobody_knows() -> Option<DateTime<Utc>> {
        None
    }

    fn temporary_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "syno-photo-frame-mirror-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("a temporary directory has to be creatable");
        directory
    }

    fn open(inner: FakeApiClient, directory: &Path) -> Mirror<FakeApiClient> {
        Mirror::open(inner, directory, Policy::default())
            .expect("a mirror has to open in a fresh directory")
            .with_clock(a_known_monday)
    }

    fn keys(photos: &[MirroredPhoto]) -> Vec<String> {
        photos.iter().map(|photo| photo.key.clone()).collect()
    }

    /// The whole point: the album is on screen while the server is not answering.
    #[test]
    fn photos_are_served_while_the_backend_is_unreachable() {
        let directory = temporary_directory("unreachable");
        let mirror = open(FakeApiClient::with_album(&["a.jpg", "b.jpg"]), &directory);
        mirror
            .get_photo_metadata(SortBy::TakenTime)
            .expect("the first pass mirrors the album");

        /* The server goes away completely */
        mirror.inner.listing_fails.store(true, Ordering::Relaxed);
        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        let photos = mirror
            .get_photo_metadata(SortBy::TakenTime)
            .expect("an unreachable server must not fail the listing");

        assert_eq!(keys(&photos), vec!["a.jpg", "b.jpg"]);
        for photo in &photos {
            let bytes = mirror
                .get_photo_bytes(photo, SourceSize::L)
                .expect("a mirrored photo is readable without the server");
            assert_eq!(bytes.len(), PHOTO.len());
        }
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_mirrored_photo_is_not_downloaded_again() {
        let directory = temporary_directory("no-redownload");
        let mirror = open(FakeApiClient::with_album(&["a.jpg"]), &directory);
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        let after_first = mirror.inner.downloads();

        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert_eq!(after_first, 1, "the first pass has to fetch it once");
        assert_eq!(
            mirror.inner.downloads(),
            1,
            "nothing about the photo changed, so it must not be fetched again"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_changed_photo_is_downloaded_again() {
        let directory = temporary_directory("changed");
        let mirror = open(FakeApiClient::with_album(&["a.jpg"]), &directory);
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        mirror
            .inner
            .set_photos(vec![FakePhoto::new("a.jpg").with_fingerprint("v2")]);
        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert_eq!(
            mirror.inner.downloads(),
            2,
            "the server says the contents moved, so the copy is stale"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// A transfer that ends early must never replace a good copy, and must never be shown. This
    /// frame's own server drops a session mid-file, so this is a real case, not a hypothetical.
    #[test]
    fn a_download_that_is_not_a_whole_photo_is_discarded() {
        let directory = temporary_directory("truncated");
        let client = FakeApiClient::with_album(&["a.jpg"]);
        client.breaks("a.jpg", b"\xff\xd8not a whole jpeg");
        let mirror = open(client, &directory);

        let photos = mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert!(
            photos.is_empty(),
            "a photo that never arrived whole must not be offered"
        );
        assert!(
            mirror
                .get_photo_bytes(
                    &MirroredPhoto {
                        key: "a.jpg".into()
                    },
                    SourceSize::L
                )
                .is_err(),
            "and must not be readable"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_empty_download_is_discarded() {
        let directory = temporary_directory("empty-download");
        let client = FakeApiClient::with_album(&["a.jpg"]);
        client.breaks("a.jpg", b"");
        let mirror = open(client, &directory);

        assert!(
            mirror
                .get_photo_metadata(SortBy::TakenTime)
                .unwrap()
                .is_empty(),
            "nothing arrived, so there is nothing to show"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// The invariant the whole deletion policy rests on: a listing that failed is not an empty
    /// album.
    #[test]
    fn a_failed_listing_removes_nothing() {
        let directory = temporary_directory("failed-listing");
        let mirror = open(FakeApiClient::with_album(&["a.jpg", "b.jpg"]), &directory);
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        mirror.inner.listing_fails.store(true, Ordering::Relaxed);
        for _ in 0..5 {
            MockClock::advance(Duration::from_secs(7 * 60 * 60));
            mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        }

        assert_eq!(
            keys(&mirror.get_photo_metadata(SortBy::TakenTime).unwrap()).len(),
            2,
            "a server that will not answer has not deleted anybody's photos"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_photo_missing_from_one_listing_is_kept() {
        let directory = temporary_directory("missing-once");
        let mirror = open(
            FakeApiClient::with_album(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]),
            &directory,
        );
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        mirror.inner.set_album(&["a.jpg", "b.jpg", "c.jpg"]);
        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert!(
            mirror.lock().index.entries.contains_key("d.jpg"),
            "one absence is not evidence enough to throw the only copy away"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_photo_absent_for_long_enough_is_forgotten() {
        let directory = temporary_directory("forgotten");
        let mirror = open(
            FakeApiClient::with_album(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]),
            &directory,
        );
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        mirror.inner.set_album(&["a.jpg", "b.jpg", "c.jpg"]);

        /* Two successful listings it is missing from, and days apart on a clock that is believed */
        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        let mirror = mirror.with_clock(a_week_later);
        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert!(
            !mirror.lock().index.entries.contains_key("d.jpg"),
            "it has been gone for two listings and a week, so the copy goes too"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// RFC 3659 gives no way to tell a complete listing from one truncated while the server is
    /// busy, so a listing that loses a large share of the album is not believed at all.
    #[test]
    fn a_listing_that_loses_most_of_the_album_removes_nothing() {
        let directory = temporary_directory("bulk");
        let mirror = open(
            FakeApiClient::with_album(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]),
            &directory,
        );
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        mirror.inner.set_album(&["a.jpg"]);
        let mirror = mirror.with_clock(a_week_later);
        for _ in 0..4 {
            MockClock::advance(Duration::from_secs(7 * 60 * 60));
            mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        }

        assert_eq!(
            mirror.lock().index.entries.len(),
            4,
            "three quarters of the album vanishing at once is a server fault, not a deletion"
        );
        assert!(
            mirror.status().bulk_guard_tripped,
            "and it has to be visible rather than silent"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// After a power cut the clock is restored from a saved stamp and is wrong until a time server
    /// answers -- over the network whose absence the mirror exists to survive.
    #[test]
    fn nothing_is_forgotten_while_the_clock_is_untrusted() {
        let directory = temporary_directory("no-clock");
        let mirror = open(
            FakeApiClient::with_album(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]),
            &directory,
        );
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        mirror.inner.set_album(&["a.jpg", "b.jpg", "c.jpg"]);
        let mirror = mirror.with_clock(nobody_knows);
        for _ in 0..6 {
            MockClock::advance(Duration::from_secs(7 * 60 * 60));
            mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        }

        assert!(
            mirror.lock().index.entries.contains_key("d.jpg"),
            "a frame that does not know the date must not age photos out"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_index_that_cannot_be_read_is_rebuilt_from_the_photos() {
        let directory = temporary_directory("rebuild");
        {
            let mirror = open(FakeApiClient::with_album(&["a.jpg", "b.jpg"]), &directory);
            mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        }
        fs::write(directory.join("index.json"), b"this is not json").unwrap();

        let reopened = open(FakeApiClient::with_album(&["a.jpg", "b.jpg"]), &directory);

        assert_eq!(
            reopened.lock().index.entries.len(),
            2,
            "the file names are the keys, so the index is an optimisation and not the copy itself"
        );
        assert_eq!(
            reopened.inner.downloads(),
            0,
            "and rebuilding it must not cost a single download"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// The index is written once per pass, so a power cut between a download and that write leaves
    /// a perfectly good photo nothing knows about.
    #[test]
    fn a_photo_the_index_does_not_mention_is_adopted() {
        let directory = temporary_directory("adopt");
        {
            let mirror = open(FakeApiClient::with_album(&["a.jpg"]), &directory);
            mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        }
        fs::write(directory.join("photos").join("stowaway.jpg"), PHOTO).unwrap();

        let reopened = open(FakeApiClient::with_album(&["a.jpg"]), &directory);

        assert!(
            reopened.lock().index.entries.contains_key("stowaway.jpg"),
            "a photo on disk is a photo the mirror has, whatever the index says"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn what_an_interrupted_pass_left_behind_is_swept() {
        let directory = temporary_directory("sweep");
        let photos = directory.join("photos");
        fs::create_dir_all(&photos).unwrap();
        fs::write(
            photos.join("half-written.jpg.1234.partial"),
            b"half a photo",
        )
        .unwrap();

        let mirror = open(FakeApiClient::with_album(&[]), &directory);

        assert_eq!(
            fs::read_dir(&photos).unwrap().count(),
            0,
            "a temporary file from a pass that never finished is rubbish, not a photo"
        );
        assert_eq!(mirror.lock().index.entries.len(), 0);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_empty_file_is_never_adopted_as_a_photo() {
        let directory = temporary_directory("empty-file");
        let photos = directory.join("photos");
        fs::create_dir_all(&photos).unwrap();
        fs::write(photos.join("nothing.jpg"), b"").unwrap();

        let mirror = open(FakeApiClient::with_album(&[]), &directory);

        assert!(
            !mirror.lock().index.entries.contains_key("nothing.jpg"),
            "an empty file is the shape a power cut leaves, not a photo"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_photo_that_keeps_failing_is_left_alone_eventually() {
        let directory = temporary_directory("quarantine");
        let client = FakeApiClient::with_album(&["bad.jpg"]);
        client.breaks("bad.jpg", b"not a photo at all");
        let mirror = open(client, &directory);

        for _ in 0..5 {
            MockClock::advance(Duration::from_secs(7 * 60 * 60));
            mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        }
        let after_quarantine = mirror.inner.downloads();
        MockClock::advance(Duration::from_secs(7 * 60 * 60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert!(
            after_quarantine <= 4,
            "it should have stopped trying after a few failures, tried {after_quarantine} times"
        );
        assert_eq!(
            mirror.inner.downloads(),
            after_quarantine,
            "a quarantined photo is not retried on the next pass"
        );
        assert_eq!(mirror.status().quarantined, 1);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn one_bad_photo_does_not_stop_the_others_arriving() {
        let directory = temporary_directory("carry-on");
        let client = FakeApiClient::with_album(&["a.jpg", "bad.jpg", "c.jpg"]);
        client.breaks("bad.jpg", b"not a photo at all");
        let mirror = open(client, &directory);

        let photos = mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert_eq!(
            keys(&photos),
            vec!["a.jpg", "c.jpg"],
            "the good photos arrive and the bad one is simply absent"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// Reported the way a photo that has left the album is reported, so the slideshow skips it
    /// rather than showing an error screen.
    #[test]
    fn a_photo_that_is_not_mirrored_yet_reads_as_not_found() {
        let directory = temporary_directory("not-yet");
        let mirror = open(FakeApiClient::with_album(&[]), &directory);

        let error = mirror
            .get_photo_bytes(
                &MirroredPhoto {
                    key: "unknown.jpg".into(),
                },
                SourceSize::L,
            )
            .expect_err("there is nothing to read");

        assert!(
            error.downcast_ref::<PhotoNotFound>().is_some(),
            "it has to be the error the slideshow already knows how to skip"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// A server that will not let us in is fatal only while there is nothing to show.
    #[test]
    fn a_login_failure_is_survivable_once_the_album_is_mirrored() {
        let directory = temporary_directory("login");
        let mirror = open(FakeApiClient::with_album(&["a.jpg"]), &directory);
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        mirror.inner.login_fails.store(true, Ordering::Relaxed);

        assert!(
            mirror.login().is_ok(),
            "the frame has an album to show, so it has no reason to give up"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_login_failure_with_nothing_mirrored_still_stops_the_frame() {
        let directory = temporary_directory("login-empty");
        let client = FakeApiClient::with_album(&["a.jpg"]);
        client.login_fails.store(true, Ordering::Relaxed);
        let mirror = open(client, &directory);

        assert!(
            mirror.login().is_err(),
            "with nothing to show, a wrong password is still worth reporting"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_album_is_listed_once_per_interval_rather_than_once_per_call() {
        let directory = temporary_directory("interval");
        let mirror = open(FakeApiClient::with_album(&["a.jpg"]), &directory);

        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();
        MockClock::advance(Duration::from_secs(60));
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        assert_eq!(
            mirror.inner.listings(),
            1,
            "a minute later is not a new pass; the server is asked every six hours"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_status_reports_what_is_mirrored() {
        let directory = temporary_directory("status");
        let mirror = open(FakeApiClient::with_album(&["a.jpg", "b.jpg"]), &directory);
        mirror.get_photo_metadata(SortBy::TakenTime).unwrap();

        let status = mirror.status();

        assert_eq!(status.mirrored, 2);
        assert_eq!(status.listed, Some(2));
        assert_eq!(status.bytes, 2 * PHOTO.len() as u64);
        assert_eq!(status.last_sync_seconds_ago, Some(0));
        assert!(!status.bulk_guard_tripped);
        let _ = fs::remove_dir_all(&directory);
    }
}
