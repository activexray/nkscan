//! Sense data, read as what to do next rather than as an error
//!
//! Neither spec has a sense-code chapter; 1-1-5-2 cites a "table 4-1-1" that
//! does not exist. The codes are spread across the per-command response tables
//! in section 2, and [`from_sense`] is assembled from those.
//!
//! CHECK CONDITION is the device's only out-of-band channel and carries
//! progress, rounding and host-cooperation as well as faults, hence [`Outcome`]
//! rather than a `Result`.

use crate::transport::{Completion, Sense, Status};

/// The device reported a fault of its own
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("mechanical error")]
    Mechanism, // 02h-04h-02h
    #[error("command aborted")]
    Aborted, // 0Bh-4Bh data phase, 0Bh-4Eh overlapped
    #[error("hardware error")]
    Hardware, // 04h
    #[error("medium error")]
    Medium, // 03h
    #[error("unexpected status {0:#04x}")]
    UnexpectedStatus(u8),
    #[error("unrecognized sense")]
    Unrecognized,
    /// Neither spec lists this one. It is SPC's OUT OF FOCUS: some units put it
    /// directly on the command's own completion, and [`from_sense`] catches it
    /// there; others report the generic `02h-04h-02h` and only reveal this
    /// through SEND DIAGNOSTIC afterward, which [`diagnosed`] reads
    #[error("autofocus did not reach focus")]
    OutOfFocus, // 01h-61h-02h
}

/// The device refused a CDB we built, meaning a capability check we missed
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("invalid field in CDB")]
    BadCdbField, // 05h-24h
    #[error("invalid field in parameter list")]
    BadParamField, // 05h-26h
    #[error("parameter list length error")]
    BadLength, // 05h-1Ah
    #[error("invalid operation code")]
    UnknownOpcode, // 05h-20h
    #[error("invalid logical unit")]
    BadLun, // 05h-25h
    #[error("command sequence error")]
    OutOfSequence, // 05h-2Ch-00h
    #[error("invalid combination of windows")]
    BadWindowCombo, // 05h-2Ch-02h
    /// The spec files this under Medium Not Present, but nobody has to touch
    /// the scanner: the caller asked for a frame the film does not have
    #[error("frame beyond the number of frames in the film")]
    FrameOutOfRange, // 02h-3Ah-00h-04h
}

/// Something else has the scanner
/// Firewire-only
#[derive(Debug, thiserror::Error)]
pub enum Contention {
    #[error("the scanner is busy")]
    Busy,
    #[error("another initiator holds the scanner")]
    Reserved,
}

/// The scanner is broken, or our model of it is
/// These may be bugs in our logic
#[derive(Debug, thiserror::Error)]
pub enum Fault {
    /// The device reported a fault of its own
    ///
    /// The sense is absent when the status alone was the fault, which is the
    /// case for an unrecognized SCSI status
    #[error("scanner fault: {0} ({1:?})")]
    Reported(Failure, Option<Sense>),
    /// It refused a CDB we built (maybe we forgot a capability check?)
    #[error("scanner rejected the command: {0} ({1:?})")]
    Rejected(Refusal, Option<Sense>),
    /// A VPD page did not parse
    #[error(transparent)]
    Caps(super::caps::Error),
}

/// Something a person has to go and fix
///
/// Table 2-1-2 in both specs. TSC picks the variant, and SBP-2 has no slot for
/// it, so the LS-9000 can leave us with [`Unreported`](Self::Unreported)
#[derive(Debug, thiserror::Error)]
pub enum Intervention {
    // 0x02-0x04-0x03-xx  Manual Intervention Required
    #[error("The adapter is ejected")]
    AdapterEjected, // 0x00
    #[error("IA-20: The LL door is not completely opened")]
    DoorNotOpen, // 0x01
    #[error("Undefined adapter")]
    UnknownAdapter, // 0x02
    #[error("SA-30: The film of 6 frames or more is loaded when the film gate is closed")]
    FilmGateClosed, // 0x03
    #[error("SA-21/SA-30: The adapter is pulled out a little in the locked status")]
    AdapterUnlocked, // 0x04
    #[error("FH-869GR: the mask is not set")]
    MaskNotSet, // 0x06
    #[error("Undefined holder")]
    UnknownHolder, // 0x07

    // 0x02-0x3A-0x00-xx  Medium Not Present
    #[error("No film or holder is loaded")]
    NoMedium, // 0x00 and 0x01
    #[error("SA-21/SA-30: a nonstandard film was inserted")]
    FilmOutOfStandard, // 0x03
    // 0x04 is "frame beyond the number of frames", which nobody has to touch
    // the scanner to fix, so it is `Refusal::FrameOutOfRange` instead
    #[error("User intervention required with no additional details")]
    Unreported,
}

impl Intervention {
    /// Manual Intervention Required, 02h-04h-03h-xx
    fn manual(tsc: Option<u8>) -> Self {
        match tsc {
            Some(0x00) => Self::AdapterEjected,
            Some(0x01) => Self::DoorNotOpen,
            Some(0x02) => Self::UnknownAdapter,
            Some(0x03) => Self::FilmGateClosed,
            Some(0x04) => Self::AdapterUnlocked,
            Some(0x06) => Self::MaskNotSet,
            Some(0x07) => Self::UnknownHolder,
            _ => Self::Unreported,
        }
    }
}

/// What the mechanism is doing while it is not ready
///
/// 02h-04h-01h-xx, discriminated by TSC, plus the two states that arrive without one
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// An operation activation command is being carried out
    ///
    /// Says nothing about what is moving: an autofocus reports this for its
    /// whole run while the stage travels. Which operations move the feed is a
    /// property of the operation, not of this
    ActivatingOperation, // 01h-00h
    /// The adapter is initializing, or the medium is being loaded or ejected
    MovingMechanism, // 01h-01h and 01h-03h
    /// Correction data is being measured
    MeasuringCorrection, // 01h-02h
    /// Automatic shading or white balance measurement
    AutoShadingOrWb, // 01h-04h
    /// Powered on but not finished initializing
    Initializing, // 02h-04h-00h and 02h-05h-00h
    /// Busy with something internal, which carries on regardless. SCSI status
    /// BUSY, or 0Bh-08h
    TargetBusy,
    /// Not ready, and the reason was not one we recognize
    Unreported,
}

impl Activity {
    /// In Process Of Becoming Ready, 02h-04h-01h-xx
    fn becoming_ready(tsc: Option<u8>) -> Self {
        match tsc {
            Some(0x00) => Self::ActivatingOperation,
            Some(0x01 | 0x03) => Self::MovingMechanism,
            Some(0x02) => Self::MeasuringCorrection,
            Some(0x04) => Self::AutoShadingOrWb,
            _ => Self::Unreported,
        }
    }
}

/// Post-processing the host has to do before the command will proceed
///
/// 09h-80h is a handshake, not an error: read `DataType::Cooperation`, do the
/// work, and re-issue. The ASCQ is also byte 0 of that record, so dispatch on
/// the record and the 4th byte never matters
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coop {
    /// The driver builds the thumbnail
    Thumbnail, // 01h
    /// The driver averages a multiple reading
    Averaging, // 02h
    /// The driver re-registers simultaneously-read CCD lines
    MultiLineRegistration, // 04h
    /// The driver truncates the data
    Truncate, // 06h
    /// The driver creates CCD data
    CcdData, // 07h
    /// An operation type we do not know
    Unknown(u8),
}

impl From<u8> for Coop {
    fn from(ascq: u8) -> Self {
        match ascq {
            0x01 => Self::Thumbnail,
            0x02 => Self::Averaging,
            0x04 => Self::MultiLineRegistration,
            0x06 => Self::Truncate,
            0x07 => Self::CcdData,
            x => Self::Unknown(x),
        }
    }
}

/// Something the unit noted about a command that still completed
///
/// Sense key 01h is RECOVERED ERROR, so none of these mean the command failed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjustment {
    /// Requested resolution was off the pitch ladder; GET WINDOW is
    /// authoritative for what it actually used
    ResolutionRounded, // 01h-37h-00h
    /// A recovered error we have no name for
    Unreported,
}

/// Why cached state is stale
///
/// Every one of these means the command did *not* run: drop what we cached and
/// re-issue. They are told apart only so a log says which happened, since the
/// unit changes underneath us with no other warning
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A holder went in or came out
    MediumChanged, // 28h-00h
    /// Powered on, reset, or told to reset
    Reset, // 29h-xx
    /// Another initiator moved a mode parameter
    ModeParameters, // 2Ah-01h
    /// The VPD pages themselves now read differently, which is exactly the
    /// signal to re-probe. Undocumented by both specs, raised on holder
    /// insertion, and standard SPC
    Capabilities, // 3Fh-03h
    /// Something attached below the unit. Seen once, on a cold start with no
    /// holder in
    Attached, // 3Fh-04h
    /// A unit attention we have no name for
    Unreported,
}

impl Change {
    fn from_code(asc: u8, ascq: u8) -> Self {
        match (asc, ascq) {
            (0x28, 0x00) => Self::MediumChanged,
            (0x29, _) => Self::Reset,
            (0x2A, 0x01) => Self::ModeParameters,
            (0x3F, 0x03) => Self::Capabilities,
            (0x3F, 0x04) => Self::Attached,
            _ => Self::Unreported,
        }
    }
}

/// What a completion means for what happens next
///
/// Constructed only by [`interpret`]. Nothing above this matches on sense keys
#[derive(Debug)]
pub enum Outcome {
    /// Done
    Complete,
    /// Done, but a parameter moved on the way
    CompleteWith(Adjustment),
    /// Not yet; wait and ask again
    Working(Activity),
    /// Do post-processing, then re-issue
    NeedsHost(Coop),
    /// Print this to a person
    NeedsOperator(Intervention),
    /// Throw away what we cached and re-issue
    StateChanged(Change),
    /// Something else holds the unit, and waiting will not clear it
    Contended(Contention),
    /// We built a bad command
    Refused(Refusal),
    /// The machine's problem
    Failed(Failure),
}

/// Classify a SEND DIAGNOSTIC sense, once a generic mechanical failure has
/// sent a caller looking for the real cause
///
/// 2-8: the wrapper on the command itself only ever says `Failure::Mechanism`;
/// this is the one place that reads what SEND DIAGNOSTIC left behind
pub fn diagnosed(sense: &Sense) -> Failure {
    match (sense.key, sense.asc, sense.ascq) {
        (0x01, 0x61, 0x02) => Failure::OutOfFocus,
        _ => Failure::Mechanism,
    }
}

/// Read a completion as what to do next
///
/// Status first: BUSY and RESERVATION CONFLICT arrive with no sense at all,
/// and 2-1-2 notes the latter can pre-empt the documented responses
pub fn interpret(c: &Completion) -> Outcome {
    match c.status {
        Status::Good | Status::CheckCondition => match &c.sense {
            Some(sense) => from_sense(sense),
            None => Outcome::Complete,
        },
        // Transient: the loop retries, and only gives up after its budget
        Status::Busy => Outcome::Working(Activity::TargetBusy),
        Status::ReservationConflict => Outcome::Contended(Contention::Reserved),
        Status::Other(x) => Outcome::Failed(Failure::UnexpectedStatus(x)),
    }
}

/// The tuple table, from the per-command response tables in section 2
///
/// Matches on `(key, asc, ascq)` only. TSC refines a message but never selects
/// a branch, since SBP-2 has nowhere to carry it
fn from_sense(s: &Sense) -> Outcome {
    use Outcome::*;
    match (s.key, s.asc, s.ascq) {
        (0x00, 0x00, 0x00) => Complete,

        // Recovered: it worked, and the unit had a note about it
        (0x01, 0x37, 0x00) => CompleteWith(Adjustment::ResolutionRounded),
        // Out of focus is the one recovered error that is not advisory: the
        // command's whole point was reaching focus, and it did not. Failed
        // rather than CompleteWith is what lets a caller see it as one signal
        // whichever route reported it, `diagnosed`'s the other
        (0x01, 0x61, 0x02) => Failed(Failure::OutOfFocus),
        (0x01, ..) => CompleteWith(Adjustment::Unreported),

        // Not ready
        (0x02, 0x04, 0x00) => Working(Activity::Initializing),
        (0x02, 0x04, 0x01) => Working(Activity::becoming_ready(s.tsc)),
        (0x02, 0x04, 0x02) => Failed(Failure::Mechanism),
        (0x02, 0x04, 0x03) => NeedsOperator(Intervention::manual(s.tsc)),
        (0x02, 0x05, 0x00) => Working(Activity::Initializing),
        (0x02, 0x3A, 0x00) => match s.tsc {
            Some(0x03) => NeedsOperator(Intervention::FilmOutOfStandard),
            Some(0x04) => Refused(Refusal::FrameOutOfRange),
            _ => NeedsOperator(Intervention::NoMedium),
        },

        // Medium and hardware
        (0x03, ..) => Failed(Failure::Medium),
        (0x04, ..) => Failed(Failure::Hardware),

        // Illegal request: our bug
        (0x05, 0x1A, _) => Refused(Refusal::BadLength),
        (0x05, 0x20, _) => Refused(Refusal::UnknownOpcode),
        (0x05, 0x24, _) => Refused(Refusal::BadCdbField),
        (0x05, 0x25, _) => Refused(Refusal::BadLun),
        (0x05, 0x26, _) => Refused(Refusal::BadParamField),
        (0x05, 0x2C, 0x00) => Refused(Refusal::OutOfSequence),
        (0x05, 0x2C, 0x02) => Refused(Refusal::BadWindowCombo),

        // Unit attention: the command did not run
        (0x06, asc, ascq) => StateChanged(Change::from_code(asc, ascq)),

        // The vendor cooperative channel, which is not an error
        (0x09, 0x80, q) => NeedsHost(Coop::from(q)),

        // Aborted command, which SCSI lets us retry. 08h is an internal
        // operation that carries on regardless; 3Eh is still coming up after a
        // reset and is in neither spec
        (0x0B, 0x08, _) => Working(Activity::TargetBusy),
        (0x0B, 0x3E, _) => Working(Activity::Initializing),
        (0x0B, ..) => Failed(Failure::Aborted),

        _ => Failed(Failure::Unrecognized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Completion, Status};

    fn sense(key: u8, asc: u8, ascq: u8) -> Sense {
        Sense {
            key,
            asc,
            ascq,
            tsc: None,
            ili: false,
            information: None,
            raw: Vec::new(),
        }
    }

    /// A unit that puts 01h-61h-02h on the autofocus command's own completion
    /// has to fail it, not complete it with a note: unlike every other
    /// recovered error, the command's whole point was reaching focus, and it
    /// did not. This is the one place a caller (`scan::focus`) needs to see
    /// [`Failure::OutOfFocus`] whichever of the two routes reported it
    #[test]
    fn out_of_focus_on_the_command_itself_fails_rather_than_completes() {
        let completion = Completion {
            status: Status::CheckCondition,
            sense: Some(sense(0x01, 0x61, 0x02)),
            transferred: 0,
        };
        assert!(matches!(
            interpret(&completion),
            Outcome::Failed(Failure::OutOfFocus)
        ));
    }

    /// Every other sense key 01h condition is genuinely advisory: the command
    /// completed, and this is a footnote
    #[test]
    fn other_recovered_errors_still_complete() {
        let completion = Completion {
            status: Status::CheckCondition,
            sense: Some(sense(0x01, 0x37, 0x00)),
            transferred: 0,
        };
        assert!(matches!(
            interpret(&completion),
            Outcome::CompleteWith(Adjustment::ResolutionRounded)
        ));
    }
}
