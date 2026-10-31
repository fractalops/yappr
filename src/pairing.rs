//! Expiring invitations with confirmation on the writer.
#[cfg(not(wasm_browser))]
use std::{collections::BTreeMap, path::Path};

use anyhow::{bail, Context, Result};
use iroh::EndpointId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::EndpointTicket;
#[cfg(not(wasm_browser))]
use crate::{disk, store::Store};

pub const ALPN: &[u8] = b"pair/0";

/// How long after the deadline the device that claimed an invitation can read
/// its result. New claims and confirmations still end at the deadline.
pub const RESULT_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_secs(120);

/// The end of the result window of `offer`.
#[cfg(not(wasm_browser))]
fn result_until(offer: &Offer) -> Timestamp {
    offer
        .invitation
        .expires_at
        .checked_add(RESULT_WINDOW)
        .unwrap_or(offer.invitation.expires_at)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invitation {
    pub ticket: String,
    pub token: String,
    pub name: String,
    pub expires_at: Timestamp,
}
impl Invitation {
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "yappr://pair/{}",
            data_encoding::BASE64URL_NOPAD.encode(&serde_json::to_vec(self)?)
        ))
    }
    pub fn decode(text: &str) -> Result<Self> {
        if text.len() > 8192 {
            bail!("the pairing code is too long");
        }
        let encoded = text
            .trim()
            .strip_prefix("yappr://pair/")
            .context("use a Yappr pairing code")?;
        let value: Self =
            serde_json::from_slice(&data_encoding::BASE64URL_NOPAD.decode(encoded.as_bytes())?)?;
        value.ticket.parse::<EndpointTicket>()?;
        if !crate::choices::valid_id(&value.token) {
            bail!("the pairing token is not valid");
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: EndpointId,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub invitation: Invitation,
    pub candidate: Option<Candidate>,
    pub status: String,
}
#[cfg(not(wasm_browser))]
#[derive(Debug, Default, Serialize, Deserialize)]
struct Offers {
    offers: BTreeMap<String, Offer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Join {
    pub token: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairReply {
    pub status: String,
    pub message: String,
}

#[cfg(not(wasm_browser))]
pub fn create(
    store: &Store,
    ticket: String,
    name: String,
    expires_at: Timestamp,
) -> Result<Invitation> {
    ticket.parse::<EndpointTicket>()?;
    if name.trim().is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        bail!("use a device name with 1 to 64 bytes on one line");
    }
    let invitation = Invitation {
        ticket,
        token: disk::random_id(),
        name,
        expires_at,
    };
    let _lock = disk::lock(store.dir(), "pairing.lock")?;
    let path = store.dir().join("pairing.json");
    let mut offers: Offers = disk::read(&path)?;
    offers
        .offers
        .retain(|_, o| result_until(o) > Timestamp::now());
    offers.offers.insert(
        invitation.token.clone(),
        Offer {
            invitation: invitation.clone(),
            candidate: None,
            status: "pending".into(),
        },
    );
    disk::write(&path, &offers)?;
    Ok(invitation)
}

/// True while an invitation in `dir` waits for a claim or for the writer's
/// answer, and has not expired.
#[cfg(not(wasm_browser))]
pub fn open(dir: &Path) -> bool {
    let now = Timestamp::now();
    read_offers(dir).is_some_and(|offers| {
        offers
            .offers
            .values()
            .any(|o| o.status == "pending" && o.invitation.expires_at > now)
    })
}

/// True if the unknown endpoint `remote` can use the pairing protocol: an
/// invitation is open, or `remote` claimed an invitation and its result window
/// has not ended.
#[cfg(not(wasm_browser))]
pub fn admits(dir: &Path, remote: EndpointId) -> bool {
    let now = Timestamp::now();
    open(dir)
        || read_offers(dir).is_some_and(|offers| {
            offers.offers.values().any(|o| {
                o.candidate.as_ref().is_some_and(|c| c.id == remote) && result_until(o) > now
            })
        })
}

#[cfg(not(wasm_browser))]
fn read_offers(dir: &Path) -> Option<Offers> {
    match disk::read::<Offers>(&dir.join("pairing.json")) {
        Ok(offers) => Some(offers),
        Err(e) => {
            tracing::warn!("cannot read the pairing invitations: {e:#}");
            None
        }
    }
}

#[cfg(not(wasm_browser))]
pub fn get(dir: &Path, token: &str) -> Result<Offer> {
    let offers: Offers = disk::read(&dir.join("pairing.json"))?;
    offers
        .offers
        .get(token)
        .cloned()
        .context("the pairing invitation does not exist")
}

#[cfg(not(wasm_browser))]
pub fn join(store: &Store, remote: EndpointId, request: Join) -> Result<PairReply> {
    if request.name.trim().is_empty()
        || request.name.len() > 64
        || request.name.chars().any(char::is_control)
    {
        bail!("use a device name with 1 to 64 bytes on one line");
    }
    let _lock = disk::lock(store.dir(), "pairing.lock")?;
    let path = store.dir().join("pairing.json");
    let mut offers: Offers = disk::read(&path)?;
    let offer = offers
        .offers
        .get_mut(&request.token)
        .context("the pairing invitation is not valid")?;
    let now = Timestamp::now();
    if offer.invitation.expires_at <= now {
        // After the deadline, only the device that claimed the invitation can
        // read the writer's answer, and only for the result window.
        let answered = offer.status != "pending";
        let claimant = offer.candidate.as_ref().is_some_and(|c| c.id == remote);
        if !(answered && claimant && result_until(offer) > now) {
            bail!("the pairing invitation expired");
        }
    }
    let claimed = match &offer.candidate {
        Some(candidate) if candidate.id != remote => {
            bail!("another device claimed this pairing invitation")
        }
        Some(_) => false,
        None => {
            offer.candidate = Some(Candidate {
                id: remote,
                name: request.name,
            });
            true
        }
    };
    let reply = PairReply {
        status: offer.status.clone(),
        message: match offer.status.as_str() {
            "accepted" => "The writer allowed this device.",
            "rejected" => "The writer rejected this device.",
            _ => "Confirm this device on the writer.",
        }
        .into(),
    };
    // The device asks again each second. Write only the first claim.
    if claimed {
        disk::write(&path, &offers)?;
    }
    Ok(reply)
}

#[cfg(not(wasm_browser))]
pub fn confirm(store: &Store, remote: EndpointId, accept: bool) -> Result<()> {
    let _lock = disk::lock(store.dir(), "pairing.lock")?;
    let path = store.dir().join("pairing.json");
    let mut offers: Offers = disk::read(&path)?;
    let offer = offers
        .offers
        .values_mut()
        .find(|o| {
            o.status == "pending"
                && o.invitation.expires_at > Timestamp::now()
                && o.candidate.as_ref().is_some_and(|c| c.id == remote)
        })
        .context("this device has no pending pairing request")?;
    if accept {
        store.allow_list().allow(remote)?;
    }
    offer.status = if accept { "accepted" } else { "rejected" }.into();
    disk::write(&path, &offers)
}

#[cfg(not(wasm_browser))]
pub fn revoke(dir: &Path, token: &str) -> Result<()> {
    let _lock = disk::lock(dir, "pairing.lock")?;
    let path = dir.join("pairing.json");
    let mut offers: Offers = disk::read(&path)?;
    if let Some(offer) = offers.offers.get_mut(token) {
        if offer.status == "pending" {
            offer.status = "rejected".into();
        }
    }
    disk::write(&path, &offers)
}
