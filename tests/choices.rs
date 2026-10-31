//! Decision and pairing checks use real iroh endpoints on loopback.
use std::{
    sync::{Arc, Barrier},
    time::Duration,
};

use iroh::{Endpoint, SecretKey};
use jiff::Timestamp;
use yappr::{
    choices::{self, Choice, ChoiceError, Submission},
    control, disk,
    net::{self, Network},
    outbox,
    pairing::{self, Join, PairReply},
    server::Server,
    store::Store,
    EndpointTicket,
};

fn options() -> Vec<Choice> {
    vec![
        Choice {
            value: "retry".into(),
            label: "Retry now".into(),
        },
        Choice {
            value: "skip".into(),
            label: "Skip".into(),
        },
    ]
}
fn question(store: &Store, responder: iroh::EndpointId) -> choices::Question {
    choices::create(
        store,
        choices::new_question(
            "What next?".into(),
            "Backup failed".into(),
            "backup".into(),
            options(),
            responder,
            None,
            false,
        ),
    )
    .unwrap()
}
fn submission(question_id: &str, value: &str) -> Submission {
    Submission {
        id: disk::random_id(),
        question_id: question_id.into(),
        value: value.into(),
    }
}
fn code(error: anyhow::Error) -> String {
    error.downcast_ref::<ChoiceError>().unwrap().code.clone()
}
async fn endpoint() -> Endpoint {
    net::bind(SecretKey::generate(), Network::Local, vec![], None)
        .await
        .unwrap()
}

#[test]
fn readers_cannot_answer_another_devices_question() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let responder = SecretKey::generate().public();
    let reader = SecretKey::generate().public();
    store.allow_list().allow(responder).unwrap();
    store.allow_list().allow(reader).unwrap();
    let q = question(&store, responder);
    assert_eq!(
        code(choices::decide(&store, reader, submission(&q.id, "retry")).unwrap_err()),
        "denied"
    );
    assert_eq!(
        code(choices::decide(&store, responder, submission(&q.id, "invented")).unwrap_err()),
        "invalid"
    );
    assert_eq!(store.last_id().unwrap(), 1);
    store.allow_list().deny(&responder).unwrap();
    assert_eq!(
        code(choices::decide(&store, responder, submission(&q.id, "retry")).unwrap_err()),
        "denied"
    );
}

#[test]
fn decision_is_durable_and_retries_do_not_append_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let responder = SecretKey::generate().public();
    store.allow_list().allow(responder).unwrap();
    let q = question(&store, responder);
    let s = submission(&q.id, "retry");
    let decision = choices::decide(&store, responder, s.clone()).unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(choices::decide(&reopened, responder, s).unwrap(), decision);
    assert_eq!(reopened.last_id().unwrap(), 2);
    assert_eq!(
        code(choices::decide(&reopened, responder, submission(&q.id, "skip")).unwrap_err()),
        "answered"
    );
    assert!(choices::cancel(&reopened, &q.id).is_err());
}

#[test]
fn simultaneous_answers_have_one_final_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let responder = SecretKey::generate().public();
    store.allow_list().allow(responder).unwrap();
    let q = question(&store, responder);
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let id = q.id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                choices::decide(&store, responder, submission(&id, "retry")).is_ok()
            })
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .map(|w| w.join().expect("a worker panicked"))
            .filter(|ok| *ok)
            .count(),
        1
    );
    assert_eq!(store.last_id().unwrap(), 2);
}

#[test]
fn cancellation_and_expiry_reject_late_answers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let responder = SecretKey::generate().public();
    store.allow_list().allow(responder).unwrap();
    let cancelled = question(&store, responder);
    choices::cancel(&store, &cancelled.id).unwrap();
    choices::cancel(&store, &cancelled.id).unwrap();
    assert_eq!(
        code(choices::decide(&store, responder, submission(&cancelled.id, "retry")).unwrap_err()),
        "cancelled"
    );
    let mut q = choices::new_question(
        "Continue?".into(),
        "".into(),
        "test".into(),
        options(),
        responder,
        Some(
            Timestamp::now()
                .checked_add(jiff::SignedDuration::from_millis(400))
                .unwrap(),
        ),
        false,
    );
    q = choices::create(&store, q).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        code(choices::decide(&store, responder, submission(&q.id, "retry")).unwrap_err()),
        "expired"
    );
}

#[test]
fn invalid_questions_do_not_change_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let responder = SecretKey::generate().public();
    let mut q = choices::new_question(
        "Continue?".into(),
        "".into(),
        "test".into(),
        options(),
        responder,
        None,
        false,
    );
    q.options[1].value = q.options[0].value.clone();
    assert!(choices::create(&store, q).is_err());
    assert_eq!(store.last_id().unwrap(), 0);
}

#[tokio::test]
async fn revoked_device_receives_a_final_rejection() {
    let writer_dir = tempfile::tempdir().unwrap();
    let device_dir = tempfile::tempdir().unwrap();
    let store = Store::open(writer_dir.path()).unwrap();
    let server = Server::spawn(store.clone(), SecretKey::generate(), Network::Local)
        .await
        .unwrap();
    let device = endpoint().await;
    store.allow_list().allow(device.id()).unwrap();
    let q = question(&store, device.id());
    let ticket = EndpointTicket::new(server.addr().await);
    let delivery = outbox::queue(device_dir.path(), &ticket, q.id, "retry".into()).unwrap();
    store.allow_list().deny(&device.id()).unwrap();
    assert_eq!(
        code(
            outbox::deliver(device_dir.path(), &device, &delivery)
                .await
                .unwrap_err()
        ),
        "denied"
    );
    assert_eq!(
        outbox::list(device_dir.path()).unwrap()[0].status,
        "rejected"
    );
    assert_eq!(store.last_id().unwrap(), 1);
    device.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn retry_after_a_lost_outbox_update_and_writer_restart_keeps_one_decision() {
    let writer_dir = tempfile::tempdir().unwrap();
    let device_dir = tempfile::tempdir().unwrap();
    let store = Store::open(writer_dir.path()).unwrap();
    let secret = SecretKey::generate();
    let server = Server::spawn(store.clone(), secret.clone(), Network::Local)
        .await
        .unwrap();
    let device = endpoint().await;
    store.allow_list().allow(device.id()).unwrap();
    let q = question(&store, device.id());
    let ticket = EndpointTicket::new(server.addr().await);
    let queued = outbox::queue(device_dir.path(), &ticket, q.id.clone(), "retry".into()).unwrap();
    // The writer commits. Pretend the device loses the response before it updates its outbox.
    let first = control::submit(&device, server.addr().await, queued.submission.clone())
        .await
        .unwrap();
    server.shutdown().await;
    let server = Server::spawn(store.clone(), secret, Network::Local)
        .await
        .unwrap();
    let mut queued = outbox::list(device_dir.path()).unwrap().remove(0);
    queued.ticket = EndpointTicket::new(server.addr().await).to_string();
    let confirmed = outbox::deliver(device_dir.path(), &device, &queued)
        .await
        .unwrap();
    assert_eq!(first, confirmed);
    assert_eq!(store.last_id().unwrap(), 2);
    assert_eq!(
        outbox::list(device_dir.path()).unwrap()[0].status,
        "delivered"
    );
    assert!(outbox::queue(device_dir.path(), &ticket, q.id, "skip".into()).is_err());
    device.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn pairing_needs_writer_confirmation_and_binds_one_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let server = Server::spawn(store.clone(), SecretKey::generate(), Network::Local)
        .await
        .unwrap();
    let device = endpoint().await;
    let stranger = endpoint().await;
    let invitation = pairing::create(
        &store,
        EndpointTicket::new(server.addr().await).to_string(),
        "Laptop".into(),
        Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(60))
            .unwrap(),
    )
    .unwrap();
    let decoded = pairing::Invitation::decode(&invitation.encode().unwrap()).unwrap();
    assert_eq!(decoded.token, invitation.token);
    let join = Join {
        token: invitation.token.clone(),
        name: "My device".into(),
    };
    let pending: PairReply = control::exchange(&device, server.addr().await, pairing::ALPN, &join)
        .await
        .unwrap();
    assert_eq!(pending.status, "pending");
    assert!(!store.allow_list().contains(&device.id()));
    let rejected: PairReply =
        control::exchange(&stranger, server.addr().await, pairing::ALPN, &join)
            .await
            .unwrap();
    assert_eq!(rejected.status, "rejected");
    pairing::confirm(&store, device.id(), true).unwrap();
    let accepted: PairReply = control::exchange(&device, server.addr().await, pairing::ALPN, &join)
        .await
        .unwrap();
    assert_eq!(accepted.status, "accepted");
    assert!(store.allow_list().contains(&device.id()));
    assert!(!store.allow_list().contains(&stranger.id()));
    let invalid: PairReply = control::exchange(
        &device,
        server.addr().await,
        pairing::ALPN,
        &Join {
            token: disk::random_id(),
            name: "Device".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(invalid.status, "rejected");
    device.close().await;
    stranger.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_devices_reach_pairing_only_while_an_invitation_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let server = Server::spawn(store.clone(), SecretKey::generate(), Network::Local)
        .await
        .unwrap();
    let device = endpoint().await;
    let stranger = endpoint().await;
    let join = |token: &str| Join {
        token: token.into(),
        name: "Device".into(),
    };
    let closed = control::exchange::<_, PairReply>(
        &stranger,
        server.addr().await,
        pairing::ALPN,
        &join(&disk::random_id()),
    )
    .await
    .unwrap_err();
    assert_eq!(code(closed), "denied");

    let invitation = pairing::create(
        &store,
        EndpointTicket::new(server.addr().await).to_string(),
        "Laptop".into(),
        Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(60))
            .unwrap(),
    )
    .unwrap();
    let pending: PairReply = control::exchange(
        &device,
        server.addr().await,
        pairing::ALPN,
        &join(&invitation.token),
    )
    .await
    .unwrap();
    assert_eq!(pending.status, "pending");

    // An accepted invitation closes the pairing protocol to unknown devices.
    pairing::confirm(&store, device.id(), true).unwrap();
    let after = endpoint().await;
    let closed = control::exchange::<_, PairReply>(
        &after,
        server.addr().await,
        pairing::ALPN,
        &join(&invitation.token),
    )
    .await
    .unwrap_err();
    assert_eq!(code(closed), "denied");
    device.close().await;
    stranger.close().await;
    after.close().await;
    server.shutdown().await;
}

#[test]
fn expired_and_rejected_invitations_do_not_grant_access() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writer = SecretKey::generate().public();
    let device = SecretKey::generate().public();
    let ticket = EndpointTicket::new(iroh::EndpointAddr::new(writer)).to_string();
    let expired = pairing::create(
        &store,
        ticket.clone(),
        "Laptop".into(),
        Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_secs(1))
            .unwrap(),
    )
    .unwrap();
    assert!(pairing::join(
        &store,
        device,
        Join {
            token: expired.token,
            name: "Device".into()
        }
    )
    .is_err());
    let invite = pairing::create(
        &store,
        ticket,
        "Laptop".into(),
        Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(60))
            .unwrap(),
    )
    .unwrap();
    pairing::join(
        &store,
        device,
        Join {
            token: invite.token.clone(),
            name: "Device".into(),
        },
    )
    .unwrap();
    pairing::confirm(&store, device, false).unwrap();
    assert_eq!(
        pairing::join(
            &store,
            device,
            Join {
                token: invite.token,
                name: "Device".into()
            }
        )
        .unwrap()
        .status,
        "rejected"
    );
    assert!(!store.allow_list().contains(&device));
}

#[tokio::test]
async fn identical_question_ids_on_two_writers_keep_separate_deliveries() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let device_dir = tempfile::tempdir().unwrap();
    let sa = Store::open(a.path()).unwrap();
    let sb = Store::open(b.path()).unwrap();
    let device = endpoint().await;
    sa.allow_list().allow(device.id()).unwrap();
    sb.allow_list().allow(device.id()).unwrap();
    let q = question(&sa, device.id());
    choices::create(&sb, q.clone()).unwrap();
    let server_a = Server::spawn(sa.clone(), SecretKey::generate(), Network::Local)
        .await
        .unwrap();
    let server_b = Server::spawn(sb.clone(), SecretKey::generate(), Network::Local)
        .await
        .unwrap();
    let ta = EndpointTicket::new(server_a.addr().await);
    let tb = EndpointTicket::new(server_b.addr().await);
    let da = outbox::queue(device_dir.path(), &ta, q.id.clone(), "retry".into()).unwrap();
    let db = outbox::queue(device_dir.path(), &tb, q.id.clone(), "skip".into()).unwrap();
    assert_ne!(da.submission.id, db.submission.id);
    let client = control::ControlClient::new(device.clone());
    outbox::deliver_with(device_dir.path(), &client, &da)
        .await
        .unwrap();
    outbox::deliver_with(device_dir.path(), &client, &db)
        .await
        .unwrap();
    assert_eq!(
        choices::get(&sa.entries().unwrap(), &q.id)
            .unwrap()
            .decision
            .unwrap()
            .value,
        "retry"
    );
    assert_eq!(
        choices::get(&sb.entries().unwrap(), &q.id)
            .unwrap()
            .decision
            .unwrap()
            .value,
        "skip"
    );
    assert_eq!(
        outbox::list(device_dir.path())
            .unwrap()
            .iter()
            .filter(|d| d.status == "delivered")
            .count(),
        2
    );
    client.close().await;
    device.close().await;
    server_a.shutdown().await;
    server_b.shutdown().await;
}
