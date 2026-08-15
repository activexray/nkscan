use anyhow::{Result, anyhow};
use nkscan::{device, error::Error, protocol::sense::Intervention, session::Session};

use crate::cli::Eject;

pub fn run(args: Eject) -> Result<()> {
    let devices = device::list();

    let device = (if let Some(d) = args.device {
        device::Selector::Location(d)
    } else {
        device::Selector::Only
    })
    .resolve(&devices)
    .map_err(|e| {
        let list: Vec<_> = devices.iter().map(ToString::to_string).collect();
        anyhow!("{e}\n\nattached:\n  {}", list.join("\n  "))
    })?;

    println!("{device}");

    let mut session = Session::open(device.open()?)?;

    println!("Connected to scanner");

    match session.eject() {
        Ok(true) => {
            println!("Ejected");
        }

        Ok(false) => {
            println!("Scanner does not support eject");
        }

        Err(Error::Media(Intervention::NoMedium)) => {
            println!("Ejected");
        }

        Err(e) => {
            return Err(e.into());
        }
    }

    Ok(())
}
