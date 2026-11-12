//! Settings for one local feed.
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{disk, net::RelaySettings, writer_name};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedConfig {
    pub name: Option<String>,
    pub relays: RelaySettings,
    pub mdns: bool,
    pub hooks: HookSettings,
}

/// The settings of `yappr hook`, for the agent plugins. An unset value uses
/// the default of the hook.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookSettings {
    /// The device that answers approvals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// How long an approval waits, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// The selected events, as `event[:matcher[:command-pattern]]` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<String>,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            name: None,
            relays: RelaySettings::default(),
            mdns: true,
            hooks: HookSettings::default(),
        }
    }
}

impl FeedConfig {
    pub fn load(dir: &Path) -> Result<Self> {
        let config: Self = disk::read(&dir.join("config.json"))?;
        config.relays.validate()?;
        Ok(config)
    }

    pub fn update(dir: &Path, change: impl FnOnce(&mut Self) -> Result<()>) -> Result<Self> {
        let _lock = disk::lock(dir, "config.lock")?;
        let mut config = Self::load(dir)?;
        change(&mut config)?;
        config.relays.validate()?;
        disk::write(&dir.join("config.json"), &config)?;
        Ok(config)
    }

    pub fn set_name(&mut self, name: &str) -> Result<()> {
        self.name = writer_name::validate(name)?;
        Ok(())
    }

    pub fn writer_name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| writer_name::platform_name())
    }
}
