//! Web Push subscriptions and writer delivery.
use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub vapid_private: String,
    pub vapid_public: String,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Subscription(..)")
    }
}

#[cfg(not(wasm_browser))]
pub(crate) use writer::Registration;
#[cfg(not(wasm_browser))]
pub use writer::{clear, notify, notify_after_failure, set};

#[cfg(not(wasm_browser))]
mod writer {
    use std::{collections::BTreeMap, path::Path, time::Duration};

    use anyhow::{anyhow, Context, Result};
    use base64ct::{Base64UrlUnpadded, Encoding};
    use iroh::EndpointId;
    use jiff::{SignedDuration, Timestamp};
    use reqwest::{Client, StatusCode, Url};
    use ring::{
        agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256},
        hkdf,
        rand::{SecureRandom, SystemRandom},
        signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING},
    };

    use super::*;
    use crate::{choices, disk, entry::Entry, peers, proto::Stored, store::Store};

    const WAIT: SignedDuration = SignedDuration::from_secs(5);
    const DEBOUNCE: SignedDuration = SignedDuration::from_secs(10);
    const FRESH: SignedDuration = SignedDuration::from_secs(120);
    const MAX_PAYLOAD: usize = 3800;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct Registration {
        subscription: Subscription,
        log: Option<String>,
        through: u64,
        retry_at: Option<Timestamp>,
    }

    fn invalid() -> choices::ChoiceError {
        choices::ChoiceError::new("invalid_push", "The push subscription is invalid.")
    }

    fn endpoint(value: &str) -> Result<Url> {
        let url = Url::parse(value).map_err(|_| invalid())?;
        let host = url.host_str().unwrap_or_default();
        let approved = matches!(
            host,
            "fcm.googleapis.com"
                | "updates.push.services.mozilla.com"
                | "push.services.mozilla.com"
        ) || host.ends_with(".push.apple.com")
            || host.ends_with(".notify.windows.com");
        if !approved
            || url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port_or_known_default() != Some(443)
            || url.fragment().is_some()
            || url.path() == "/"
            || value.len() > 4096
        {
            return Err(invalid().into());
        }
        Ok(url)
    }

    impl Subscription {
        fn keys(&self, rng: &SystemRandom) -> Result<(Vec<u8>, [u8; 16], EcdsaKeyPair)> {
            if self.p256dh.len() != 87
                || self.auth.len() != 22
                || self.vapid_private.len() != 43
                || self.vapid_public.len() != 87
            {
                return Err(invalid().into());
            }
            let decode = |value| Base64UrlUnpadded::decode_vec(value).map_err(|_| invalid());
            let key = decode(&self.p256dh)?;
            if key.first() != Some(&4) {
                return Err(invalid().into());
            }
            let auth: [u8; 16] = decode(&self.auth)?.try_into().map_err(|_| invalid())?;
            let vapid = EcdsaKeyPair::from_private_key_and_public_key(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &decode(&self.vapid_private)?,
                &decode(&self.vapid_public)?,
                rng,
            )
            .map_err(|_| invalid())?;
            Ok((key, auth, vapid))
        }

        pub(crate) fn validate(&self) -> Result<()> {
            endpoint(&self.endpoint)?;
            let rng = SystemRandom::new();
            let (key, _, _) = self.keys(&rng)?;
            let secret = EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| invalid())?;
            agreement::agree_ephemeral(secret, &UnparsedPublicKey::new(&ECDH_P256, key), |_| ())
                .map_err(|_| invalid())?;
            Ok(())
        }

        fn request(&self, client: &Client, payload: Vec<u8>) -> Result<reqwest::Request> {
            let url = endpoint(&self.endpoint)?;
            let rng = SystemRandom::new();
            let (key, auth, vapid) = self.keys(&rng)?;
            let body = encrypt(&rng, &key, &auth, payload)?;
            let authorization = authorization(&rng, &url, &vapid)?;
            Ok(client
                .post(url)
                .header("Authorization", authorization)
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .header("TTL", 60)
                .body(body)
                .build()
                .map_err(reqwest::Error::without_url)?)
        }
    }

    fn encrypt(rng: &SystemRandom, key: &[u8], auth: &[u8], payload: Vec<u8>) -> Result<Vec<u8>> {
        let error = || anyhow!("Cannot encrypt the notification.");
        let secret = EphemeralPrivateKey::generate(&ECDH_P256, rng).map_err(|_| error())?;
        let sender = secret.compute_public_key().map_err(|_| error())?;
        let recipient = UnparsedPublicKey::new(&ECDH_P256, key);
        let ikm = agreement::agree_ephemeral(secret, &recipient, |shared| {
            content_key(auth, shared, key, sender.as_ref())
        })
        .map_err(|_| error())??;
        let mut salt = [0; 16];
        rng.fill(&mut salt).map_err(|_| error())?;
        let size = u32::try_from(payload.len() + 17)?;
        ece_native::encrypt(ikm, salt, sender, std::iter::once(payload), size).map_err(|_| error())
    }

    // RFC 8291, section 3.3. The content encoding adds its own salt and headers.
    fn content_key(
        auth: &[u8],
        shared: &[u8],
        recipient: &[u8],
        sender: &[u8],
    ) -> Result<[u8; 32]> {
        let info: &[&[u8]] = &[b"WebPush: info\0", recipient, sender];
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, auth);
        let prk = salt.extract(shared);
        let mut key = [0; 32];
        prk.expand(info, hkdf::HKDF_SHA256)
            .and_then(|value| value.fill(&mut key))
            .map_err(|_| anyhow!("Cannot derive the notification key."))?;
        Ok(key)
    }

    // RFC 8292 uses a compact ES256 JWT and an uncompressed public key.
    fn authorization(rng: &SystemRandom, url: &Url, key: &EcdsaKeyPair) -> Result<String> {
        let claims = serde_json::to_vec(&serde_json::json!({
            "aud": url.origin().ascii_serialization(),
            "exp": Timestamp::now().as_second() + 3600,
            "sub": "https://yappr.run",
        }))?;
        let header = Base64UrlUnpadded::encode_string(br#"{"alg":"ES256","typ":"JWT"}"#);
        let claims = Base64UrlUnpadded::encode_string(&claims);
        let token = format!("{header}.{claims}");
        let signature = key
            .sign(rng, token.as_bytes())
            .map_err(|_| anyhow!("Cannot sign the notification."))?;
        let signature = Base64UrlUnpadded::encode_string(signature.as_ref());
        let public = Base64UrlUnpadded::encode_string(key.public_key().as_ref());
        Ok(format!("vapid t={token}.{signature}, k={public}"))
    }

    fn edit<T>(
        dir: &Path,
        apply: impl FnOnce(&mut BTreeMap<String, peers::Peer>) -> Result<(T, bool)>,
    ) -> Result<T> {
        let _lock = disk::lock(dir, "peers.lock")?;
        let mut peers = peers::list(dir)?;
        let (result, changed) = apply(&mut peers)?;
        if changed {
            disk::write(&dir.join("peers.json"), &peers)?;
        }
        Ok(result)
    }

    pub fn set(
        store: &Store,
        device: EndpointId,
        subscription: Option<Subscription>,
    ) -> Result<()> {
        if !store.allow_list().contains(&device) {
            return Err(
                choices::ChoiceError::new("denied", "This device cannot read the feed.").into(),
            );
        }
        if let Some(value) = &subscription {
            value.validate()?;
        }
        edit(store.dir(), |peers| {
            let peer = peers
                .values_mut()
                .find(|peer| peer.id == device && peer.receives)
                .ok_or_else(|| {
                    choices::ChoiceError::new("denied", "Pair this device to receive entries.")
                })?;
            if peer.push.as_ref().map(|push| &push.subscription) == subscription.as_ref() {
                return Ok(((), false));
            }
            peer.push = match subscription {
                Some(subscription) => Some(Registration {
                    subscription,
                    log: store.log()?,
                    through: store.last_id()?,
                    retry_at: None,
                }),
                None => None,
            };
            Ok(((), true))
        })
    }

    /// Remove the push subscription when the writer denies the device.
    pub fn clear(store: &Store, device: EndpointId) -> Result<()> {
        edit(store.dir(), |peers| {
            let mut changed = false;
            for peer in peers.values_mut().filter(|peer| peer.id == device) {
                changed |= peer.push.take().is_some();
            }
            Ok(((), changed))
        })
    }

    #[derive(Serialize)]
    struct Notice<'a> {
        writer: String,
        title: String,
        body: &'a str,
        kind: &'a str,
        level: Option<&'a str>,
        entry_id: u64,
        choices: Vec<String>,
    }

    fn payload(
        entry: &Entry,
        writer: String,
        question: Option<&choices::Question>,
    ) -> Result<Vec<u8>> {
        let title = entry.title.as_deref().unwrap_or(&entry.source);
        let mut notice = Notice {
            writer,
            title: title
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect(),
            body: "",
            kind: if question.is_some_and(|q| q.approval) {
                "approval"
            } else {
                &entry.kind
            },
            level: entry.level.as_deref(),
            entry_id: entry.id,
            choices: question.map_or_else(Vec::new, |q| {
                q.options
                    .iter()
                    .map(|choice| {
                        choice
                            .label
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(40)
                            .collect()
                    })
                    .collect()
            }),
        };
        let overhead = serde_json::to_vec(&notice)?.len();
        let mut remaining = MAX_PAYLOAD.saturating_sub(overhead);
        let mut end = 0;
        for c in entry.body.chars() {
            let size = match c {
                '\u{0000}'..='\u{001f}' => 6,
                '"' | '\\' => 2,
                _ => c.len_utf8(),
            };
            if size > remaining {
                break;
            }
            end += c.len_utf8();
            remaining -= size;
        }
        notice.body = &entry.body[..end];
        serde_json::to_vec(&notice).context("Cannot encode the notification.")
    }

    struct Pending {
        subscription: Subscription,
        log: Option<String>,
        entry: u64,
        body: Vec<u8>,
    }

    fn claim(
        store: &Store,
        device: EndpointId,
        saved: &Stored,
        writer: String,
        now: Timestamp,
    ) -> Result<Option<Pending>> {
        if !store.allow_list().contains(&device) {
            return Ok(None);
        }
        // Most feeds have no subscription. Avoid the lock in that case.
        if !peers::list(store.dir())?
            .values()
            .any(|p| p.id == device && p.push.is_some())
        {
            return Ok(None);
        }
        edit(store.dir(), |peers| {
            let Some(push) = peers
                .values_mut()
                .find(|p| p.id == device && p.push.is_some())
                .and_then(|p| p.push.as_mut())
            else {
                return Ok((None, false));
            };
            let log = store.log()?;
            let mut changed = push.log != log;
            if changed {
                push.log = log.clone();
                push.through = 0;
            }
            if saved.log == log && saved.through > push.through {
                push.through = saved.through;
                changed = true;
            }
            if push.retry_at.is_some_and(|retry| retry > now) || store.last_id()? <= push.through {
                return Ok((None, changed));
            }
            let entries = store.entries()?;
            let through = push.through;
            for entry in entries.iter().filter(|entry| entry.id > through) {
                if entry.time + WAIT > now {
                    break;
                }
                let question = choices::question(entry);
                let relevant = if let Some(question) = &question {
                    question.responder == device
                        && choices::get(&entries, &question.id)
                            .is_ok_and(|state| state.status == choices::QuestionStatus::Pending)
                } else {
                    matches!(entry.level.as_deref(), Some("warn" | "alert"))
                        && entry.time + FRESH > now
                };
                if relevant {
                    let body = payload(entry, writer, question.as_ref())?;
                    push.retry_at = Some(now + DEBOUNCE);
                    return Ok((
                        Some(Pending {
                            subscription: push.subscription.clone(),
                            log,
                            entry: entry.id,
                            body,
                        }),
                        true,
                    ));
                }
                push.through = entry.id;
                changed = true;
            }
            Ok((None, changed))
        })
    }

    fn finish(
        store: &Store,
        device: EndpointId,
        pending: &Pending,
        status: StatusCode,
    ) -> Result<()> {
        edit(store.dir(), |peers| {
            let Some(peer) = peers.values_mut().find(|peer| {
                peer.id == device
                    && peer.push.as_ref().is_some_and(|push| {
                        push.subscription == pending.subscription && push.log == pending.log
                    })
            }) else {
                return Ok(((), false));
            };
            match status {
                StatusCode::NOT_FOUND | StatusCode::GONE => peer.push = None,
                code if code.is_success() => {
                    if let Some(push) = &mut peer.push {
                        push.through = push.through.max(pending.entry);
                    }
                }
                _ => return Ok(((), false)),
            }
            Ok(((), true))
        })
    }

    fn client() -> Result<Client> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?)
    }

    async fn send(client: &Client, request: reqwest::Request) -> Result<StatusCode> {
        client
            .execute(request)
            .await
            .map(|response| response.status())
            .map_err(|error| error.without_url().into())
    }

    /// Send a notification if the device did not confirm an eligible entry.
    pub async fn notify(store: &Store, device: EndpointId, saved: &Stored) {
        let result = async {
            let writer = gethostname::gethostname()
                .to_string_lossy()
                .chars()
                .filter(|c| !c.is_control())
                .take(64)
                .collect();
            let Some(pending) = claim(store, device, saved, writer, Timestamp::now())? else {
                return Ok(());
            };
            // Check again after selecting the entry.
            if !store.allow_list().contains(&device) {
                return Ok(());
            }
            let client = client()?;
            let request = pending
                .subscription
                .request(&client, pending.body.clone())?;
            let status = send(&client, request).await?;
            finish(store, device, &pending, status)?;
            if !status.is_success() && !matches!(status, StatusCode::GONE | StatusCode::NOT_FOUND) {
                tracing::debug!(%status, "the push service rejected the notification");
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(%error, "cannot send a notification");
        }
    }

    /// Give the reader time to save the entry before sending a notification.
    pub async fn notify_after_failure(
        store: &Store,
        device: EndpointId,
        saved: &Stored,
        started: tokio::time::Instant,
    ) {
        if peers::list(store.dir()).is_ok_and(|peers| {
            peers
                .values()
                .any(|peer| peer.id == device && peer.push.is_some())
        }) {
            tokio::time::sleep_until(started + Duration::from_secs(5)).await;
            notify(store, device, saved).await;
        }
    }

    #[cfg(test)]
    mod tests {
        use ring::signature::{UnparsedPublicKey as VerifyKey, ECDSA_P256_SHA256_FIXED};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        use super::*;
        use crate::{entry::Draft, EndpointTicket};

        const AUTH: [u8; 16] = [42; 16];
        const ENDPOINT: &str = "https://fcm.googleapis.com/fcm/send/test";

        fn keys() -> (Subscription, EphemeralPrivateKey) {
            let rng = SystemRandom::new();
            let receiver = EphemeralPrivateKey::generate(&ECDH_P256, &rng).unwrap();
            let receiver_key = receiver.compute_public_key().unwrap();
            // P-256 private scalar 1 and its public point, the generator.
            let mut vapid = [0; 32];
            vapid[31] = 1;
            let public = "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296\
                          4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5";
            let public = data_encoding::HEXLOWER.decode(public.as_bytes()).unwrap();
            let b64 = Base64UrlUnpadded::encode_string;
            (
                Subscription {
                    endpoint: ENDPOINT.into(),
                    p256dh: b64(receiver_key.as_ref()),
                    auth: b64(&AUTH),
                    vapid_private: b64(&vapid),
                    vapid_public: b64(&public),
                },
                receiver,
            )
        }

        fn store_with_receiver() -> (tempfile::TempDir, Store, EndpointId) {
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(root.path()).unwrap();
            let device = iroh::SecretKey::generate().public();
            let ticket = EndpointTicket::new(iroh::EndpointAddr::new(device));
            store.allow_list().allow(device).unwrap();
            peers::set_receiver(root.path(), "phone", &ticket).unwrap();
            (root, store, device)
        }

        #[test]
        fn join_accepts_old_messages_and_old_readers_skip_push() {
            #[derive(Serialize, Deserialize)]
            struct OldJoin {
                token: String,
                name: String,
                ticket: Option<String>,
            }
            let old = OldJoin {
                token: "code".into(),
                name: "Phone".into(),
                ticket: None,
            };
            let mut join: crate::pairing::Join =
                serde_json::from_value(serde_json::to_value(old).unwrap()).unwrap();
            assert!(join.push.is_none());
            join.push = Some(keys().0);
            let old: OldJoin = serde_json::from_value(serde_json::to_value(join).unwrap()).unwrap();
            assert_eq!(old.name, "Phone");
        }

        #[tokio::test]
        async fn control_updates_the_subscription_of_the_authenticated_receiver() {
            use crate::{control, net, server::Server};
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(root.path()).unwrap();
            let device = net::bind(
                iroh::SecretKey::generate(),
                net::Network::Local,
                vec![],
                None,
            )
            .await
            .unwrap();
            store.allow_list().allow(device.id()).unwrap();
            let ticket = EndpointTicket::new(device.addr());
            peers::set_receiver(store.dir(), "phone", &ticket).unwrap();
            let server = Server::spawn(
                store.clone(),
                iroh::SecretKey::generate(),
                net::Network::Local,
            )
            .await
            .unwrap();
            let address = server.addr().await;
            let client = control::ControlClient::new(device.clone());
            let subscription = keys().0;
            client
                .set_push(address.clone(), Some(subscription.clone()))
                .await
                .unwrap();
            assert_eq!(
                peers::list(store.dir()).unwrap()["phone"]
                    .push
                    .as_ref()
                    .unwrap()
                    .subscription,
                subscription
            );
            let reply: control::Reply = client
                .exchange(
                    address.clone(),
                    control::ALPN,
                    &serde_json::json!({"type":"unknown"}),
                )
                .await
                .unwrap();
            assert!(
                matches!(reply, control::Reply::Rejected { error } if error.code == "unsupported")
            );
            client.set_push(address, None).await.unwrap();
            assert!(peers::list(store.dir()).unwrap()["phone"].push.is_none());
            client.close().await;
            device.close().await;
            server.shutdown().await;
        }

        fn post(store: &Store, level: &str) -> Entry {
            store
                .append(Draft {
                    source: "build".into(),
                    kind: "event".into(),
                    level: Some(level.into()),
                    subject: None,
                    title: Some("Build failed".into()),
                    body: "🚧\n".repeat(5000),
                    format: None,
                    data: None,
                    extra: Default::default(),
                })
                .unwrap()
        }

        #[test]
        fn accepts_only_push_service_endpoints() {
            for value in [
                ENDPOINT,
                "https://updates.push.services.mozilla.com/wpush/v2/test",
                "https://web.push.apple.com/QT123",
                "https://db5p.notify.windows.com/w/?token=test",
            ] {
                assert!(endpoint(value).is_ok(), "{value}");
            }
            for value in [
                "http://fcm.googleapis.com/fcm/send/test",
                "https://fcm.googleapis.com.evil.test/test",
                "https://evilpush.apple.com/test",
                "https://evilnotify.windows.com/test",
                "https://user@fcm.googleapis.com/test",
                "https://fcm.googleapis.com:8080/test",
                "https://127.0.0.1/test",
                "https://fcm.googleapis.com/test#other",
            ] {
                assert!(endpoint(value).is_err(), "{value}");
            }
        }

        #[test]
        fn skips_saved_and_old_entries_and_limits_attempts() {
            let (_dir, store, device) = store_with_receiver();
            let saved_before_opt_in = post(&store, "warn");
            set(&store, device, Some(keys().0)).unwrap();
            let now = Timestamp::now() + WAIT;
            let empty = Stored::default();
            assert!(claim(&store, device, &empty, "Writer".into(), now)
                .unwrap()
                .is_none());

            let info = post(&store, "info");
            let now = Timestamp::now() + WAIT;
            assert!(claim(&store, device, &empty, "Writer".into(), now)
                .unwrap()
                .is_none());
            let warn = post(&store, "warn");
            let saved = Stored {
                through: warn.id,
                log: store.log().unwrap(),
            };
            assert!(claim(
                &store,
                device,
                &saved,
                "Writer".into(),
                Timestamp::now() + WAIT
            )
            .unwrap()
            .is_none());

            let alert = post(&store, "alert");
            assert!(
                claim(&store, device, &saved, "Writer".into(), Timestamp::now())
                    .unwrap()
                    .is_none()
            );
            let now = Timestamp::now() + WAIT;
            let pending = claim(&store, device, &saved, "Writer".into(), now)
                .unwrap()
                .unwrap();
            assert_eq!(pending.entry, alert.id);
            assert!(alert.id > info.id && info.id > saved_before_opt_in.id);
            assert!(pending.body.len() <= MAX_PAYLOAD);
            assert!(claim(&store, device, &saved, "Writer".into(), now)
                .unwrap()
                .is_none());
            finish(&store, device, &pending, StatusCode::CREATED).unwrap();
            assert!(
                claim(&store, device, &saved, "Writer".into(), now + DEBOUNCE)
                    .unwrap()
                    .is_none()
            );
        }

        #[test]
        fn sends_only_open_questions_for_this_device() {
            let (_dir, store, device) = store_with_receiver();
            set(&store, device, Some(keys().0)).unwrap();
            let expires = Timestamp::now() + SignedDuration::from_secs(600);
            let add = |responder| {
                let question = choices::new_question(
                    "Deploy?".into(),
                    "Run the release".into(),
                    "build".into(),
                    vec![
                        choices::Choice {
                            value: "approve".into(),
                            label: "Approve".into(),
                        },
                        choices::Choice {
                            value: "reject".into(),
                            label: "Reject".into(),
                        },
                    ],
                    responder,
                    Some(expires),
                    true,
                );
                choices::create_with(&store, question, Default::default()).unwrap()
            };
            add(iroh::SecretKey::generate().public());
            let cancelled = add(device);
            choices::cancel(&store, &cancelled.id).unwrap();
            add(device);
            let pending = claim(
                &store,
                device,
                &Stored::default(),
                "Writer".into(),
                Timestamp::now() + WAIT,
            )
            .unwrap()
            .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&pending.body).unwrap();
            assert_eq!(pending.entry, store.last_id().unwrap());
            assert_eq!(body["kind"], "approval");
            assert_eq!(body["choices"], serde_json::json!(["Approve", "Reject"]));
        }

        async fn round_trip(status: &str) {
            let (_dir, store, device) = store_with_receiver();
            let (subscription, receiver) = keys();
            set(&store, device, Some(subscription.clone())).unwrap();
            let entry = post(&store, "alert");
            let now = Timestamp::now() + WAIT;
            let pending = claim(&store, device, &Stored::default(), "Writer".into(), now)
                .unwrap()
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url =
                Url::parse(&format!("http://{}/push", listener.local_addr().unwrap())).unwrap();
            let client = client().unwrap();
            let mut request = subscription.request(&client, pending.body.clone()).unwrap();
            // Keep the production allow list in use. Redirect only this test request.
            *request.url_mut() = url;
            let server = async {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end(), "POST /push HTTP/1.1");
                let mut headers = BTreeMap::new();
                loop {
                    line.clear();
                    stream.read_line(&mut line).await.unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                }
                assert_eq!(headers["content-encoding"], "aes128gcm");
                assert_eq!(headers["ttl"], "60");
                let mut encrypted = vec![0; headers["content-length"].parse().unwrap()];
                stream.read_exact(&mut encrypted).await.unwrap();
                assert!(encrypted.len() < 4096);
                let sender = &encrypted[21..21 + usize::from(encrypted[20])];
                let receiver_public = receiver.compute_public_key().unwrap();
                let ikm = agreement::agree_ephemeral(
                    receiver,
                    &UnparsedPublicKey::new(&ECDH_P256, sender),
                    |shared| content_key(&AUTH, shared, receiver_public.as_ref(), sender),
                )
                .unwrap()
                .unwrap();
                let body = ece_native::decrypt(ikm, encrypted).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["entry_id"], entry.id);
                assert_eq!(body["title"], "Build failed");
                assert_eq!(body["level"], "alert");
                assert!(body["body"].as_str().unwrap().starts_with("🚧\n"));
                let (token, public) = headers["authorization"]
                    .strip_prefix("vapid t=")
                    .unwrap()
                    .split_once(", k=")
                    .unwrap();
                assert_eq!(public, subscription.vapid_public);
                let (signed, signature) = token.rsplit_once('.').unwrap();
                let signature = Base64UrlUnpadded::decode_vec(signature).unwrap();
                let public = Base64UrlUnpadded::decode_vec(public).unwrap();
                VerifyKey::new(&ECDSA_P256_SHA256_FIXED, public)
                    .verify(signed.as_bytes(), &signature)
                    .unwrap();
                let claims = signed.split_once('.').unwrap().1;
                let claims: serde_json::Value =
                    serde_json::from_slice(&Base64UrlUnpadded::decode_vec(claims).unwrap())
                        .unwrap();
                assert_eq!(claims["aud"], "https://fcm.googleapis.com");
                assert!(claims["exp"].as_i64().unwrap() > Timestamp::now().as_second());
                stream
                    .get_mut()
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            };
            let (result, ()) = tokio::join!(send(&client, request), server);
            let code = result.unwrap();
            finish(&store, device, &pending, code).unwrap();
            let peer = peers::list(store.dir()).unwrap().remove("phone").unwrap();
            if code.is_success() {
                assert_eq!(peer.push.unwrap().through, entry.id);
            } else {
                assert!(peer.push.is_none());
            }
        }

        #[tokio::test]
        async fn sends_encrypted_push_and_clears_gone_subscriptions() {
            round_trip("201 Created").await;
            round_trip("404 Not Found").await;
            round_trip("410 Gone").await;
        }
    }
}
