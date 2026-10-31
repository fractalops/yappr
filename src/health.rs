//! Connection state from iroh's selected QUIC path.
use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub state: String,
    pub route: Option<String>,
    pub address: Option<String>,
    pub rtt_ms: Option<f64>,
    pub error: Option<String>,
    pub retry_in_ms: Option<u64>,
}
impl Default for Health {
    fn default() -> Self {
        Self::state("connecting")
    }
}
impl Health {
    pub fn state(state: &str) -> Self {
        Self {
            state: state.into(),
            route: None,
            address: None,
            rtt_ms: None,
            error: None,
            retry_in_ms: None,
        }
    }
    pub fn failure(state: &str, error: String, retry_in_ms: Option<u64>) -> Self {
        Self {
            error: Some(error),
            retry_in_ms,
            ..Self::state(state)
        }
    }
}
pub fn snapshot(conn: &Connection) -> Health {
    if let Some(error) = conn.close_reason() {
        return Health::failure("disconnected", error.to_string(), None);
    }
    let paths = conn.paths();
    let path = paths.iter().find(|path| path.is_selected());
    match path {
        Some(path) => Health {
            route: Some(
                if path.is_relay() {
                    "relay"
                } else if path.is_ip() {
                    "direct"
                } else {
                    "other"
                }
                .into(),
            ),
            address: Some(path.remote_addr().to_string()),
            rtt_ms: Some((path.rtt().as_secs_f64() * 100_000.0).round() / 100.0),
            ..Health::state("connected")
        },
        None => Health::state("connected"),
    }
}
