//! What the frame is doing right now, so that a frame which has stopped can say where it stopped.
//!
//! A hang is only mysterious because nothing records what was being attempted when it began. Every
//! long-lived thread stamps what it is about to do here, and the web app serves the result, so
//! diagnosing a stuck frame is loading a page rather than correlating timestamps in a log.
//!
//! The one rule that makes this useful: reading it must never need anything that can itself be
//! stuck. Everything here is a short critical section over values already in memory, so the status
//! still answers while the FTP side is wedged -- which is exactly when it is worth asking.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// Names of the threads that report here. Constants rather than free strings so that a typo
/// cannot quietly create a second entry for the same thread.
pub const FETCHER: &str = "photo-fetcher";
pub const DISPLAY: &str = "display";
pub const WEB: &str = "web";

/// What one thread is doing, and since when
#[derive(Debug, Clone)]
pub struct Activity {
    pub doing: String,
    pub since: Instant,
}

/// Something that happened once and is worth knowing the age of
#[derive(Debug, Clone)]
pub struct Event {
    pub what: String,
    pub at: Instant,
}

#[derive(Debug, Default)]
struct State {
    activities: BTreeMap<&'static str, Activity>,
    last_photo_shown: Option<Event>,
    last_photo_fetched: Option<Event>,
    album_photos: Option<usize>,
    waiting_for_a_photo: bool,
}

/// Shared by every thread that reports, and by the web app that serves it
#[derive(Debug)]
pub struct Diagnostics {
    started_at: Instant,
    state: Mutex<State>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl Diagnostics {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            state: Mutex::new(State::default()),
        }
    }

    /// Records what `thread` is about to do. Called before the thing, never after: the whole point
    /// is to name the operation that never finished.
    ///
    /// Repeating the same activity leaves its start time alone. Callers stamp from inside loops,
    /// and a timestamp that restarted every iteration would report a thread stuck for hours as
    /// having been busy for a millisecond -- the opposite of what this is for.
    pub fn doing(&self, thread: &'static str, doing: impl Into<String>) {
        let doing = doing.into();
        let mut state = self.lock();
        match state.activities.get_mut(thread) {
            Some(activity) if activity.doing == doing => (),
            Some(activity) => {
                activity.doing = doing;
                activity.since = Instant::now();
            }
            None => {
                state.activities.insert(
                    thread,
                    Activity {
                        doing,
                        since: Instant::now(),
                    },
                );
            }
        }
    }

    pub fn photo_shown(&self, what: impl Into<String>) {
        self.lock().last_photo_shown = Some(Event {
            what: what.into(),
            at: Instant::now(),
        });
    }

    /// A photo that arrived from the server, which doubles as the last proof that the source was
    /// reachable at all
    pub fn photo_fetched(&self, what: impl Into<String>) {
        self.lock().last_photo_fetched = Some(Event {
            what: what.into(),
            at: Instant::now(),
        });
    }

    pub fn album(&self, photos: usize, waiting_for_a_photo: bool) {
        let mut state = self.lock();
        state.album_photos = Some(photos);
        state.waiting_for_a_photo = waiting_for_a_photo;
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = Instant::now();
        let state = self.lock();
        Snapshot {
            uptime: now.saturating_duration_since(self.started_at),
            threads: state
                .activities
                .iter()
                .map(|(name, activity)| ThreadStatus {
                    name,
                    doing: activity.doing.clone(),
                    for_: now.saturating_duration_since(activity.since),
                })
                .collect(),
            last_photo_shown: state.last_photo_shown.as_ref().map(|event| event.aged(now)),
            last_photo_fetched: state
                .last_photo_fetched
                .as_ref()
                .map(|event| event.aged(now)),
            album_photos: state.album_photos,
            waiting_for_a_photo: state.waiting_for_a_photo,
        }
    }

    /// A panicking reporter must not make the status unreadable: a poisoned lock here would hide
    /// precisely the evidence somebody came looking for
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Event {
    fn aged(&self, now: Instant) -> AgedEvent {
        AgedEvent {
            what: self.what.clone(),
            ago: now.saturating_duration_since(self.at),
        }
    }
}

/// What a thread is doing and for how long. `for_` rather than an absolute time on purpose: the
/// question being asked is "has something been stuck", and an age answers it without arithmetic.
#[derive(Debug, Clone)]
pub struct ThreadStatus {
    pub name: &'static str,
    pub doing: String,
    pub for_: Duration,
}

#[derive(Debug, Clone)]
pub struct AgedEvent {
    pub what: String,
    pub ago: Duration,
}

/// A reading taken at one moment, owned by the caller so that nothing holds the lock while it is
/// being rendered
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub uptime: Duration,
    pub threads: Vec<ThreadStatus>,
    pub last_photo_shown: Option<AgedEvent>,
    pub last_photo_fetched: Option<AgedEvent>,
    pub album_photos: Option<usize>,
    pub waiting_for_a_photo: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_reports_what_it_is_doing() {
        let diagnostics = Diagnostics::new();

        diagnostics.doing(FETCHER, "downloading /Photos/IMG_0001.jpg");

        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.threads.len(), 1);
        assert_eq!(snapshot.threads[0].name, FETCHER);
        assert_eq!(
            snapshot.threads[0].doing,
            "downloading /Photos/IMG_0001.jpg"
        );
    }

    /// The bug being guarded against is a thread appearing twice because its name was typed
    /// differently in two places, which would make the status say two contradictory things
    #[test]
    fn reporting_again_replaces_the_previous_activity() {
        let diagnostics = Diagnostics::new();

        diagnostics.doing(FETCHER, "listing /Photos");
        diagnostics.doing(FETCHER, "downloading /Photos/IMG_0001.jpg");

        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.threads.len(), 1, "one thread, one activity");
        assert_eq!(
            snapshot.threads[0].doing,
            "downloading /Photos/IMG_0001.jpg"
        );
    }

    /// Stamping happens inside loops, so this is the difference between a status that says a
    /// thread has been stuck for an hour and one that always says a millisecond
    #[test]
    fn repeating_an_activity_keeps_the_time_it_started() {
        let diagnostics = Diagnostics::new();

        diagnostics.doing(FETCHER, "waiting for the display");
        std::thread::sleep(Duration::from_millis(30));
        diagnostics.doing(FETCHER, "waiting for the display");

        let elapsed = diagnostics.snapshot().threads[0].for_;
        assert!(
            elapsed >= Duration::from_millis(30),
            "the clock restarted, was {elapsed:?}"
        );
    }

    #[test]
    fn a_different_activity_starts_its_own_clock() {
        let diagnostics = Diagnostics::new();

        diagnostics.doing(FETCHER, "waiting for the display");
        std::thread::sleep(Duration::from_millis(30));
        diagnostics.doing(FETCHER, "downloading a photo");

        let elapsed = diagnostics.snapshot().threads[0].for_;
        assert!(
            elapsed < Duration::from_millis(30),
            "a new activity has to start counting again, was {elapsed:?}"
        );
    }

    #[test]
    fn several_threads_are_reported_together() {
        let diagnostics = Diagnostics::new();

        diagnostics.doing(FETCHER, "downloading");
        diagnostics.doing(DISPLAY, "showing IMG_0001.jpg");
        diagnostics.doing(WEB, "idle");

        let names: Vec<_> = diagnostics
            .snapshot()
            .threads
            .iter()
            .map(|thread| thread.name)
            .collect();
        assert_eq!(names, vec![DISPLAY, FETCHER, WEB], "sorted by name");
    }

    #[test]
    fn events_and_the_album_are_reported() {
        let diagnostics = Diagnostics::new();

        diagnostics.photo_fetched("/Photos/IMG_0001.jpg");
        diagnostics.photo_shown("IMG_0001.jpg");
        diagnostics.album(42, false);

        let snapshot = diagnostics.snapshot();
        assert_eq!(
            snapshot.last_photo_fetched.unwrap().what,
            "/Photos/IMG_0001.jpg"
        );
        assert_eq!(snapshot.last_photo_shown.unwrap().what, "IMG_0001.jpg");
        assert_eq!(snapshot.album_photos, Some(42));
        assert!(!snapshot.waiting_for_a_photo);
    }

    #[test]
    fn nothing_reported_yet_is_an_empty_reading_rather_than_an_error() {
        let snapshot = Diagnostics::new().snapshot();

        assert!(snapshot.threads.is_empty());
        assert!(snapshot.last_photo_shown.is_none());
        assert!(snapshot.last_photo_fetched.is_none());
        assert_eq!(snapshot.album_photos, None);
    }

    /// The status is read while other threads are reporting into it, and a reader that could be
    /// blocked by a reporter would be useless exactly when it is needed
    #[test]
    fn the_status_can_be_read_while_threads_are_reporting() {
        use std::sync::Arc;

        let diagnostics = Arc::new(Diagnostics::new());
        let reporter = Arc::clone(&diagnostics);
        let writing = std::thread::spawn(move || {
            for n in 0..500 {
                reporter.doing(FETCHER, format!("downloading {n}"));
            }
        });

        for _ in 0..500 {
            let _ = diagnostics.snapshot();
        }
        writing.join().expect("the reporting thread must not panic");

        assert_eq!(diagnostics.snapshot().threads[0].doing, "downloading 499");
    }

    #[test]
    fn a_poisoned_lock_still_answers() {
        use std::sync::Arc;

        let diagnostics = Arc::new(Diagnostics::new());
        diagnostics.doing(FETCHER, "downloading");
        let poisoner = Arc::clone(&diagnostics);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.state.lock().unwrap();
            panic!("poisoning the lock on purpose");
        })
        .join();

        let snapshot = diagnostics.snapshot();

        assert_eq!(
            snapshot.threads[0].doing, "downloading",
            "a panic while reporting must not hide the evidence"
        );
    }
}
