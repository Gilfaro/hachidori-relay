// SPDX-License-Identifier: GPL-3.0-or-later
//! Host mode: the relay is itself the sharing Hachidori.
//!
//! Linked browsers on `/link` are answered by [`local::Local`], never by a browser.
//! A browser that connects on `/host` is only a source: [`sync::Sync`] mirrors its
//! shared storage and dictionaries into the data directory ([`store::Store`]), and
//! what it mirrors is what linked browsers then see. Everything persists, so the
//! relay keeps sharing after that browser closes and after a restart.
//!
//! - [`store`]: the data directory, plain JSON plus the installed dictionaries.
//! - [`archive`]: unpacks the one-dictionary archives a browser host serves.
//! - [`sync`]: the pull from a browser host.
//! - [`local`]: the sharing host linked browsers talk to.
//! - [`engine`]: the native dictionary engine, answering lookups as the extension's does.
//! - [`lookup`]: what the extension adds to the engine's replies (tags, furigana).
//! - [`yomitan`]: the host side of the Yomitan-compatible API.
//! - [`anki`]: Anki over AnkiConnect, and the duplicate index behind word status.
//! - [`settings`]: the service worker's settings rules and storage requests.
//! - [`js`]: JavaScript's semantics for the values those rules read.

/// A JSON object whose values are moved in, each through `Value::from`. `json!`
/// copies every value it is given (`to_value(&value)`), which for a lookup's
/// results is a deep copy of a tree of a hundred thousand nodes or more.
macro_rules! owned_json {
    ({ $($key:literal: $value:expr),* $(,)? }) => {{
        #[allow(unused_mut)]
        let mut fields = serde_json::Map::new();
        $( fields.insert(($key).to_owned(), serde_json::Value::from($value)); )*
        serde_json::Value::Object(fields)
    }};
}

mod anki;
mod archive;
mod engine;
mod js;
mod local;
mod lookup;
mod remote;
mod settings;
mod setup;
mod store;
mod sync;
#[cfg(test)]
mod tests;
mod updates;
mod upload;
mod yomitan;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::relay::Shared;

/// How host mode is set up (see [`crate::Builder::host`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostOptions {
    /// Where the configuration (`storage.json`) and the dictionaries are kept.
    pub data_dir: PathBuf,
    /// Accept linked browsers from the network as well as from this computer.
    pub network: bool,
    /// Write `storage.json` at most this often, with every change made meanwhile;
    /// zero writes each change at once. A new dictionary list is always written at
    /// once. Pending changes are written when the relay shuts down.
    pub write_delay: Duration,
}

impl HostOptions {
    /// Host mode keeping its data in `data_dir`, listening on this computer only.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), network: false, write_delay: Duration::from_secs(5) }
    }

    /// The platform's data directory for the relay: `%APPDATA%\hachidori-relay` on
    /// Windows, `$XDG_DATA_HOME/hachidori-relay` (or `~/.local/share/hachidori-relay`)
    /// elsewhere. `None` when the environment names no such place.
    #[must_use]
    pub fn default_data_dir() -> Option<PathBuf> {
        let from = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        let base = if cfg!(windows) { from("APPDATA") } else { from("XDG_DATA_HOME").or_else(|| Some(from("HOME")?.join(".local/share"))) };
        Some(base?.join("hachidori-relay"))
    }
}

/// Opens the data directory, installs the relay's own host and starts the pull.
pub(crate) fn start(
    relay: &Arc<Shared>,
    options: &HostOptions,
    version: &str,
    lookup_timeout: Duration,
    chunk_timeout: Duration,
    stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let store = Arc::new(store::Store::open_with(&options.data_dir, options.write_delay)?);
    tracing::info!(data_dir = %store.root().display(), "host mode");
    let local = local::Local::new(relay, store.clone(), version);
    relay.set_local(local.clone());
    local.start();
    let sync = Arc::new(sync::Sync { relay: relay.clone(), store, local, version: version.to_owned(), lookup_timeout, chunk_timeout });
    tokio::spawn(sync.run(stop));
    if options.network {
        let relay = relay.clone();
        tokio::spawn(async move {
            if let Some(error) = relay.switch_network(true).await {
                tracing::warn!(%error, "host mode could not listen on the network");
            }
        });
    }
    Ok(())
}
