// SPDX-License-Identifier: GPL-3.0-or-later
//! Pulls a browser host's configuration and dictionaries into the store.
//!
//! While a browser host is connected at `/host`, one in-process client session talks
//! to it as the API session does. Its `hello` snapshot and every `storage` broadcast
//! overwrite the relay's copy (the browser's values win). A changed `dictionaryState`
//! is held back until every generation it names is installed here: each missing one
//! is downloaded with the host contract's `hd_api_dictionary_*` frames and unpacked,
//! and only then is the new list stored and published to linked browsers, so they
//! never see a dictionary the relay cannot read. A failed download is retried.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

use super::archive;
use super::local::Local;
use super::store::{self, Store};
use crate::api::routes::{self, Opened};
use crate::api::session::{HostNews, Session};
use crate::relay::Shared;

/// How long a failed dictionary download waits before it is tried again.
const RETRY: Duration = Duration::from_secs(30);

/// The pull side of host mode.
pub(crate) struct Sync {
    pub(crate) relay: Arc<Shared>,
    pub(crate) store: Arc<Store>,
    pub(crate) local: Arc<Local>,
    pub(crate) version: String,
    pub(crate) lookup_timeout: Duration,
    pub(crate) chunk_timeout: Duration,
}

impl Sync {
    /// Follows every browser host that connects, until `stop`.
    pub(crate) async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let mut browser = self.relay.browser_host();
        loop {
            let host = *browser.borrow_and_update();
            if let Some(host) = host {
                self.follow(host, &mut browser, &mut stop).await;
            } else {
                tokio::select! {
                    () = crate::until_true(&mut stop) => return,
                    changed = browser.changed() => if changed.is_err() { return },
                }
            }
            if *stop.borrow() {
                return;
            }
        }
    }

    /// Mirrors one browser host connection until it goes.
    async fn follow(self: &Arc<Self>, host: u64, browser: &mut watch::Receiver<Option<u64>>, stop: &mut watch::Receiver<bool>) {
        let still_here = |browser: &watch::Receiver<Option<u64>>| *browser.borrow() == Some(host);
        let (news_sender, mut news) = mpsc::unbounded_channel();
        let session = Session::pulling(self.relay.clone(), &self.version, self.lookup_timeout, news_sender);
        if let Err(error) = session.ready().await {
            tracing::warn!(error = %error.message, "cannot pull from the browser host");
            while still_here(browser) {
                tokio::select! {
                    () = crate::until_true(stop) => break,
                    changed = browser.changed() => if changed.is_err() { break },
                }
            }
            return;
        }
        tracing::info!("pulling configuration and dictionaries from the browser host");
        let (wanted, wanted_watch) = watch::channel(None::<Value>);
        let reconciler = tokio::spawn(self.clone().reconcile_forever(session.clone(), wanted_watch));
        while still_here(browser) && session.alive() {
            tokio::select! {
                () = crate::until_true(stop) => break,
                changed = browser.changed() => if changed.is_err() { break },
                item = news.recv() => match item {
                    Some(item) => self.take(item, &wanted),
                    None => break,
                },
            }
        }
        reconciler.abort();
        session.close();
    }

    /// One snapshot or storage batch from the browser host.
    fn take(&self, news: HostNews, wanted: &watch::Sender<Option<Value>>) {
        let mut changes: Map<String, Value> = match news {
            HostNews::Snapshot(raw) => {
                let snapshot: Map<String, Value> = serde_json::from_str(raw.get()).unwrap_or_default();
                store::SNAPSHOT_KEYS.iter().map(|key| ((*key).to_owned(), snapshot.get(*key).cloned().unwrap_or(Value::Null))).collect()
            }
            HostNews::Storage(raw) => {
                let changes: Map<String, Value> = serde_json::from_str(raw.get()).unwrap_or_default();
                changes.into_iter().filter(|(key, _)| store::is_shared_key(key)).collect()
            }
        };
        if let Some(state) = changes.remove(store::DICTIONARY_STATE) {
            wanted.send_replace(Some(state));
        }
        if let Err(error) = self.local.publish(changes) {
            tracing::error!(%error, "could not save the browser host's settings");
        }
    }

    /// Installs and then publishes each wanted `dictionaryState`, newest first, retrying on failure.
    async fn reconcile_forever(self: Arc<Self>, session: Arc<Session>, mut wanted: watch::Receiver<Option<Value>>) {
        loop {
            if wanted.changed().await.is_err() {
                return;
            }
            loop {
                let Some(state) = wanted.borrow_and_update().clone() else { break };
                match self.reconcile(&session, &state).await {
                    Ok(()) => break,
                    Err(error) => tracing::warn!(%error, "could not mirror the browser host's dictionaries; retrying"),
                }
                tokio::select! {
                    () = tokio::time::sleep(RETRY) => {}
                    changed = wanted.changed() => if changed.is_err() { return },
                }
            }
        }
    }

    async fn reconcile(&self, session: &Arc<Session>, state: &Value) -> io::Result<()> {
        let wanted = store::generations(state);
        for (id, path) in &wanted {
            if self.store.has_dictionary(path) {
                continue;
            }
            let installed = self.install(session, id).await?;
            if installed.path != *path {
                // The host imported it again meanwhile; its next storage batch names this generation.
                return Err(io::Error::other(format!("dictionary {id} changed during its download")));
            }
        }
        self.local.publish(Map::from_iter([(store::DICTIONARY_STATE.to_owned(), state.clone())]))?;
        let removed = self.store.prune_dictionaries(wanted.iter().map(|(_, path)| path.as_str()))?;
        tracing::info!(dictionaries = wanted.len(), removed, "dictionaries mirrored");
        Ok(())
    }

    /// Downloads dictionary `id` and installs the generation it turns out to be.
    async fn install(&self, session: &Arc<Session>, id: &str) -> io::Result<archive::Manifest> {
        let failed = |error: crate::api::ApiError| io::Error::other(error.message.into_owned());
        let Opened { token, size, file_name } = routes::open_download(session, id, self.lookup_timeout).await.map_err(failed)?;
        tracing::info!(id, %file_name, size, "downloading a dictionary from the browser host");
        let download = self.store.download_file(id);
        let (chunks, mut received) = mpsc::channel(1);
        tokio::spawn(routes::stream_chunks(session.clone(), token, size, chunks, self.chunk_timeout));
        let mut file = tokio::fs::File::create(&download).await?;
        let mut written: u64 = 0;
        while let Some(chunk) = received.recv().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        drop(file);
        if size.is_some_and(|size| written != size) {
            return Err(io::Error::other(format!("dictionary {id} ended after {written} bytes")));
        }
        let store = self.store.clone();
        let key = format!("download:{id}");
        let manifest = tokio::task::spawn_blocking(move || {
            let staging = store.staging_dir(&key)?;
            let unpacked = archive::unpack(&download, &staging);
            let _ = std::fs::remove_file(&download);
            let manifest = unpacked.inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&staging);
            })?;
            store.commit_dictionary(&manifest.path, &staging)?;
            Ok::<_, io::Error>(manifest)
        })
        .await
        .map_err(io::Error::other)??;
        Ok(manifest)
    }
}
