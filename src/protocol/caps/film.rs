//! Film formats and their frame heights
//!
//! The frame height (film format) is the one piece of information the scanner
//! cannot derive from a thumbnail. It is not a property of the scanner or the
//! holder, since the same 6×9 holder accepts 6×6, 6×7, 6×8 and 6×9 film, so the
//! holder ID narrows the choices but does not fix the answer.
//!
//! The holder ID does fix it for the FH-869GR, which has a mask that selects
//! the format physically. For everything else the operator supplies it.

use crate::error::Error;
use std::{fmt, str::FromStr};

/// A film format, keyed by its frame height in millimeters
///
/// The names are nominal and the gates are not: 6×4.5 exposes 41.5 mm of film
/// and 6×8 exposes 76. These carry the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilmFormat {
    /// APS (IX240), 30.2 × 16.7 mm recorded. C and P are crops of that same
    /// recorded frame, so all three scan at its length
    IX240,
    /// 135 film (35mm), 24 × 36 mm
    F135,
    /// 135 film shot half frame, 18 × 24 mm: two frames in the space of one
    F135Half,
    /// 16mm film, 16 × 20 mm (FH-816)
    F16,
    /// 120 film, 6 × 4.5 cm, which exposes 41.5 mm
    F645,
    /// 120 film, 6 × 6 cm
    F66,
    /// 120 film, 6 × 7 cm, which exposes 69.5 mm
    F67,
    /// 120 film, 6 × 8 cm, which exposes 76 mm
    F68,
    /// 120 film, 6 × 9 cm
    F69,
    /// Operator-supplied height in millimeters
    Custom(u32),
}

impl FilmFormat {
    /// Frame height along the feed, in dots of a 4000 dpi axis
    ///
    /// Address units, which is what the unit consumes and what Nikon Scan's own
    /// numbers are in. A millimeter cannot express them: 6x6's 8964 dots are
    /// 56.92 mm, and 56.9 mm reaches only 8961.
    ///
    /// 6x7 and 6x8 are the round millimeter, not checked against Nikon Scan.
    /// 6x9 can be the longest frame the holder takes and not the frame
    const fn height_dots_4000(self) -> u32 {
        match self {
            Self::IX240 => 4756,
            Self::F135 => 5670,
            Self::F135Half => 2835,
            Self::F16 => 3150,
            Self::F645 => 6696,
            Self::F66 => 8964,
            Self::F67 => 10945,
            Self::F68 => 11969,
            Self::F69 => 13176,
            // Whole millimeters, so 4000 dots to 25.4 of them
            Self::Custom(mm) => mm * 40000 / 254,
        }
    }

    /// Frame height in scanner address units (dots at optical DPI)
    pub fn height_dots(self, dpi: u16) -> u32 {
        let num = u64::from(self.height_dots_4000()) * u64::from(dpi);
        ((num + 2000) / 4000) as u32
    }

    /// The format a holder ID implies, where the holder fixes it
    ///
    /// Returns `None` where the holder accepts more than one format, or where
    /// the holder is unknown. The FH-869GR is the only holder that physically
    /// selects the format via its mask
    pub fn from_holder(holder_id: u8) -> Option<Self> {
        match holder_id {
            0x12 => Some(Self::F16),  // FH-816
            0x19 => Some(Self::F645), // FH-869GR 6×4.5
            0x1A => Some(Self::F66),  // FH-869GR 6×6
            0x1B => Some(Self::F67),  // FH-869GR 6×7
            0x1C => Some(Self::F68),  // FH-869GR 6×8
            0x1D => Some(Self::F69),  // FH-869GR 6×9
            _ => None,
        }
    }

    /// The formats a holder ID accepts, where it accepts more than one
    ///
    /// Returns `None` where the holder fixes the format (see [`Self::from_holder`])
    /// or is unknown. Used to offer the operator a choice
    pub fn choices_for_holder(holder_id: u8) -> Option<&'static [Self]> {
        match holder_id {
            0x14 => Some(&[Self::F66, Self::F67, Self::F69]), // FH-835M
            0x15 => Some(&[Self::F66, Self::F67, Self::F69]), // FH-835S
            0x16 => Some(&[Self::F66, Self::F67, Self::F69]), // FH-869M
            0x17 => Some(&[Self::F66, Self::F67, Self::F69]), // FH-869S
            0x18 => Some(&[Self::F66, Self::F67, Self::F69]), // FH-869G
            _ => None,
        }
    }

    /// Same as holder ID, but for LS-4x and LS-5x adapters
    pub fn from_adapter(adapter_id: u8) -> Option<Self> {
        match adapter_id {
            0x31 => Some(Self::F135),  // SA-21/SA-30
            0x35 => Some(Self::IX240), // IA-20
            0x32 => Some(Self::F135),  // SF-210
            _ => None,
        }
    }

    /// The format for whatever is loaded: `explicit` wins, and otherwise the
    /// holder or adapter ID fixes or narrows it
    ///
    /// `caps.identity.is_mf_scanner()` is what picks holder ID over adapter ID
    pub fn resolve(explicit: Option<Self>, caps: &super::Capabilities) -> Result<Self, Error> {
        if let Some(format) = explicit {
            return Ok(format);
        }

        let uses_adapter =
            !caps.identity.is_mf_scanner() && caps.address.adapter_id.is_some_and(|id| id > 0);
        let id = if uses_adapter {
            caps.address.connected_adapter
        } else {
            caps.address.holder_id
        }
        .ok_or_else(|| Error::Unsupported {
            op: "film format",
            reason: "no holder loaded; supply a format".into(),
        })?;

        Self::from_holder(id)
            .or_else(|| Self::from_adapter(id))
            .ok_or_else(|| {
                let choices = Self::choices_for_holder(id)
                    .map(|c| {
                        format!(
                            " (try: {})",
                            c.iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .unwrap_or_default();
                Error::Unsupported {
                    op: "film format",
                    reason: format!("this holder does not fix it{choices}"),
                }
            })
    }
}

impl FromStr for FilmFormat {
    type Err = String;

    /// The named ones are what the holders take; anything else is a height,
    /// which is what a format nobody named still needs
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "IX240" | "aps" | "APS" => Self::IX240,
            "135" => Self::F135,
            "half" | "135half" => Self::F135Half,
            "16" => Self::F16,
            "645" => Self::F645,
            "66" => Self::F66,
            "67" => Self::F67,
            "68" => Self::F68,
            "69" => Self::F69,
            mm => Self::Custom(
                mm.parse()
                    .map_err(|_| format!("'{mm}' is neither a film format nor a height in mm"))?,
            ),
        })
    }
}

/// What [`FromStr`] would take back for this format, for saying what is on offer
impl fmt::Display for FilmFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IX240 => write!(f, "IX240"),
            Self::F135 => write!(f, "135"),
            Self::F135Half => write!(f, "half"),
            Self::F16 => write!(f, "16"),
            Self::F645 => write!(f, "645"),
            Self::F66 => write!(f, "66"),
            Self::F67 => write!(f, "67"),
            Self::F68 => write!(f, "68"),
            Self::F69 => write!(f, "69"),
            Self::Custom(mm) => write!(f, "{mm}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are nominal and the gates are not. Getting these wrong leaves
    /// a frame with a strip of the next one along its edge
    #[test]
    fn a_format_is_its_gate_rather_than_its_name() {
        assert_eq!(FilmFormat::F645.height_dots_4000(), 6696);
        assert_eq!(FilmFormat::F67.height_dots_4000(), 10945);
        assert_eq!(FilmFormat::F68.height_dots_4000(), 11969);
        // Half frame is two frames in the space one full frame takes
        assert_eq!(
            FilmFormat::F135Half.height_dots_4000() * 2,
            FilmFormat::F135.height_dots_4000()
        );
    }

    #[test]
    fn dots_at_4000_dpi() {
        // Nikon Scan's own numbers, exactly
        assert_eq!(FilmFormat::F66.height_dots(4000), 8964);
        assert_eq!(FilmFormat::F69.height_dots(4000), 13176);
        assert_eq!(FilmFormat::F645.height_dots(4000), 6696);
        // And what a thumbnail of one is, which is what the frames are found in
        assert_eq!(FilmFormat::F66.height_dots(83), 186);
        assert_eq!(FilmFormat::F135.height_dots(97), 137);
    }

    #[test]
    fn gr_holder_fixes_format() {
        assert_eq!(FilmFormat::from_holder(0x1A), Some(FilmFormat::F66));
        assert_eq!(FilmFormat::from_holder(0x1D), Some(FilmFormat::F69));
        assert_eq!(FilmFormat::from_holder(0x17), None);
    }

    #[test]
    fn strip_holder_offers_choices() {
        let choices = FilmFormat::choices_for_holder(0x17).unwrap();
        assert!(choices.contains(&FilmFormat::F66));
        assert!(choices.contains(&FilmFormat::F69));
        assert!(FilmFormat::choices_for_holder(0x1A).is_none());
    }
}
