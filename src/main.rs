use std::{sync::Arc, time::Duration};

use anyhow::{Result, bail};
use log::LevelFilter;
use simple_logger::SimpleLogger;

use syno_photo_frame::{
    self, LoginError, QuitEvent, RandomImpl,
    cli::{Cli, Parser},
    env::EnvImpl,
    error::AnyhowErrorMapper,
    http::ClientBuilder,
    logging::LoggingClientDecorator,
    sdl::{self, SdlWrapper},
    standby::{self, CommandDisplayPower, Standby},
};

fn main() -> Result<()> {
    SimpleLogger::new()
        .with_level(LevelFilter::Info) /* Default */
        .env() /* Allow overwriting default level with RUST_LOG env var */
        .init()?;

    if let Err(error) = init_and_run() {
        if error.is::<QuitEvent>() {
            return Ok(());
        }
        log::error!("{error}");
        if let Some(LoginError(_)) = error.downcast_ref::<LoginError>() {
            bail!(
                "Login failed. Make sure the share link is pointing to a *publicly shared album*. \
                If the album's password link protection is enabled, use the --password option with \
                a valid password.",
            )
        }
        bail!(error)
    }
    Ok(())
}

/// Setup "real" dependencies and run
fn init_and_run() -> Result<()> {
    let cli = Cli::parse();

    /* Motion sensor. Set up before SDL, so that a missing sensor or an unusable display power
     * command is reported before the screen is taken over by the slideshow. */
    let standby = match cli.motion_sensor_gpio {
        None => Standby::disabled(),
        Some(gpio_pin) => Standby::new(
            standby::new_motion_sensor(gpio_pin)?,
            Box::new(CommandDisplayPower::new(&cli.display_power_command)?),
            cli.motion_sensor_timeout,
        ),
    };

    /* HTTP client */
    let cookie_store = Arc::new(reqwest::cookie::Jar::default());
    let http_client = ClientBuilder::new()
        .cookie_provider(Arc::clone(&cookie_store))
        .timeout(Duration::from_secs(cli.timeout_seconds as u64))
        .build()?;

    /* SDL */
    let sdl = sdl::init()?;
    let video = sdl.video().map_err_to_anyhow()?;
    let display_size = sdl::display_size(&video)?;
    let canvas = sdl::create_canvas(&video, display_size)?;
    let texture_creator = canvas.texture_creator();

    let events = sdl.event_pump().map_err_to_anyhow()?;

    let ttf = sdl::init_ttf()?;

    let mut sdl = SdlWrapper::new(canvas, &texture_creator, events, &ttf)?;

    syno_photo_frame::run(
        &cli,
        (
            &LoggingClientDecorator::new(http_client).with_level(log::Level::Trace),
            cookie_store.as_ref(),
        ),
        &mut sdl,
        RandomImpl,
        env!("CARGO_PKG_VERSION"),
        &EnvImpl,
        standby,
    )
}
