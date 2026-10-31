//! Private feeds, questions, and choices between your devices over iroh.
//!
//! A browser build (`wasm_browser`) has the modules that a reader needs. The
//! writer, the file storage, and the terminal output are native only.
pub mod choices;
pub mod client;
pub mod control;
pub mod entry;
pub mod health;
pub mod net;
pub mod pairing;
pub mod proto;

#[cfg(not(wasm_browser))]
pub mod access;
#[cfg(not(wasm_browser))]
pub mod cursor;
#[cfg(not(wasm_browser))]
pub mod disk;
#[cfg(not(wasm_browser))]
pub mod ingest;
#[cfg(not(wasm_browser))]
pub mod outbox;
#[cfg(not(wasm_browser))]
pub mod peers;
#[cfg(not(wasm_browser))]
pub mod render;
#[cfg(not(wasm_browser))]
pub mod server;
#[cfg(not(wasm_browser))]
pub mod store;

/// The feed protocol identifier.
///
/// A change to the protocol uses a new ALPN, so that both versions can run on
/// one endpoint at the same time.
pub const ALPN: &[u8] = b"feed/0";

pub use iroh_tickets::endpoint::EndpointTicket;
