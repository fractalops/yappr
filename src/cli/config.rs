//! Read or change the settings of a local feed.
use anyhow::Result;
use yappr::{
    config::{FeedConfig, HookSettings},
    hooks,
    net::RelaySettings,
    peers,
};

use super::{
    args::{parse_interval, CommonArgs, Format, HookConfigArgs},
    Usage,
};

pub(super) fn run(
    common: &CommonArgs,
    name: Option<String>,
    default_relays: bool,
    mdns: bool,
    hook_args: HookConfigArgs,
    output: Format,
) -> Result<()> {
    let relay_override = common.no_relay || !common.relays.is_empty();
    if default_relays && relay_override {
        return Err(Usage("Use --default-relays, --relay, or --no-relay.".into()).into());
    }
    if mdns && common.no_mdns {
        return Err(Usage("Use --mdns or --no-mdns.".into()).into());
    }
    let dir = common.dir()?;
    let changes_hooks = hook_args.any();
    let config = if name.is_some()
        || relay_override
        || default_relays
        || mdns
        || common.no_mdns
        || changes_hooks
    {
        FeedConfig::update(&dir, |config| {
            if changes_hooks {
                update_hooks(&dir, &mut config.hooks, hook_args)?;
            }
            if let Some(name) = name {
                config.set_name(&name)?;
            }
            if relay_override || default_relays {
                config.relays = RelaySettings {
                    disabled: common.no_relay,
                    urls: common.relays.clone(),
                };
            }
            if mdns || common.no_mdns {
                config.mdns = mdns;
            }
            Ok(())
        })?
    } else {
        FeedConfig::load(&dir)?
    };

    match output {
        Format::Json => println!("{}", serde_json::to_string(&config)?),
        Format::Text => {
            let name = config.writer_name();
            if config.name.is_some() {
                println!("Writer name: {name}");
            } else {
                println!("Writer name: {name} (computer name)");
            }
            let relays = if config.relays.disabled {
                "Off".to_owned()
            } else if config.relays.urls.is_empty() {
                "Default".to_owned()
            } else {
                config.relays.urls.join(", ")
            };
            println!("Relays: {relays}");
            println!(
                "Local discovery: {}",
                if config.mdns { "On" } else { "Off" }
            );
            let hooks = &config.hooks;
            println!(
                "Agent approvals go to: {}",
                hooks
                    .to
                    .as_deref()
                    .unwrap_or("the one device that receives this feed")
            );
            let wait = hooks
                .timeout
                .map_or(hooks::DEFAULT_WAIT, std::time::Duration::from_secs);
            println!(
                "Agent approvals wait: {}",
                jiff::SignedDuration::try_from(wait)
                    .map(|d| format!("{d:#}"))
                    .unwrap_or_default()
            );
            println!(
                "Agent events: {}",
                hooks.events.as_deref().unwrap_or("permission,stop")
            );
            println!(
                "Agent hooks act: {}",
                match hooks.when.unwrap_or_default() {
                    yappr::config::HookWhen::Away =>
                        "when you are away (screen locked, or idle for 2 minutes)",
                    yappr::config::HookWhen::Always => "always",
                }
            );
        }
    }
    Ok(())
}

/// Apply the hook options of `yappr config`. An empty value restores the
/// default.
fn update_hooks(
    dir: &std::path::Path,
    settings: &mut HookSettings,
    args: HookConfigArgs,
) -> Result<()> {
    let set =
        |value: Option<String>| value.map(|v| Some(v.trim().to_owned()).filter(|v| !v.is_empty()));
    if let Some(to) = set(args.hook_to) {
        if let Some(device) = &to {
            peers::resolve(dir, device)?;
        }
        settings.to = to;
    }
    if let Some(timeout) = set(args.hook_timeout) {
        settings.timeout = match timeout {
            Some(text) => {
                let wait =
                    parse_interval(&text).map_err(|e| Usage(format!("--hook-timeout: {e}")))?;
                if wait > hooks::MAX_WAIT {
                    return Err(Usage("--hook-timeout can be at most 59m".into()).into());
                }
                Some(wait.as_secs().max(1))
            }
            None => None,
        };
    }
    if let Some(when) = set(args.hook_when) {
        settings.when = match when {
            Some(text) => Some(
                <yappr::config::HookWhen as clap::ValueEnum>::from_str(&text, true)
                    .map_err(|_| Usage("--hook-when: use away or always".into()))?,
            ),
            None => None,
        };
    }
    if let Some(events) = set(args.hook_events) {
        if let Some(text) = &events {
            hooks::parse_events(text).map_err(|e| Usage(format!("--hook-events: {e:#}")))?;
        }
        settings.events = events;
    }
    Ok(())
}
