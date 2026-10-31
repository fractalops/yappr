//! Endpoint setup and the local identity.
#[cfg(not(wasm_browser))]
use std::{fs, io::Write, net::Ipv4Addr, path::Path, str::FromStr};

use anyhow::{Context, Result};
use iroh::{
    endpoint::presets, Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl, SecretKey, Watcher,
};
#[cfg(not(wasm_browser))]
use iroh_mdns_address_lookup::MdnsAddressLookup;
use serde::{Deserialize, Serialize};

#[cfg(not(wasm_browser))]
use crate::{access::AccessHook, store};

/// How the endpoint finds and reaches other endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Network {
    /// n0 relays and n0 address lookup.
    #[default]
    Default,
    /// Loopback only. No relays and no address lookup. For tests.
    Local,
}

/// How to bind an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetOptions {
    pub network: Network,
    /// The UDP port for IPv4. `None` gives a random port.
    pub port: Option<u16>,
    /// Find endpoints on the local network with mDNS, and announce this one.
    pub mdns: bool,
    pub relays: RelaySettings,
}

impl From<Network> for NetOptions {
    fn from(network: Network) -> Self {
        Self {
            network,
            port: None,
            mdns: network == Network::Default,
            relays: RelaySettings::default(),
        }
    }
}

/// Empty URLs use n0 defaults. Disabled mode uses direct paths only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelaySettings {
    pub disabled: bool,
    pub urls: Vec<String>,
}
impl RelaySettings {
    pub fn validate(&self) -> Result<()> {
        if self.disabled && !self.urls.is_empty() {
            anyhow::bail!("choose custom relays or disabled relays, not both");
        }
        if self.urls.len() > 16 {
            anyhow::bail!("configure at most 16 relays");
        }
        for text in &self.urls {
            let url: RelayUrl = text.parse().context("the relay URL is not valid")?;
            if !matches!(url.scheme(), "https" | "http")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                anyhow::bail!(
                    "use an HTTP or HTTPS relay URL without credentials, a query, or a fragment"
                );
            }
        }
        Ok(())
    }
    #[cfg(not(wasm_browser))]
    pub fn load(dir: &Path) -> Result<Self> {
        let settings: Self = crate::disk::read(&dir.join("network.json"))?;
        settings.validate()?;
        Ok(settings)
    }
    #[cfg(not(wasm_browser))]
    pub fn save(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        let _lock = crate::disk::lock(dir, "network.lock")?;
        crate::disk::write(&dir.join("network.json"), self)
    }

    /// The relay mode of these settings. `None` keeps the preset relays.
    fn relay_mode(&self) -> Result<Option<RelayMode>> {
        self.validate()?;
        if self.disabled {
            return Ok(Some(RelayMode::Disabled));
        }
        if self.urls.is_empty() {
            return Ok(None);
        }
        let urls: Vec<RelayUrl> = self
            .urls
            .iter()
            .map(|s| s.parse())
            .collect::<Result<_, _>>()?;
        Ok(Some(RelayMode::Custom(RelayMap::from_iter(urls))))
    }
}

/// Bind a browser endpoint. A browser reaches other endpoints through relays
/// only, so `relays` must not disable them.
#[cfg(wasm_browser)]
pub async fn bind(secret: SecretKey, relays: &RelaySettings) -> Result<Endpoint> {
    if relays.disabled {
        anyhow::bail!("a browser needs a relay");
    }
    let mut builder = Endpoint::builder(presets::N0).secret_key(secret);
    if let Some(mode) = relays.relay_mode()? {
        builder = builder.relay_mode(mode);
    }
    Ok(builder.bind().await?)
}

/// Bind an endpoint. `hook` controls access to incoming connections.
#[cfg(not(wasm_browser))]
pub async fn bind(
    secret: SecretKey,
    opts: impl Into<NetOptions>,
    alpns: Vec<Vec<u8>>,
    hook: Option<AccessHook>,
) -> Result<Endpoint> {
    let opts = opts.into();
    let mut builder = match opts.network {
        Network::Default => {
            let builder = Endpoint::builder(presets::N0);
            match opts.port {
                Some(port) => builder.bind_addr((Ipv4Addr::UNSPECIFIED, port))?,
                None => builder,
            }
        }
        Network::Local => Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, opts.port.unwrap_or(0)))?,
    };
    opts.relays.validate()?;
    if opts.network == Network::Local && !opts.relays.urls.is_empty() {
        anyhow::bail!("relay settings do not apply to loopback test mode");
    }
    if let Some(mode) = opts.relays.relay_mode()? {
        builder = builder.relay_mode(mode);
    }
    if opts.mdns {
        builder = builder.address_lookup(MdnsAddressLookup::builder());
    }
    builder = builder.secret_key(secret).alpns(alpns);
    if let Some(hook) = hook {
        builder = builder.hooks(hook);
    }
    Ok(builder.bind().await?)
}

/// The address of `endpoint`, after it knows at least one way to reach it.
pub async fn addr(endpoint: &Endpoint) -> EndpointAddr {
    let mut watcher = endpoint.watch_addr();
    loop {
        let addr = watcher.get();
        if addr.ip_addrs().next().is_some() || addr.relay_urls().next().is_some() {
            return addr;
        }
        if watcher.updated().await.is_err() {
            return endpoint.addr();
        }
    }
}

/// The secret key from `IROH_SECRET`, or from `<dir>/secret.key`. Create the
/// file if it does not exist.
#[cfg(not(wasm_browser))]
pub fn load_or_create_secret(dir: &Path) -> Result<SecretKey> {
    if let Ok(secret) = std::env::var("IROH_SECRET") {
        return SecretKey::from_str(secret.trim()).context("IROH_SECRET is not a valid secret key");
    }
    let _lock = crate::disk::lock(dir, "identity.lock")?;
    let path = dir.join("secret.key");
    match fs::read_to_string(&path) {
        Ok(text) => SecretKey::from_str(text.trim())
            .with_context(|| format!("{} is not a valid secret key", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            store::create_private_dir(dir)?;
            let key = SecretKey::generate();
            let mut file = store::private_file(&path)?;
            writeln!(file, "{}", data_encoding::HEXLOWER.encode(&key.to_bytes()))?;
            file.sync_all()?;
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn options_from_network() {
        let local = NetOptions::from(Network::Local);
        assert_eq!((local.port, local.mdns), (None, false));
        let default = NetOptions::from(Network::Default);
        assert_eq!((default.port, default.mdns), (None, true));
    }

    #[tokio::test]
    async fn binds_the_given_port() {
        let port = free_port();
        let opts = NetOptions {
            port: Some(port),
            ..Network::Local.into()
        };
        let ep = bind(SecretKey::generate(), opts, vec![], None)
            .await
            .unwrap();
        assert!(
            ep.bound_sockets().iter().any(|a| a.port() == port),
            "{:?}",
            ep.bound_sockets()
        );
        ep.close().await;
    }
}
