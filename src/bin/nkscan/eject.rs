use anyhow::{Result, anyhow};
use nkscan::{device, error::Error, protocol::sense::Intervention, session::Session};

use crate::cli::Eject;

pub async fn run(args: Eject) -> Result<()> {
    let selector = match args.device {
        Some(d) => device::Selector::Location(d),
        None => device::Selector::Only,
    };
    let (device, transport) = match selector.open().await {
        Ok(opened) => opened,
        Err(e) => {
            let list: Vec<_> = device::list()
                .await
                .iter()
                .map(ToString::to_string)
                .collect();
            return Err(anyhow!("{e}\n\nattached:\n  {}", list.join("\n  ")));
        }
    };

    println!("{device}");

    let mut session = Session::open(transport).await?;

    println!("Connected to scanner");

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
