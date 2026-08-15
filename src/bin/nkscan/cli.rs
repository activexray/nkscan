use clap::{Parser, Subcommand};
use nkscan::{protocol::caps::film::FilmFormat, scan::profile};
use profile::Film;
use std::{path::PathBuf, sync::LazyLock};
use tracing_subscriber::filter::LevelFilter;

/// The version, plus the notices for what is compiled in alongside our own code
///
/// `-V` stays the bare version; this is what `--version` gives
static LONG_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{}\n\n{}", env!("CARGO_PKG_VERSION"), profile::NOTICE));

#[derive(Parser)]
#[command(version, about, long_version = LONG_VERSION.as_str())]
/// Scan film on a Nikon Coolscan
pub struct Cli {
    #[command(subcommand)]
    pub action: Action,

    /// Log verbosity: trace, debug, info, warn, error, or off.
    #[arg(long, global = true, default_value_t = LevelFilter::INFO, value_parser = parse_log_level)]
    pub log: LevelFilter,
}

#[derive(Subcommand)]
pub enum Action {
    /// List available scanners
    List,
    /// Perform a scan. Defaults to batch scanning with sensible defaults.
    Scan(Scan),
    /// Dump a scanner's INQUIRY pages. Reads only; nothing moves.
    Dump(Dump),
    /// Eject the loaded film or holder
    Eject(Eject),
}

/// Eject the loaded film or holder
#[derive(clap::Args)]
pub struct Eject {
    /// The scanner to eject. Optional, will default to the first found.
    pub device: Option<String>,
}

/// Which scanner to ask about itself
#[derive(clap::Args)]
pub struct Dump {
    /// The scanner to read. Optional, will default to the first found.
    pub device: Option<String>,
}

/// What to scan and how
#[derive(clap::Args)]
pub struct Scan {
    /// The scanner to connect to. Optional, will default to the first found.
    pub device: Option<String>,

    /// Where to write, as a path prefix. Each frame becomes <basename>_<n>.tiff, and its infrared mask <basename>_<n>_IR.tiff
    #[arg(long, default_value = "scan")]
    pub basename: PathBuf,

    /// Autoexpose per channel. Better dynamic range, but no longer "calibrated".
    #[arg(long)]
    pub unlock_wb: bool,

    /// Autoexpose the first frame and reuse that exposure across all frames.
    #[arg(long)]
    pub lock_ae: bool,

    /// Resolution. Defaults to scanner maximum.
    #[arg(long)]
    pub dpi: Option<u16>,

    /// Number of samples. Defaults to 1.
    #[arg(long, default_value_t = 1)]
    pub samples: u8,

    /// Singleline CCD mode. Only supported on multiline CCD scanners.
    #[arg(long)]
    pub superfine: bool,

    /// Which frame(s) to scan, comma separated. Defaults to all detected.
    /// Naming any stops after one holder rather than batching.
    #[arg(long, value_delimiter = ',')]
    pub frames: Vec<usize>,

    /// Include the IR pass
    #[arg(long)]
    pub ir: bool,

    /// Remove dust and scratches using the infrared channel.
    #[arg(long)]
    pub clean: bool,

    /// Don't eject at the end of the strip
    #[arg(long)]
    pub no_eject: bool,

    /// Film format. One of: 135, 16, 645, 66, 67, 68, 69, or a custom frame length in mm. Defaults to what the holder reports (if any).
    #[arg(long, value_parser = parse_format)]
    pub format: Option<FilmFormat>,

    /// Film type, which picks the color profile the scans are tagged with
    #[arg(long, value_enum, default_value_t = FilmType::Negative)]
    pub film: FilmType,
}

/// The film types Nikon profiled, as flag values
#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum FilmType {
    /// Slide film
    Positive,
    /// Color negative
    Negative,
    /// Kodachrome, whose dyes need their own profile
    Kodachrome,
    /// Black and white negative
    Mono,
}

impl From<FilmType> for Film {
    fn from(f: FilmType) -> Self {
        match f {
            FilmType::Positive => Film::Positive,
            FilmType::Negative => Film::Negative,
            FilmType::Kodachrome => Film::Kodachrome,
            FilmType::Mono => Film::MonochromeNegative,
        }
    }
}

/// A log level flag, by name
pub fn parse_log_level(flag: &str) -> Result<LevelFilter, String> {
    flag.parse()
        .map_err(|_| format!("'{flag}' is not a valid log level (expected one of: trace, debug, info, warn, error, off)"))
}

/// A film format flag, by name or as a frame height in millimetres
///
/// The named ones are what the holders take; anything else is a height, which
/// is what a format nobody named still needs
pub fn parse_format(flag: &str) -> Result<FilmFormat, String> {
    Ok(match flag {
        "IX240" => FilmFormat::IX240,
        "135" => FilmFormat::F135,
        "16" => FilmFormat::F16,
        "645" => FilmFormat::F645,
        "66" => FilmFormat::F66,
        "67" => FilmFormat::F67,
        "68" => FilmFormat::F68,
        "69" => FilmFormat::F69,
        mm => FilmFormat::Custom(
            mm.parse()
                .map_err(|_| format!("'{mm}' is neither a film format nor a height in mm"))?,
        ),
    })
}

/// What `parse_format` would take for this format, for saying what is on offer
pub fn format_name(format: &FilmFormat) -> String {
    match format {
        FilmFormat::IX240 => "24".into(),
        FilmFormat::F135 => "135".into(),
        FilmFormat::F16 => "16".into(),
        FilmFormat::F645 => "645".into(),
        FilmFormat::F66 => "66".into(),
        FilmFormat::F67 => "67".into(),
        FilmFormat::F68 => "68".into(),
        FilmFormat::F69 => "69".into(),
        FilmFormat::Custom(mm) => mm.to_string(),
    }
}
