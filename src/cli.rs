//! CLI options

use std::{path::PathBuf, time::Duration};

pub use clap::Parser;
use clap::{builder::TypedValueParser as _, ValueEnum};

use crate::error::ErrorToString;

/// FTP-hosted Photos fullscreen slideshow
///
/// Project website: <https://github.com/Finnfj/ftp-photo-frame>
/// Forked from: <https://github.com/caleb9/syno-photo-frame>
#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Cli {
    /// IP address of the FTP server
    #[arg(long)]
    pub server: String,
    
    /// Folder path on the FTP server
    #[arg(long)]
    pub folder: String,

    /// User for FTP access
    #[arg(short = 'u', long = "user")]
    pub user: Option<String>,

    /// Password for FTP access
    #[arg(short = 'p', long = "password")]
    pub password: Option<String>,

    /// Photo change interval in seconds
    ///
    /// Must be greater or equal to 5. Note that it is only guaranteed that the display time will
    /// not be shorter than specified value, but it may exceed this value if next photo fetching and
    /// processing takes longer time
    #[arg(
        short = 'i',
        long = "interval",
        default_value = "30",
        value_parser = try_parse_duration)]
    pub photo_change_interval: Duration,

    /// Slideshow ordering
    #[arg(short = 'o', long, value_enum, default_value_t = Order::ByDate)]
    pub order: Order,

    /// Start at randomly selected photo, then continue according to --order
    #[arg(long, default_value_t = false)]
    pub random_start: bool,

    /// Transition effect
    #[arg(short = 't', long, value_enum, default_value_t = Transition::Crossfade)]
    pub transition: Transition,

    /// Rotate display to match screen orientation
    #[arg(
        long = "rotate",
        default_value = "0",
        value_parser =
            clap::builder::PossibleValuesParser::new(ROTATIONS).map(Rotation::from)
    )]
    pub rotation: Rotation,
    
    /// Use motion sensor to sleep when no motion is detected
    #[arg(long, default_value_t = false)]
    pub motionsensor: bool,

    /// Path to a JPEG file to display during startup, replacing the default splash-screen
    #[arg(long)]
    pub splash: Option<PathBuf>,
}

fn try_parse_duration(arg: &str) -> Result<Duration, String> {
    let seconds = arg.parse().map_err_to_string()?;
    if seconds < 5 {
        Err("must not be less than 5".to_string())
    } else {
        Ok(Duration::from_secs(seconds))
    }
}

/// Slideshow ordering
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Order {
    /// by photo shooting date
    ByDate,
    /// by photo file name
    ByName,
    /// randomly
    Random,
}

/// Transition to next photo effect
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Transition {
    /// Crossfade (or cross dissolve)
    Crossfade,
    /// Fade out to black and in to next photo
    FadeToBlack,
    /// Disable transition effect
    None,
}

const ROTATIONS: [&str; 4] = ["0", "90", "180", "270"];

/// Screen rotation in degrees
#[derive(Debug, Copy, Clone)]
pub enum Rotation {
    /// 0°
    D0,
    /// 90°
    D90,
    /// 180°
    D180,
    /// 270°
    D270,
}

impl From<String> for Rotation {
    fn from(value: String) -> Self {
        match value.as_str() {
            "0" => Rotation::D0,
            "90" => Rotation::D90,
            "180" => Rotation::D180,
            "270" => Rotation::D270,
            _ => panic!(),
        }
    }
}

/// Requested size of source photo to fetch from Server
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum SourceSize {
    /// small (360x240)
    S,
    /// medium (481x320)
    M,
    /// large (1922x1280)
    L,
}

#[test]
fn verify_cli() {
    use clap::CommandFactory;
    Cli::command().debug_assert()
}
