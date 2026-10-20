//! Which scanner this is
//!
//! Two ways to tell it, and both are needed. A USB unit is identified by its
//! product ID before anything is opened, which is what an enumeration filters
//! on and what names a unit another process is holding. Anything else has to be
//! asked, and answers with the product field of standard INQUIRY.

/// Nikon
pub const VENDOR: u16 = 0x04B0;

/// A scanner this library knows the name of
///
/// Not a list of what it can drive: what a unit can do is read from its pages,
/// so an unlisted model still scans. This is for the things that follow the
/// model rather than the capabilities, of which color is the one that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Coolscan IV ED
    Ls40,
    /// Coolscan V ED
    Ls50,
    Ls4000,
    Ls5000,
    Ls8000,
    Ls9000,
}

/// Model, what INQUIRY's product field carries, and the USB product ID
///
/// Longest first, since a product string for one model contains another's:
/// "LS-5000 ED" holds "LS-50". The firewire units are on no USB bus and have
/// no product ID of their own
const MODELS: &[(Model, &str, Option<u16>)] = &[
    (Model::Ls9000, "LS-9000", None),
    (Model::Ls8000, "LS-8000", None),
    (Model::Ls5000, "LS-5000", Some(0x4002)),
    (Model::Ls4000, "LS-4000", None),
    (Model::Ls50, "LS-50", Some(0x4001)),
    (Model::Ls40, "LS-40", Some(0x4000)),
];

impl Model {
    /// The model an INQUIRY product field names, if it is one we know
    pub fn from_product(product: &str) -> Option<Self> {
        MODELS
            .iter()
            .find(|(_, name, _)| product.contains(name))
            .map(|(model, _, _)| *model)
    }

    /// The model behind a USB vendor and product ID
    pub fn from_usb(vendor: u16, product: u16) -> Option<Self> {
        if vendor != VENDOR {
            return None;
        }
        MODELS
            .iter()
            .find(|(_, _, pid)| *pid == Some(product))
            .map(|(model, _, _)| *model)
    }

    /// The USB product ID of every model that is a USB unit
    pub fn usb_products() -> impl Iterator<Item = u16> {
        MODELS.iter().filter_map(|(_, _, pid)| *pid)
    }

    /// This model's USB product ID, where it is a USB unit at all
    pub fn usb(self) -> Option<u16> {
        MODELS
            .iter()
            .find(|(model, _, _)| *model == self)
            .and_then(|(_, _, pid)| *pid)
    }

    /// What to call it
    pub fn name(self) -> &'static str {
        MODELS
            .iter()
            .find(|(model, _, _)| *model == self)
            .map(|(_, name, _)| *name)
            .expect("every model is in the table")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The product field carries more than the model, and one model's name
    /// sits inside another's
    #[test]
    fn a_product_field_names_one_model() {
        assert_eq!(Model::from_product("LS-9000 ED"), Some(Model::Ls9000));
        assert_eq!(Model::from_product("LS-5000 ED"), Some(Model::Ls5000));
        assert_eq!(Model::from_product("LS-50 ED"), Some(Model::Ls50));
        assert_eq!(Model::from_product("LS-2000"), None);
    }

    /// What a USB enumeration filters on, before anything is opened
    #[test]
    fn a_usb_unit_is_known_before_it_is_asked() {
        assert_eq!(Model::from_usb(VENDOR, 0x4002), Some(Model::Ls5000));
        assert_eq!(Model::from_usb(0x1234, 0x4002), None);
        assert_eq!(Model::Ls5000.usb(), Some(0x4002));
        // The firewire units are on no bus that has product IDs
        assert_eq!(Model::Ls9000.usb(), None);
    }

    /// Every entry answers to both of its keys
    #[test]
    fn the_table_agrees_with_itself() {
        for (model, name, pid) in MODELS {
            assert_eq!(Model::from_product(name), Some(*model));
            assert_eq!(model.usb(), *pid);
            assert_eq!(model.name(), *name);
            if let Some(pid) = pid {
                assert_eq!(Model::from_usb(VENDOR, *pid), Some(*model));
            }
        }
    }
}
