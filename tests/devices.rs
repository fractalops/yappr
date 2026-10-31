//! Device aliases must keep identity boundaries.
use iroh::{EndpointAddr, SecretKey};
use yappr::{disk, peers, store::Store, EndpointTicket};

fn ticket() -> EndpointTicket {
    EndpointTicket::new(EndpointAddr::new(SecretKey::generate().public()))
}
#[test]
fn aliases_preserve_identity_and_ticket_without_granting_access() {
    let dir = tempfile::tempdir().unwrap();
    let first = ticket();
    let id = first.endpoint_addr().id;
    peers::set(dir.path(), "laptop", &first.to_string()).unwrap();
    peers::set(dir.path(), "laptop", &id.to_string()).unwrap();
    assert_eq!(
        peers::resolve(dir.path(), "laptop").unwrap(),
        *first.endpoint_addr()
    );
    assert!(!Store::open(dir.path()).unwrap().allow_list().contains(&id));
    assert!(peers::set(dir.path(), "laptop", &ticket().to_string()).is_err());
    peers::remove(dir.path(), "laptop").unwrap();
    assert!(peers::resolve(dir.path(), "laptop").is_err());
    for name in [
        "",
        "CAPS",
        "bad name",
        "bad\nname",
        "-dash",
        "123",
        "../bad",
    ] {
        assert!(peers::set(dir.path(), name, &id.to_string()).is_err());
    }
}
#[test]
fn corrupt_alias_ticket_cannot_redirect_a_request() {
    let dir = tempfile::tempdir().unwrap();
    disk::write(&dir.path().join("peers.json"), &serde_json::json!({"device": {"id": SecretKey::generate().public(), "ticket": ticket().to_string()}})).unwrap();
    assert!(peers::resolve(dir.path(), "device").is_err());
}
