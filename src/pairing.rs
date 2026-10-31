//! Expiring invitations with confirmation on the writer.
use std::{collections::BTreeMap, path::Path};

use anyhow::{bail, Context, Result};
use iroh::EndpointId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{disk, store::Store, EndpointTicket};

pub const ALPN: &[u8] = b"pair/0";

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
        .retain(|_, o| o.invitation.expires_at > Timestamp::now());
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

/// True while an invitation in `dir` can still give an unknown device an
/// answer. Only then can an unknown device use the pairing protocol.
/// An accepted device is in the allow list and does not need this.
pub fn open(dir: &Path) -> bool {
    match disk::read::<Offers>(&dir.join("pairing.json")) {
        Ok(offers) => {
            let now = Timestamp::now();
            offers
                .offers
                .values()
                .any(|o| o.status != "accepted" && o.invitation.expires_at > now)
        }
        Err(e) => {
            tracing::warn!("cannot read the pairing invitations: {e:#}");
            false
        }
    }
}

pub fn get(dir: &Path, token: &str) -> Result<Offer> {
    let offers: Offers = disk::read(&dir.join("pairing.json"))?;
    offers
        .offers
        .get(token)
        .cloned()
        .context("the pairing invitation does not exist")
}

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
    if offer.invitation.expires_at <= Timestamp::now() {
        bail!("the pairing invitation expired");
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
