use std::process::Command;

use anyhow::{Result, bail};

use crate::standby::{DisplayPower, PowerState};

/// Replaced with the requested state in the command template
const STATE_PLACEHOLDER: &str = "{state}";

/// Switches the display by running an external command, since how to do that depends entirely on the
/// hardware and the graphics stack in use
pub struct CommandDisplayPower {
    program: String,
    arguments: Vec<String>,
}

impl CommandDisplayPower {
    /// The template is validated here rather than when the display is first switched, so that a
    /// typo is reported before the slideshow starts
    pub fn new(template: &str) -> Result<Self> {
        if !template.contains(STATE_PLACEHOLDER) {
            bail!("The display power command must contain the {STATE_PLACEHOLDER} placeholder")
        }
        /* Split on whitespace and executed directly, rather than passed to a shell: it avoids
         * turning a mistyped command into arbitrary code, and works the same on any platform */
        let mut words = template.split_whitespace().map(str::to_string);
        let Some(program) = words.next() else {
            bail!("The display power command must not be empty")
        };
        Ok(Self {
            program,
            arguments: words.collect(),
        })
    }

    fn arguments(&self, state: PowerState) -> Vec<String> {
        let value = match state {
            PowerState::On => "1",
            PowerState::Standby => "0",
        };
        self.arguments
            .iter()
            .map(|argument| argument.replace(STATE_PLACEHOLDER, value))
            .collect()
    }
}

impl DisplayPower for CommandDisplayPower {
    fn set(&self, state: PowerState) -> Result<()> {
        /* status() rather than output(), so that whatever the command complains about ends up in
         * this application's output */
        let status = Command::new(&self.program)
            .args(self.arguments(state))
            .status()?;
        if !status.success() {
            bail!("{} failed with {status}", self.program)
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_splits_the_template_into_a_program_and_its_arguments() {
        let display_power = CommandDisplayPower::new("vcgencmd display_power {state}").unwrap();

        assert_eq!(display_power.program, "vcgencmd");
        assert_eq!(display_power.arguments, vec!["display_power", "{state}"]);
    }

    #[test]
    fn arguments_substitute_the_requested_state() {
        let display_power = CommandDisplayPower::new("vcgencmd display_power {state} 2").unwrap();

        assert_eq!(
            display_power.arguments(PowerState::On),
            vec!["display_power", "1", "2"]
        );
        assert_eq!(
            display_power.arguments(PowerState::Standby),
            vec!["display_power", "0", "2"]
        );
    }

    #[test]
    fn new_rejects_a_template_that_cannot_switch_anything() {
        for template in ["", "   ", "vcgencmd display_power"] {
            assert!(
                CommandDisplayPower::new(template).is_err(),
                "template was {template:?}"
            );
        }
    }

    #[test]
    fn set_reports_a_command_that_cannot_be_run() {
        let display_power =
            CommandDisplayPower::new("definitely-not-an-executable-6f21ba {state}").unwrap();

        assert!(display_power.set(PowerState::Standby).is_err());
    }
}
