//! Local names for authenticated device identities.
use std::{collections::BTreeMap, path::Path};

use anyhow::{bail, Context, Result};
use iroh::{EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};

use crate::{disk, EndpointTicket};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: EndpointId,
    pub ticket: Option<String>,
    /// This machine delivers its entries to the device at `ticket`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receives: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) push: Option<crate::push::Registration>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty()
        || alias.len() > 48
        || !alias
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
        || !alias.as_bytes()[0].is_ascii_lowercase()
        || alias.parse::<EndpointId>().is_ok()
    {
        bail!("use an alias of 1 to 48 lowercase letters, digits, underscores, or hyphens, starting with a letter");
    }
    Ok(())
}

/// The longest alias that [`alias_for`] makes, before a number.
const MAX_DERIVED: usize = 40;

/// An alias for the device `id`, made from its name. "My phone" gives
/// "my-phone". An alias that already names `id` is used again. When another
/// device has the alias, a number follows it: "my-phone-2".
pub fn alias_for(dir: &Path, name: &str, id: EndpointId) -> Result<String> {
    let peers = list(dir)?;
    if let Some((alias, _)) = peers.iter().find(|(_, peer)| peer.id == id) {
        return Ok(alias.clone());
    }
    let mut base = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    base.truncate(MAX_DERIVED);
    let base = base.trim_end_matches('-');
    let base = match base.chars().next() {
        Some(c) if c.is_ascii_lowercase() => base.to_string(),
        Some(_) => format!("device-{base}"),
        None => "device".to_string(),
    };
    (1..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        })
        .find(|alias| !peers.contains_key(alias) && validate_alias(alias).is_ok())
        .context("no free alias")
}

pub fn list(dir: &Path) -> Result<BTreeMap<String, Peer>> {
    disk::read(&dir.join("peers.json"))
}

/// An alias never grants access or silently changes its target identity.
pub fn set(dir: &Path, alias: &str, target: &str) -> Result<()> {
    validate_alias(alias)?;
    let (id, ticket) = if let Ok(ticket) = target.parse::<EndpointTicket>() {
        (ticket.endpoint_addr().id, Some(ticket.to_string()))
    } else {
        (
            target
                .parse()
                .context("use a full device ID or feed ticket")?,
            None,
        )
    };
    let _lock = disk::lock(dir, "peers.lock")?;
    let mut peers = list(dir)?;
    if let Some(peer) = peers.get_mut(alias) {
        if peer.id != id {
            bail!("alias {alias:?} belongs to another device. Remove the alias before assigning a different device");
        }
        if ticket.is_some() {
            peer.ticket = ticket;
        }
    } else {
        peers.insert(
            alias.into(),
            Peer {
                id,
                ticket,
                receives: false,
                push: None,
                extra: BTreeMap::new(),
            },
        );
    }
    disk::write(&dir.join("peers.json"), &peers)
}

/// Save `ticket` as the address of `alias`, and deliver this machine's
/// entries to it.
pub fn set_receiver(dir: &Path, alias: &str, ticket: &EndpointTicket) -> Result<()> {
    set(dir, alias, &ticket.to_string())?;
    let _lock = disk::lock(dir, "peers.lock")?;
    let mut peers = list(dir)?;
    if let Some(peer) = peers.get_mut(alias) {
        peer.receives = true;
    }
    disk::write(&dir.join("peers.json"), &peers)
}

/// The devices that receive this machine's entries, with their addresses.
pub fn receivers(dir: &Path) -> Result<Vec<(String, EndpointAddr)>> {
    let mut out = Vec::new();
    for (alias, peer) in list(dir)? {
        if !peer.receives {
            continue;
        }
        let Some(ticket) = peer.ticket else { continue };
        let ticket: EndpointTicket = ticket
            .parse()
            .context("the saved peer ticket is not valid")?;
        if ticket.endpoint_addr().id == peer.id {
            out.push((alias, ticket.endpoint_addr().clone()));
        }
    }
    Ok(out)
}

pub fn remove(dir: &Path, alias: &str) -> Result<()> {
    let _lock = disk::lock(dir, "peers.lock")?;
    let mut peers = list(dir)?;
    if peers.remove(alias).is_none() {
        bail!("alias {alias:?} does not exist");
    }
    disk::write(&dir.join("peers.json"), &peers)
}

pub fn resolve(dir: &Path, target: &str) -> Result<EndpointAddr> {
    if let Ok(ticket) = target.parse::<EndpointTicket>() {
        return Ok(ticket.endpoint_addr().clone());
    }
    if let Ok(id) = target.parse::<EndpointId>() {
        return Ok(EndpointAddr::new(id));
    }
    let peer = list(dir)?.remove(target).with_context(|| {
        format!("unknown device alias {target:?}. Use yappr alias set NAME ID_OR_TICKET")
    })?;
    match peer.ticket {
        Some(ticket) => {
            let ticket: EndpointTicket = ticket
                .parse()
                .context("the saved peer ticket is not valid")?;
            if ticket.endpoint_addr().id != peer.id {
                bail!("the saved alias and ticket identities do not match");
            }
            Ok(ticket.endpoint_addr().clone())
        }
        None => Ok(EndpointAddr::new(peer.id)),
    }
}
