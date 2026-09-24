//! Nikon's own scanner profiles, one per model per film type
//!
//! Nikon Scan characterized each model and shipped an input profile for every
//! film type it offers. The device primaries are identical across the film types
//! of one model: what changes is the table from device values to the connection
//! space, since the transform runs through the film's dyes and those differ per
//! stock. Negative and Kodachrome need their own for that reason. Positive and
//! Kodachrome were measured per model; the negative table is shared by the
//! LS-9000, LS-5000 and LS-50, while the LS-8000 and LS-4000 have their own.
//!
//! Nikon also ships an `_R` profile, the same bytes for every model and older
//! than the rest. It is not a film type, so it is not here: the manual's
//! "Scanner RGB" color space is what it reads like.
//!
//! Converted from what the installer ships, by `scripts/profiles.py`:
//!
//! - The class was `nkpf`, a Nikon private one that littlecms refuses to open.
//!   It is `scnr` here, which is what the file otherwise already was.
//! - They expected the gamma 2.2 values Nikon Scan's driver hands its CMS. The
//!   encode is composed into the profile's own curves so these take the linear
//!   samples a pass produces.

use crate::protocol::{caps::identity::Identity, model::Model};

/// Which film a profile characterizes
///
/// The scanner is the same; the dyes it is looking through are not
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Film {
    /// Slide film, and what Nikon Scan uses when nothing else is chosen
    #[default]
    Positive,
    /// Color negative, mask and all
    Negative,
    /// Kodachrome, whose dyes are enough unlike E6 to need their own
    Kodachrome,
    /// Black and white negative. A matrix rather than a table
    MonochromeNegative,
}

impl std::str::FromStr for Film {
    type Err = String;

    /// Parse "positive", "slide", "negative", "kodachrome" or "mono". Case
    /// does not matter
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name.to_ascii_lowercase().as_str() {
            "positive" | "slide" => Ok(Self::Positive),
            "negative" => Ok(Self::Negative),
            "kodachrome" => Ok(Self::Kodachrome),
            "mono" | "monochrome" | "monochromenegative" => Ok(Self::MonochromeNegative),
            other => Err(format!("unknown film type {other:?}")),
        }
    }
}

/// A profile is not quite per model: Nikon shipped one covering the LS-4000 and
/// the LS-40, and the LS-50's profiles hold the same measurements as the
/// LS-5000's, differing only in the description they carry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Ls9000,
    Ls8000,
    Ls5000,
    Ls4000,
}

impl Family {
    fn of(model: Model) -> Self {
        match model {
            Model::Ls9000 => Self::Ls9000,
            Model::Ls8000 => Self::Ls8000,
            Model::Ls5000 | Model::Ls50 => Self::Ls5000,
            Model::Ls4000 | Model::Ls40 => Self::Ls4000,
        }
    }
}

/// What the embedded profiles are and whose they are
///
/// The bytes are compiled in, so this crate's own license is not the whole
/// story for anything built from it. Anything redistributing a binary should
/// put this where its user can read it
pub const NOTICE: &str = "\
The ICC profiles this program tags its scans with are derived from the ICM
profiles in the Nikon Scan 4 installer, which carry the notice \"Nikon Inc. &
Nikon Corporation 2003\". That notice is retained in each profile.

They are not covered by this program's license, and no license to them is
granted here. Their tables have been altered (the profile class, and the gamma
2.2 encode composed into the curves), so they are not what Nikon shipped and
should not be read as Nikon's characterization.";

/// Bake the color profiles into our binary
macro_rules! profile {
    ($name:literal) => {
        Some(include_bytes!(concat!("../../profiles/", $name, ".icc")).as_slice())
    };
}

/// Nikon's profile for this unit and film, where they made one
///
/// `None` for a model Nikon Scan never shipped a profile for, and for the two
/// families that have no monochrome negative profile
pub fn nikon(identity: &Identity, film: Film) -> Option<&'static [u8]> {
    match (Family::of(identity.model()?), film) {
        (Family::Ls9000, Film::Positive) => profile!("NKLS9000_P"),
        (Family::Ls9000, Film::Negative) => profile!("NKLS9000_N"),
        (Family::Ls9000, Film::Kodachrome) => profile!("NKLS9000_K"),
        (Family::Ls9000, Film::MonochromeNegative) => profile!("NKLS9000_MN"),

        (Family::Ls8000, Film::Positive) => profile!("NKLS8000_P"),
        (Family::Ls8000, Film::Negative) => profile!("NKLS8000_N"),
        (Family::Ls8000, Film::Kodachrome) => profile!("NKLS8000_K"),
        (Family::Ls8000, Film::MonochromeNegative) => None,

        (Family::Ls5000, Film::Positive) => profile!("NKLS5000_P"),
        (Family::Ls5000, Film::Negative) => profile!("NKLS5000_N"),
        (Family::Ls5000, Film::Kodachrome) => profile!("NKLS5000_K"),
        (Family::Ls5000, Film::MonochromeNegative) => profile!("NKLS5000_MN"),

        (Family::Ls4000, Film::Positive) => profile!("NKLS4000LS40_P"),
        (Family::Ls4000, Film::Negative) => profile!("NKLS4000LS40_N"),
        (Family::Ls4000, Film::Kodachrome) => profile!("NKLS4000LS40_K"),
        (Family::Ls4000, Film::MonochromeNegative) => None,
    }
}
