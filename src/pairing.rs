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
use iroh_tickets::Ticket;

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
/// The start of a pairing code. A code uses only capital letters, digits, and
/// a colon, so a QR code can store it in its compact alphanumeric mode.
const CODE_PREFIX: &str = "YAPPR:";
/// The version of the binary layout inside a pairing code.
const CODE_VERSION: u8 = 1;
/// The longest writer name in a pairing code, in bytes.
const MAX_NAME: usize = 64;

impl Invitation {
    /// The pairing code: `YAPPR:` and the base32 of this layout:
    /// version (1 byte), token (32 bytes), deadline in Unix seconds (8 bytes,
    /// big endian), name length (1 byte), name, ticket bytes.
    pub fn encode(&self) -> Result<String> {
        let ticket: EndpointTicket = self.ticket.parse()?;
        let token = data_encoding::HEXLOWER.decode(self.token.as_bytes())?;
        if token.len() != 32 {
            bail!("the pairing token is not valid");
        }
        if self.name.len() > MAX_NAME {
            bail!("the writer name is longer than {MAX_NAME} bytes");
        }
        let mut bytes = vec![CODE_VERSION];
        bytes.extend(token);
        bytes.extend(self.expires_at.as_second().to_be_bytes());
        bytes.push(self.name.len() as u8);
        bytes.extend(self.name.as_bytes());
        bytes.extend(ticket.encode_bytes());
        Ok(format!(
            "{CODE_PREFIX}{}",
            data_encoding::BASE32_NOPAD.encode(&bytes)
        ))
    }

    pub fn decode(text: &str) -> Result<Self> {
        if text.len() > 8192 {
            bail!("the pairing code is too long");
        }
        // A person can type the code in lower case.
        let text = text.trim().to_ascii_uppercase();
        let encoded = text
            .strip_prefix(CODE_PREFIX)
            .context("use a Yappr pairing code. It starts with YAPPR:")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(encoded.as_bytes())
            .context("the pairing code is not valid")?;
        let invalid = || anyhow::anyhow!("the pairing code is not valid");
        let (&version, rest) = bytes.split_first().ok_or_else(invalid)?;
        if version != CODE_VERSION {
            bail!("the pairing code has version {version}. Update Yappr to read it");
        }
        let (token, rest) = rest.split_at_checked(32).ok_or_else(invalid)?;
        let (seconds, rest) = rest.split_at_checked(8).ok_or_else(invalid)?;
        let (&name_len, rest) = rest.split_first().ok_or_else(invalid)?;
        let (name, ticket) = rest
            .split_at_checked(name_len as usize)
            .ok_or_else(invalid)?;
        let name = String::from_utf8(name.to_vec()).map_err(|_| invalid())?;
        if name.len() > MAX_NAME || name.chars().any(char::is_control) {
            return Err(invalid());
        }
        let seconds = i64::from_be_bytes(seconds.try_into().map_err(|_| invalid())?);
        Ok(Self {
            ticket: EndpointTicket::decode_bytes(ticket)
                .map_err(|_| invalid())?
                .to_string(),
            token: data_encoding::HEXLOWER.encode(token),
            name,
            expires_at: Timestamp::from_second(seconds).map_err(|_| invalid())?,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation() -> Invitation {
        let id = iroh::SecretKey::generate().public();
        let addr = iroh::EndpointAddr::new(id)
            .with_relay_url("https://euc1-1.relay.n0.iroh.link./".parse().unwrap())
            .with_ip_addr("192.168.1.20:4919".parse().unwrap());
        Invitation {
            ticket: EndpointTicket::new(addr).to_string(),
            token: crate::choices::random_id(),
            name: "Work laptop".into(),
            expires_at: Timestamp::from_second(1_800_000_000).unwrap(),
        }
    }

    #[test]
    fn a_code_round_trips_in_any_case() {
        let i = invitation();
        let code = i.encode().unwrap();
        assert!(code.starts_with("YAPPR:"));
        // Capital letters, digits, and a colon: the QR alphanumeric mode.
        assert!(code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b':'));
        for text in [code.clone(), code.to_lowercase(), format!("  {code}\n")] {
            let back = Invitation::decode(&text).unwrap();
            assert_eq!(back.ticket, i.ticket);
            assert_eq!(back.token, i.token);
            assert_eq!(back.name, i.name);
            assert_eq!(back.expires_at, i.expires_at);
        }
    }

    #[test]
    fn a_code_is_short() {
        let code = invitation().encode().unwrap();
        assert!(code.len() < 300, "{} characters", code.len());
    }

    #[test]
    fn a_damaged_code_is_an_error() {
        let code = invitation().encode().unwrap();
        assert!(Invitation::decode(&code[..code.len() - 10]).is_err());
        assert!(Invitation::decode("YAPPR:AAAA").is_err());
        assert!(Invitation::decode("yappr://pair/eyJ0aWNrZXQiOiJ").is_err());
        assert!(Invitation::decode("hello").is_err());
    }
}
