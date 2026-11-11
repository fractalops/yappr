//! Endpoint setup and the local identity.
#[cfg(not(wasm_browser))]
use std::{
    fs,
    io::{Read, Write},
    net::Ipv4Addr,
    path::Path,
    str::FromStr,
};

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
    /// The relay mode of these settings: the n0 relays or the custom relays,
    /// with each URL as [`without_trailing_dot`] gives it.
    fn relay_mode(&self) -> Result<RelayMode> {
        Ok(match self.disabled {
            true => RelayMode::Disabled,
            false => RelayMode::Custom(RelayMap::from_iter(self.relay_urls()?)),
        })
    }

    /// The relays of these settings, as [`without_trailing_dot`] gives them.
    /// Disabled relays give none.
    pub fn relay_urls(&self) -> Result<Vec<RelayUrl>> {
        self.validate()?;
        if self.disabled {
            return Ok(Vec::new());
        }
        let urls: Vec<RelayUrl> = if self.urls.is_empty() {
            iroh::defaults::prod::default_relay_map().urls()
        } else {
            self.urls
                .iter()
                .map(|s| s.parse())
                .collect::<Result<_, _>>()?
        };
        Ok(urls.iter().map(without_trailing_dot).collect())
    }
}

/// Bind a browser endpoint. A browser reaches other endpoints through relays
/// only, so `relays` must not disable them.
#[cfg(wasm_browser)]
pub async fn bind(secret: SecretKey, relays: &RelaySettings) -> Result<Endpoint> {
    if relays.disabled {
        anyhow::bail!("a browser needs a relay");
    }
    Ok(Endpoint::builder(presets::N0)
        .secret_key(secret)
        .relay_mode(relays.relay_mode()?)
        .bind()
        .await?)
}

/// `url` without the trailing dot of its host name. WebKit sends that dot in
/// the TLS server name, and a relay refuses such a handshake. Every endpoint
/// uses this form: with two forms of one relay URL, an endpoint opens two
/// sessions to one relay, and the relay keeps only one of them.
fn without_trailing_dot(url: &RelayUrl) -> RelayUrl {
    match url.host_str() {
        Some(host) if host.ends_with('.') => url
            .to_string()
            .replacen(host, host.trim_end_matches('.'), 1)
            .parse()
            .unwrap_or_else(|_| url.clone()),
        _ => url.clone(),
    }
}

/// The address of a device that another device gave: its id, and only the
/// relays in `known`. This machine then contacts no host that the other
/// device chose. It finds direct paths by itself.
pub fn known_relays_only(addr: &EndpointAddr, known: &[RelayUrl]) -> EndpointAddr {
    let relays = addr
        .relay_urls()
        .map(without_trailing_dot)
        .filter(|url| known.contains(url))
        .map(iroh::TransportAddr::Relay);
    EndpointAddr::from_parts(addr.id, relays)
}

/// `addr` with relay URLs in the form that every endpoint uses. See
/// [`without_trailing_dot`].
pub fn relay_addr(addr: EndpointAddr) -> EndpointAddr {
    let addrs = addr.addrs.into_iter().map(|a| match a {
        iroh::TransportAddr::Relay(url) => iroh::TransportAddr::Relay(without_trailing_dot(&url)),
        other => other,
    });
    EndpointAddr::from_parts(addr.id, addrs)
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
    if opts.network == Network::Default {
        builder = builder.relay_mode(opts.relays.relay_mode()?);
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

/// The environment variables that can hold the private key, in order. The
/// second one is the older name.
#[cfg(not(wasm_browser))]
pub const KEY_VARS: [&str; 2] = ["YAPPR_KEY", "IROH_SECRET"];

/// The private key from `YAPPR_KEY` or `IROH_SECRET`, or from
/// `<dir>/secret.key`. Create the file if it does not exist.
#[cfg(not(wasm_browser))]
pub fn load_or_create_secret(dir: &Path) -> Result<SecretKey> {
    for var in KEY_VARS {
        if let Ok(key) = std::env::var(var) {
            return SecretKey::from_str(key.trim())
                .with_context(|| format!("{var} is not a valid private key"));
        }
    }
    let _lock = crate::disk::lock(dir, "identity.lock")?;
    let path = dir.join("secret.key");
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            // The open checks the owner, removes access for other users, and
            // refuses a symbolic link.
            let mut text = String::new();
            store::private_file(&path)?.read_to_string(&mut text)?;
            SecretKey::from_str(text.trim())
                .with_context(|| format!("{} is not a valid secret key", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            store::create_private_dir(dir)?;
            let key = SecretKey::generate();
            let mut file = store::new_private_file(&path)?;
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
    fn every_endpoint_uses_relay_urls_without_a_trailing_dot() {
        let RelayMode::Custom(map) = RelaySettings::default().relay_mode().unwrap() else {
            panic!("the n0 relays are custom relays without the dot");
        };
        let urls: Vec<RelayUrl> = map.urls();
        assert!(!urls.is_empty());
        assert!(urls
            .iter()
            .all(|url| !url.host_str().unwrap().ends_with('.')));
        let addr = EndpointAddr::new(SecretKey::generate().public())
            .with_relay_url("https://relay.example.com./".parse().unwrap());
        assert_eq!(
            relay_addr(addr).relay_urls().next().unwrap().to_string(),
            "https://relay.example.com/"
        );
    }

    #[test]
    fn a_given_address_keeps_only_the_known_relays() {
        let known = RelaySettings::default().relay_urls().unwrap();
        let addr = EndpointAddr::new(SecretKey::generate().public())
            .with_relay_url(known[0].clone())
            .with_relay_url("http://192.168.1.1/admin".parse().unwrap())
            .with_ip_addr("192.168.1.1:80".parse().unwrap());
        let kept = known_relays_only(&addr, &known);
        assert_eq!(kept.id, addr.id);
        assert_eq!(kept.relay_urls().collect::<Vec<_>>(), [&known[0]]);
        assert_eq!(kept.ip_addrs().count(), 0);
        assert!(known_relays_only(&addr, &[]).addrs.is_empty());
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
