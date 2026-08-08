//! Deciding where to focus

/// How focusing went
///
/// Not reaching focus is a recovered error, sense key 01h, so the command
/// finished and the lens is wherever it ended up. Worth knowing about, not
/// worth refusing to scan over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focused {
    /// The unit reached focus, or was driven to a position
    Yes,
    /// Autofocus ran and did not converge. It focuses on grain rather than on
    /// the picture, so there is no such thing as a subject too smooth for it
    NotReached,
    /// Nothing was asked of it
    Skipped,
}

/// What to do about focus before a scan
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    /// Let the unit focus on a point of the window, given as a fraction of its
    /// size. `(0.5, 0.5)` is the middle
    ///
    /// `color` needs the unit to offer `Op::ColorAutoFocus`. `None` uses
    /// `Op::AutoFocus` and lets it choose the channel.
    Auto { at: (f32, f32), color: Option<u8> },
    /// Drive the lens to an absolute position, bounded by `Address` bytes 76-79
    At(u16),
    /// Leave the focus wherever it is
    Hold,
}

impl Default for Focus {
    /// The middle of the window. Nikon Scan focuses there and nowhere else,
    /// and grain is what the unit measures, so the picture does not matter
    fn default() -> Self {
        Self::Auto {
            at: (0.5, 0.5),
            color: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_middle_of_the_window() {
        assert_eq!(
            Focus::default(),
            Focus::Auto {
                at: (0.5, 0.5),
                color: None
            }
        );
    }
}
