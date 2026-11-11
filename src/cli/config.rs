//! Read or change the settings of a local feed.
use anyhow::Result;
use yappr::{config::FeedConfig, net::RelaySettings};

use super::{
    args::{CommonArgs, Format},
    Usage,
};

pub(super) fn run(
    common: &CommonArgs,
    name: Option<String>,
    default_relays: bool,
    mdns: bool,
    output: Format,
) -> Result<()> {
    let relay_override = common.no_relay || !common.relays.is_empty();
    if default_relays && relay_override {
        return Err(Usage("Use --default-relays, --relay, or --no-relay.".into()).into());
    }
    if mdns && common.no_mdns {
        return Err(Usage("Use --mdns or --no-mdns.".into()).into());
    }
    let dir = common.dir()?;
    let config = if name.is_some() || relay_override || default_relays || mdns || common.no_mdns {
        FeedConfig::update(&dir, |config| {
            if let Some(name) = name {
                config.set_name(&name)?;
            }
            if relay_override || default_relays {
                config.relays = RelaySettings {
                    disabled: common.no_relay,
                    urls: common.relays.clone(),
                };
            }
            if mdns || common.no_mdns {
                config.mdns = mdns;
            }
            Ok(())
        })?
    } else {
        FeedConfig::load(&dir)?
    };

    match output {
        Format::Json => println!("{}", serde_json::to_string(&config)?),
        Format::Text => {
            let name = config.writer_name();
            if config.name.is_some() {
                println!("Writer name: {name}");
            } else {
                println!("Writer name: {name} (computer name)");
            }
            let relays = if config.relays.disabled {
                "Off".to_owned()
            } else if config.relays.urls.is_empty() {
                "Default".to_owned()
            } else {
                config.relays.urls.join(", ")
            };
            println!("Relays: {relays}");
            println!(
                "Local discovery: {}",
                if config.mdns { "On" } else { "Off" }
            );
        }
    }
    Ok(())
}
