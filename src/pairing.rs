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

pub const ALPN: &[u8] = b"yappr/pair/1";

/// How long after the deadline the device that claimed an invitation can read
/// its result. New claims and confirmations still end at the deadline.
pub const RESULT_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_secs(120);

/// The status of an invitation that the writer accepted, before the device is
/// in the allow list. A failed write can leave this status. The next pairing
/// operation, or the next `yappr start`, finishes the acceptance.
#[cfg(not(wasm_browser))]
const ACCEPTING: &str = "accepting";

/// The check code that the writer and the device show while the writer
/// decides. Both sides compute it from the writer's id, the device's id, and
/// the invitation token. If the codes are the same, the device that claimed the
/// invitation is the device in your hand.
///
/// The writer keeps the first device that claims an invitation, and a device
/// shows the code only after its own claim. So another device that claims
/// first cannot make your device show a matching code.
pub fn check_code(writer: EndpointId, device: EndpointId, token: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("yappr pairing check code 1");
    hasher.update(writer.as_bytes());
    hasher.update(device.as_bytes());
    hasher.update(token.as_bytes());
    let hash = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    let n = u64::from_be_bytes(first) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

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

/// The public key and the name of a machine that a device allows without
/// pairing, such as a CI job. A device that scans the code then takes
/// deliveries from that key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineCode {
    pub id: EndpointId,
    pub name: String,
}

/// The start of a machine code. Like a pairing code, it uses only capital
/// letters, digits, a hyphen, and a colon, for the QR alphanumeric mode.
const MACHINE_PREFIX: &str = "YAPPR-MACHINE:";
/// The version of the binary layout inside a machine code.
const MACHINE_VERSION: u8 = 1;

impl MachineCode {
    /// The machine code: `YAPPR-MACHINE:` and the base32 of this layout:
    /// version (1 byte), public key (32 bytes), name length (1 byte), name.
    pub fn encode(&self) -> Result<String> {
        if self.name.trim().is_empty()
            || self.name.len() > MAX_NAME
            || self.name.chars().any(char::is_control)
        {
            bail!("use a machine name of 1 to {MAX_NAME} bytes on one line");
        }
        let mut bytes = vec![MACHINE_VERSION];
        bytes.extend(self.id.as_bytes());
        bytes.push(self.name.len() as u8);
        bytes.extend(self.name.as_bytes());
        Ok(format!(
            "{MACHINE_PREFIX}{}",
            data_encoding::BASE32_NOPAD.encode(&bytes)
        ))
    }

    pub fn decode(text: &str) -> Result<Self> {
        if text.len() > 512 {
            bail!("the machine code is too long");
        }
        let text = text.trim().to_ascii_uppercase();
        let encoded = text
            .strip_prefix(MACHINE_PREFIX)
            .context("use a Yappr machine code. It starts with YAPPR-MACHINE:")?;
        let invalid = || anyhow::anyhow!("the machine code is not valid");
        let bytes = data_encoding::BASE32_NOPAD
            .decode(encoded.as_bytes())
            .map_err(|_| invalid())?;
        let (&version, rest) = bytes.split_first().ok_or_else(invalid)?;
        if version != MACHINE_VERSION {
            bail!("the machine code has version {version}. Update Yappr to read it");
        }
        let (key, rest) = rest.split_at_checked(32).ok_or_else(invalid)?;
        let (&len, name) = rest.split_first().ok_or_else(invalid)?;
        if name.len() != len as usize {
            return Err(invalid());
        }
        let name = String::from_utf8(name.to_vec()).map_err(|_| invalid())?;
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            return Err(invalid());
        }
        let key: [u8; 32] = key.try_into().map_err(|_| invalid())?;
        Ok(Self {
            id: EndpointId::from_bytes(&key).map_err(|_| invalid())?,
            name,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: EndpointId,
    pub name: String,
    /// The address where the device receives this feed's entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
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
    /// The ticket of a device that receives entries, such as the app. The
    /// writer then delivers its entries to that address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

/// The longest ticket in a [`Join`], in bytes.
#[cfg(not(wasm_browser))]
const MAX_TICKET: usize = 4096;
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
    // Finish an interrupted acceptance before old invitations are removed.
    finish_accepting(store, &mut offers)?;
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
    if let Some(text) = &request.ticket {
        if text.len() > MAX_TICKET {
            bail!("the device address is too long");
        }
        let ticket: EndpointTicket = text.parse().context("the device address is not valid")?;
        if ticket.endpoint_addr().id != remote {
            bail!("the device address belongs to another device");
        }
    }
    let _lock = disk::lock(store.dir(), "pairing.lock")?;
    let path = store.dir().join("pairing.json");
    let mut offers: Offers = disk::read(&path)?;
    // If this fails, the device keeps waiting and asks again.
    let finished = match finish_accepting(store, &mut offers) {
        Ok(finished) => finished,
        Err(e) => {
            tracing::warn!("cannot finish a pairing acceptance: {e:#}");
            false
        }
    };
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
                ticket: request.ticket,
            });
            true
        }
    };
    // A device knows "pending", "accepted" and "rejected". An acceptance that
    // is not finished is still "pending" for the device.
    let status = match offer.status.as_str() {
        ACCEPTING => "pending".to_string(),
        other => other.to_string(),
    };
    let reply = PairReply {
        status,
        message: match offer.status.as_str() {
            "accepted" => "The writer allowed this device.",
            "rejected" => "The writer rejected this device.",
            _ => "Confirm this device on the writer.",
        }
        .into(),
    };
    // The device asks again each second. Write only the first claim.
    if claimed || finished {
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
    if !accept {
        offer.status = "rejected".into();
        return disk::write(&path, &offers);
    }
    // Save the decision first. Then a failure in the next steps leaves a
    // decision that the next pairing operation finishes, not a device in the
    // allow list with an invitation that still waits.
    offer.status = ACCEPTING.into();
    disk::write(&path, &offers)?;
    finish_accepting(store, &mut offers)?;
    disk::write(&path, &offers)
}

/// Finish the acceptances that a failed write interrupted: add each device to
/// the allow list, then mark its invitation "accepted". Returns true if an
/// invitation changed. The caller holds `pairing.lock` and saves `offers`.
#[cfg(not(wasm_browser))]
fn finish_accepting(store: &Store, offers: &mut Offers) -> Result<bool> {
    let mut changed = false;
    for offer in offers.offers.values_mut().filter(|o| o.status == ACCEPTING) {
        let candidate = offer
            .candidate
            .as_ref()
            .context("an accepted pairing invitation has no device")?;
        store.allow_list().allow(candidate.id)?;
        offer.status = "accepted".into();
        changed = true;
    }
    Ok(changed)
}

/// Finish the acceptances that a failed write interrupted. `yappr start` calls
/// this, so that the allow list and the invitations agree.
#[cfg(not(wasm_browser))]
pub fn recover(store: &Store) -> Result<()> {
    let _lock = disk::lock(store.dir(), "pairing.lock")?;
    let path = store.dir().join("pairing.json");
    if !path.exists() {
        return Ok(());
    }
    let mut offers: Offers = disk::read(&path)?;
    if finish_accepting(store, &mut offers)? {
        disk::write(&path, &offers)?;
    }
    Ok(())
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

    #[cfg(not(wasm_browser))]
    #[test]
    fn a_device_that_receives_gives_its_own_address() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let writer = iroh::SecretKey::generate().public();
        let ticket = EndpointTicket::new(iroh::EndpointAddr::new(writer)).to_string();
        let expires_at = Timestamp::now() + jiff::SignedDuration::from_secs(60);
        let invitation = create(&store, ticket, "Mac".into(), expires_at).unwrap();
        let phone = iroh::SecretKey::generate().public();
        let join_with = |id: EndpointId| Join {
            token: invitation.token.clone(),
            name: "Phone".into(),
            ticket: Some(EndpointTicket::new(iroh::EndpointAddr::new(id)).to_string()),
        };
        let other = iroh::SecretKey::generate().public();
        assert!(join(&store, phone, join_with(other)).is_err());
        join(&store, phone, join_with(phone)).unwrap();
        let candidate = get(store.dir(), &invitation.token)
            .unwrap()
            .candidate
            .unwrap();
        assert_eq!(candidate.ticket, join_with(phone).ticket);
    }

    #[test]
    fn a_check_code_is_six_digits_and_covers_both_devices_and_the_token() {
        let key = |n: u8| iroh::SecretKey::from_bytes(&[n; 32]).public();
        let (writer, phone, other) = (key(1), key(2), key(3));
        let token = "ab".repeat(32);
        let code = check_code(writer, phone, &token);
        assert_eq!(code.len(), 7);
        assert!(code[..3].bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(&code[3..4], " ");
        assert!(code[4..].bytes().all(|b| b.is_ascii_digit()));
        // The writer and the device compute it on their own.
        assert_eq!(code, check_code(writer, phone, &token));
        assert_ne!(code, check_code(writer, other, &token));
        assert_ne!(code, check_code(other, phone, &token));
        assert_ne!(code, check_code(phone, writer, &token));
        assert_ne!(code, check_code(writer, phone, &"cd".repeat(32)));
    }

    /// An invitation that a device claimed and that the writer accepted, but
    /// the write failed before the device was in the allow list.
    #[cfg(not(wasm_browser))]
    fn interrupted_acceptance() -> (tempfile::TempDir, Store, EndpointId, Join) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let writer = iroh::SecretKey::generate().public();
        let ticket = EndpointTicket::new(iroh::EndpointAddr::new(writer)).to_string();
        let expires_at = Timestamp::now() + jiff::SignedDuration::from_secs(60);
        let invitation = create(&store, ticket, "Mac".into(), expires_at).unwrap();
        let phone = iroh::SecretKey::generate().public();
        let request = Join {
            token: invitation.token.clone(),
            name: "Phone".into(),
            ticket: None,
        };
        join(&store, phone, request.clone()).unwrap();
        let path = store.dir().join("pairing.json");
        let mut offers: Offers = disk::read(&path).unwrap();
        offers.offers.get_mut(&invitation.token).unwrap().status = ACCEPTING.into();
        disk::write(&path, &offers).unwrap();
        assert!(!store.allow_list().contains(&phone));
        (dir, store, phone, request)
    }

    #[cfg(not(wasm_browser))]
    #[test]
    fn the_next_ask_of_the_device_finishes_an_interrupted_acceptance() {
        let (_dir, store, phone, request) = interrupted_acceptance();
        let reply = join(&store, phone, request.clone()).unwrap();
        assert_eq!(reply.status, "accepted");
        assert!(store.allow_list().contains(&phone));
        assert_eq!(get(store.dir(), &request.token).unwrap().status, "accepted");
    }

    #[cfg(not(wasm_browser))]
    #[test]
    fn start_finishes_an_interrupted_acceptance() {
        let (_dir, store, phone, request) = interrupted_acceptance();
        recover(&store).unwrap();
        assert!(store.allow_list().contains(&phone));
        assert_eq!(get(store.dir(), &request.token).unwrap().status, "accepted");
        // A second run changes nothing.
        recover(&store).unwrap();
    }

    #[cfg(not(wasm_browser))]
    #[test]
    fn a_new_invitation_finishes_an_interrupted_acceptance_before_cleanup() {
        let (_dir, store, phone, request) = interrupted_acceptance();
        // Move the invitation past its result window, so that cleanup would
        // remove it.
        let path = store.dir().join("pairing.json");
        let mut offers: Offers = disk::read(&path).unwrap();
        offers
            .offers
            .get_mut(&request.token)
            .unwrap()
            .invitation
            .expires_at = Timestamp::now() - jiff::SignedDuration::from_secs(600);
        disk::write(&path, &offers).unwrap();
        let ticket = EndpointTicket::new(iroh::EndpointAddr::new(phone)).to_string();
        let later = Timestamp::now() + jiff::SignedDuration::from_secs(60);
        create(&store, ticket, "Mac".into(), later).unwrap();
        assert!(store.allow_list().contains(&phone));
    }

    #[cfg(not(wasm_browser))]
    #[test]
    fn recover_without_invitations_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        recover(&store).unwrap();
        assert!(!store.dir().join("pairing.json").exists());
    }

    #[test]
    fn a_machine_code_round_trips_and_rejects_damage() {
        let code = MachineCode {
            id: iroh::SecretKey::generate().public(),
            name: "CI deploys".into(),
        };
        let text = code.encode().unwrap();
        assert!(text.starts_with("YAPPR-MACHINE:"));
        assert!(text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b':' || b == b'-'));
        assert_eq!(MachineCode::decode(&text.to_lowercase()).unwrap(), code);
        assert!(MachineCode::decode(&text[..text.len() - 4]).is_err());
        assert!(MachineCode::decode(&invitation().encode().unwrap()).is_err());
        let bad = MachineCode {
            name: "a\u{1b}b".into(),
            ..code
        };
        assert!(bad.encode().is_err());
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
