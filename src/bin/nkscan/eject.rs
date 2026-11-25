use anyhow::Result;
use nkscan::{error::Error, protocol::sense::Intervention, session::Session};

use crate::cli::Eject;

pub async fn run(args: Eject) -> Result<()> {
    let (device, transport) = crate::open(args.device).await?;

    println!("{device}");

    let mut session = Session::open(transport).await?;

    println!("Connected to scanner");

    crate::cache::forget(session.capabilities());
    match session.eject().await {
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
