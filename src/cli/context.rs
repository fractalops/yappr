//! Feed paths, identity, and network configuration shared by CLI commands.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use iroh::EndpointAddr;
use yappr::{
    entry,
    net::{self, NetOptions, Network},
    store::Store,
    EndpointTicket,
};

use super::args::CommonArgs;

impl CommonArgs {
    /// The feed directory.
    pub(super) fn dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        if !entry::is_plain_file_name(&self.feed) || self.feed.starts_with('.') {
            bail!("feed name {:?} is not a plain name", self.feed);
        }
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::data_dir)
            .context("cannot find a data directory. Use --dir")?;
        Ok(base.join("yappr").join(&self.feed))
    }

    /// The feed of this machine. It signs new entries with this machine's key.
    pub(super) fn store(&self) -> Result<Store> {
        Ok(Store::open(self.dir()?)?.signed_by(self.secret()?))
    }

    pub(super) fn net(&self, port: Option<u16>) -> Result<NetOptions> {
        let relays = if self.no_relay || !self.relays.is_empty() {
            net::RelaySettings {
                disabled: self.no_relay,
                urls: self.relays.clone(),
            }
        } else {
            net::RelaySettings::load(&self.dir()?)?
        };
        relays.validate()?;
        Ok(NetOptions {
            port,
            mdns: !self.no_mdns && self.network == Network::Default,
            relays,
            ..self.network.into()
        })
    }

    pub(super) fn secret(&self) -> Result<iroh::SecretKey> {
        net::load_or_create_secret(&self.dir()?)
    }
}

pub(super) fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let short = name.split('.').next().unwrap_or_default();
    if short.is_empty() || short.len() > entry::MAX_SOURCE {
        "localhost".into()
    } else {
        short.into()
    }
}

/// A ticket with only the endpoint id and the relay urls. The follower finds
/// the direct addresses with address lookup.
pub(super) fn short_ticket(addr: &EndpointAddr) -> EndpointTicket {
    let mut short = EndpointAddr::new(addr.id);
    for relay_url in addr.relay_urls() {
        short = short.with_relay_url(relay_url.clone());
    }
    short.into()
}
