//! Switching the display off while nobody is around to look at it

use std::time::Duration;

#[cfg(test)]
use mock_instant::thread_local::Instant;
#[cfg(not(test))]
use std::time::Instant;

use anyhow::Result;

pub use crate::standby::display_power::CommandDisplayPower;

mod display_power;
#[cfg(all(feature = "motion-sensor", target_os = "linux"))]
mod gpio_sensor;

/// How often the sensor is read, independently of how often the slideshow loop runs
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long the sensor has to report motion continuously before it is believed, which filters out
/// spurious spikes on its output pin
const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// Isolates the motion sensor hardware for testing
#[cfg_attr(test, mockall::automock)]
pub trait MotionSensor {
    /// True while the sensor signals movement
    fn is_motion_detected(&self) -> Result<bool>;
}

/// Isolates switching the display on and off for testing
#[cfg_attr(test, mockall::automock)]
pub trait DisplayPower {
    fn set(&self, state: PowerState) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerState {
    On,
    Standby,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayState {
    Awake,
    Asleep,
}

/// Creates the motion sensor of the platform. Returns an error, rather than panicking, when the
/// application was built without support for one.
pub fn new_motion_sensor(gpio_pin: u8) -> Result<Box<dyn MotionSensor>> {
    #[cfg(all(feature = "motion-sensor", target_os = "linux"))]
    return Ok(Box::new(gpio_sensor::GpioMotionSensor::new(gpio_pin)?));

    #[cfg(not(all(feature = "motion-sensor", target_os = "linux")))]
    {
        let _ = gpio_pin;
        anyhow::bail!(
            "--motion-sensor-gpio is not supported by this build. Rebuild on Linux with \
             `cargo install syno-photo-frame --features motion-sensor`."
        )
    }
}

/// Keeps track of whether anybody is around, and switches the display accordingly
pub struct Standby(Option<Sensing>);

impl Standby {
    /// Never switches the display off
    pub const fn disabled() -> Self {
        Self(None)
    }

    pub fn new(
        sensor: Box<dyn MotionSensor>,
        display_power: Box<dyn DisplayPower>,
        timeout: Duration,
    ) -> Self {
        let now = Instant::now();
        Self(Some(Sensing {
            sensor,
            display_power,
            timeout,
            state: DisplayState::Awake,
            last_poll: now,
            last_motion: now,
            motion_since: None,
        }))
    }

    /// Reads the sensor, at most once per [POLL_INTERVAL], and switches the display when presence
    /// changed. Meant to be called on every iteration of the slideshow loop.
    ///
    /// Errors are logged rather than propagated: neither a broken sensor nor a display that refuses
    /// to be switched is a reason to stop a running slideshow.
    pub fn update(&mut self) -> DisplayState {
        match &mut self.0 {
            Some(sensing) => sensing.update(),
            None => DisplayState::Awake,
        }
    }
}

impl Drop for Standby {
    /// Leaving a switched off display behind would look like a broken frame
    fn drop(&mut self) {
        if let Some(sensing) = &mut self.0
            && sensing.state == DisplayState::Asleep
        {
            sensing.switch(DisplayState::Awake);
        }
    }
}

struct Sensing {
    sensor: Box<dyn MotionSensor>,
    display_power: Box<dyn DisplayPower>,
    timeout: Duration,
    state: DisplayState,
    last_poll: Instant,
    /// When motion was last confirmed, i.e. after debouncing
    last_motion: Instant,
    /// When the sensor started reporting motion, or [None] while it reports none
    motion_since: Option<Instant>,
}

impl Sensing {
    fn update(&mut self) -> DisplayState {
        let now = Instant::now();
        if now - self.last_poll < POLL_INTERVAL {
            return self.state;
        }
        self.last_poll = now;

        if self.is_motion_confirmed(now) {
            self.last_motion = now;
        }
        match (self.state, now - self.last_motion >= self.timeout) {
            (DisplayState::Awake, true) => self.switch(DisplayState::Asleep),
            (DisplayState::Asleep, false) => self.switch(DisplayState::Awake),
            _ => (),
        }
        self.state
    }

    fn is_motion_confirmed(&mut self, now: Instant) -> bool {
        let is_motion_detected = self.sensor.is_motion_detected().unwrap_or_else(|error| {
            /* Treated as motion, so that a broken sensor leaves the display on instead of off */
            log::error!("Cannot read the motion sensor: {error}");
            true
        });
        if !is_motion_detected {
            self.motion_since = None;
            return false;
        }
        let motion_since = *self.motion_since.get_or_insert(now);
        now - motion_since >= DEBOUNCE_DURATION
    }

    fn switch(&mut self, state: DisplayState) {
        let power_state = match state {
            DisplayState::Awake => PowerState::On,
            DisplayState::Asleep => PowerState::Standby,
        };
        log::info!("Switching the display to {power_state:?}");
        if let Err(error) = self.display_power.set(power_state) {
            log::error!("Cannot switch the display: {error}");
        }
        /* Recorded even when switching failed, so that a command which does not work is not run
         * again on every iteration of the slideshow loop */
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mock_instant::thread_local::MockClock;
    use mockall::{Sequence, predicate::eq};

    const TIMEOUT: Duration = Duration::from_secs(300);

    #[test]
    fn when_no_motion_is_detected_for_the_timeout_then_the_display_is_switched_to_standby() {
        let mut display_power = MockDisplayPower::new();
        display_power
            .expect_set()
            .with(eq(PowerState::Standby))
            .times(1)
            .returning(|_| Ok(()));
        expect_restore_on_drop(&mut display_power);
        let mut standby = new_standby(motion_sensor(|| Ok(false)), display_power);

        assert_eq!(
            advance_and_update(&mut standby, TIMEOUT - POLL_INTERVAL),
            DisplayState::Awake,
            "the display must stay on until the timeout has passed"
        );
        assert_eq!(
            advance_and_update(&mut standby, POLL_INTERVAL),
            DisplayState::Asleep
        );
    }

    #[test]
    fn when_motion_is_detected_during_standby_then_the_display_is_switched_on() {
        let mut sequence = Sequence::new();
        let mut display_power = MockDisplayPower::new();
        display_power
            .expect_set()
            .with(eq(PowerState::Standby))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(()));
        display_power
            .expect_set()
            .with(eq(PowerState::On))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(()));
        /* Motion only after the display went to standby */
        let sensor = motion_sensor(|| Ok(MockClock::time() > TIMEOUT));
        let mut standby = new_standby(sensor, display_power);

        assert_eq!(
            advance_and_update(&mut standby, TIMEOUT),
            DisplayState::Asleep
        );
        assert_eq!(
            /* Comfortably more than the debounce duration, which needs several polls */
            poll_for(&mut standby, DEBOUNCE_DURATION * 2),
            DisplayState::Awake
        );
    }

    #[test]
    fn when_motion_is_shorter_than_the_debounce_duration_then_it_is_ignored() {
        let mut display_power = MockDisplayPower::new();
        display_power
            .expect_set()
            .with(eq(PowerState::Standby))
            .times(1)
            .returning(|_| Ok(()));
        expect_restore_on_drop(&mut display_power);
        /* A single spike on the sensor's output, in the middle of an otherwise quiet period */
        let sensor = motion_sensor(|| Ok(MockClock::time() == POLL_INTERVAL * 2));
        let mut standby = new_standby(sensor, display_power);

        for _ in 0..(TIMEOUT.as_millis() / POLL_INTERVAL.as_millis()) {
            advance_and_update(&mut standby, POLL_INTERVAL);
        }

        assert_eq!(standby.update(), DisplayState::Asleep);
    }

    #[test]
    fn the_sensor_is_not_read_more_often_than_the_poll_interval() {
        let mut sensor = MockMotionSensor::new();
        sensor
            .expect_is_motion_detected()
            .times(1)
            .returning(|| Ok(true));
        let mut standby = new_standby(Box::new(sensor), MockDisplayPower::new());

        /* The slideshow loop runs far more often than the sensor needs to be read */
        for _ in 0..10 {
            advance_and_update(&mut standby, POLL_INTERVAL / 10);
        }
    }

    #[test]
    fn when_the_sensor_cannot_be_read_then_the_display_stays_on() {
        let mut display_power = MockDisplayPower::new();
        display_power.expect_set().never();
        let sensor = motion_sensor(|| anyhow::bail!("GPIO is unavailable"));
        let mut standby = new_standby(sensor, display_power);

        assert_eq!(poll_for(&mut standby, TIMEOUT * 2), DisplayState::Awake);
    }

    #[test]
    fn when_standby_is_disabled_then_the_display_is_never_switched() {
        let mut standby = Standby::disabled();

        assert_eq!(
            advance_and_update(&mut standby, TIMEOUT * 2),
            DisplayState::Awake
        );
    }

    #[test]
    fn when_dropped_during_standby_then_the_display_is_switched_back_on() {
        let mut sequence = Sequence::new();
        let mut display_power = MockDisplayPower::new();
        display_power
            .expect_set()
            .with(eq(PowerState::Standby))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(()));
        display_power
            .expect_set()
            .with(eq(PowerState::On))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(()));
        let mut standby = new_standby(motion_sensor(|| Ok(false)), display_power);

        assert_eq!(
            advance_and_update(&mut standby, TIMEOUT),
            DisplayState::Asleep
        );

        /* Expectations are verified when the mock is dropped together with the standby */
        drop(standby);
    }

    #[test]
    fn when_the_display_cannot_be_switched_then_it_is_not_attempted_again() {
        let mut display_power = MockDisplayPower::new();
        display_power
            .expect_set()
            .with(eq(PowerState::Standby))
            .times(1)
            .returning(|_| anyhow::bail!("vcgencmd not found"));
        /* Switching back on when dropped is attempted regardless */
        display_power
            .expect_set()
            .with(eq(PowerState::On))
            .returning(|_| Ok(()));
        let mut standby = new_standby(motion_sensor(|| Ok(false)), display_power);

        for _ in 0..5 {
            assert_eq!(
                advance_and_update(&mut standby, TIMEOUT),
                DisplayState::Asleep
            );
        }
    }

    /// Standby switches the display back on when it is dropped, which tests ending in standby have
    /// to allow for
    fn expect_restore_on_drop(display_power: &mut MockDisplayPower) {
        display_power
            .expect_set()
            .with(eq(PowerState::On))
            .times(1)
            .returning(|_| Ok(()));
    }

    fn new_standby(sensor: Box<dyn MotionSensor>, display_power: MockDisplayPower) -> Standby {
        MockClock::set_time(Duration::ZERO);
        Standby::new(sensor, Box::new(display_power), TIMEOUT)
    }

    fn motion_sensor(
        is_motion_detected: impl Fn() -> Result<bool> + Send + 'static,
    ) -> Box<dyn MotionSensor> {
        let mut sensor = MockMotionSensor::new();
        sensor.expect_is_motion_detected().returning(move || {
            let is_motion_detected = &is_motion_detected;
            is_motion_detected()
        });
        Box::new(sensor)
    }

    /// Jumps ahead and updates once, for tests that do not depend on how often the sensor is polled
    fn advance_and_update(standby: &mut Standby, duration: Duration) -> DisplayState {
        MockClock::advance(duration);
        standby.update()
    }

    /// Updates repeatedly over the given duration, the way the slideshow loop does, so that the
    /// sensor is actually polled more than once
    fn poll_for(standby: &mut Standby, duration: Duration) -> DisplayState {
        let mut state = standby.update();
        for _ in 0..(duration.as_millis() / POLL_INTERVAL.as_millis()) {
            state = advance_and_update(standby, POLL_INTERVAL);
        }
        state
    }
}
