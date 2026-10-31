//! Private feeds, questions, and choices between your devices over iroh.
pub mod access;
pub mod choices;
pub mod client;
pub mod control;
pub mod cursor;
pub mod disk;
pub mod entry;
pub mod health;
pub mod ingest;
pub mod net;
pub mod outbox;
pub mod pairing;
pub mod peers;
pub mod proto;
pub mod render;
pub mod server;
pub mod store;

/// The feed protocol identifier.
///
/// A change to the protocol uses a new ALPN, so that both versions can run on
/// one endpoint at the same time.
pub const ALPN: &[u8] = b"feed/0";

pub use iroh_tickets::endpoint::EndpointTicket;
