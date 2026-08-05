use anyhow::{Context, Result};
use rppal::gpio::{Gpio, InputPin};

use crate::standby::MotionSensor;

/// A motion sensor, such as the widespread HC-SR501, wired to a GPIO pin which it pulls high while
/// it detects movement
pub struct GpioMotionSensor {
    pin: InputPin,
}

impl GpioMotionSensor {
    pub fn new(pin_number: u8) -> Result<Self> {
        let gpio = Gpio::new().context(
            "Cannot access the GPIO peripheral. Make sure the user is a member of the gpio group",
        )?;
        let pin = gpio
            .get(pin_number)
            .with_context(|| format!("Cannot use GPIO pin {pin_number}"))?
            /* Pulled down, so that a disconnected pin reads as no motion rather than floating */
            .into_input_pulldown();
        Ok(Self { pin })
    }
}

impl MotionSensor for GpioMotionSensor {
    fn is_motion_detected(&self) -> Result<bool> {
        Ok(self.pin.is_high())
    }
}
