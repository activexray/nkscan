use anyhow::Result;
use nkscan::{
    device,
    error::Error,
    protocol::sense::Intervention,
    session::Session,
};

use crate::cli::Eject;

pub fn run(_args: Eject) -> Result<()> {
    let device = device::list()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No scanner found"))?;

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