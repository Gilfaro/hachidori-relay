// SPDX-License-Identifier: GPL-3.0-or-later
//! The host's data directory: the shared storage as plain JSON, and the installed
//! dictionaries.
//!
//! ```text
//! <data dir>/
//!   storage.json              every shared key, as the extension stores it
//!   dictionaries/<generation>/  one installed dictionary's Hoshidicts files
//! ```
//!
//! A dictionary is installed per *generation*: the `path` its `dictionaryState`
//! entry names. That path changes every time the dictionary is imported again, so a
//! generation's files never change once installed, and the directory name is a hash
//! of that path. Writes go to a temporary name first and are then renamed into
//! place, so a crash leaves the previous version whole.
//!
//! `storage.json` can be written behind: a change is in memory (and published) at
//! once, and the file is rewritten at most once per write delay, with every change
//! made meanwhile, so a lookup-count row per lookup costs no write of its own. A
//! change of `dictionaryState` is still written before it is applied, because the
//! files of the dictionaries it replaces are deleted after it. The store is written
//! when it is flushed or dropped, too.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// The keys a linked browser mirrors (`SHARED_STATE_KEYS` in the extension's
/// `background-sharing.js`), in its order. The hello snapshot carries exactly these.
pub(crate) const SNAPSHOT_KEYS: [&str; 6] =
    ["dictionaryState", "options", "customDictionarySource", "dictionaryUpdates", "lookupStats", "wordStatusOverrides"];
/// The extension's dictionary list, with each installed generation's `path`.
pub(crate) const DICTIONARY_STATE: &str = "dictionaryState";
/// Lookup-count rows are shared keys too, one per term, under this prefix.
pub(crate) const LOOKUP_STATS_ROW_PREFIX: &str = "lookupStats:";

const STORAGE_FILE: &str = "storage.json";
const DICTIONARIES_DIR: &str = "dictionaries";
const PARTIAL_SUFFIX: &str = ".partial";

/// Whether `key` is mirrored to linked browsers.
pub(crate) fn is_shared_key(key: &str) -> bool {
    SNAPSHOT_KEYS.contains(&key) || key.starts_with(LOOKUP_STATS_ROW_PREFIX)
}

/// The data directory. Cheap to share; every method takes its own short lock.
pub(crate) struct Store {
    root: PathBuf,
    shared: Arc<Shared>,
    /// The thread that writes changes behind, when there is a write delay.
    writer: Option<JoinHandle<()>>,
}

/// What the store and its writer thread share. Locks are taken in the order
/// `storage`, then `file`; `pending` is only ever taken last, and briefly.
struct Shared {
    path: PathBuf,
    storage: Mutex<BTreeMap<String, Value>>,
    /// Held while `storage.json` is replaced. It is taken before `storage` is
    /// released, so states are written in the order they were serialised.
    file: Mutex<()>,
    pending: Mutex<Pending>,
    wake: Condvar,
    delay: Duration,
}

#[derive(Default)]
struct Pending {
    /// When the oldest change not yet written was made.
    since: Option<Instant>,
    closed: bool,
}

impl Shared {
    fn storage(&self) -> MutexGuard<'_, BTreeMap<String, Value>> {
        self.storage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `storage`, the store's contents under its lock, which is released
    /// before the disk is touched. A failed write leaves the changes pending.
    fn write(&self, storage: MutexGuard<'_, BTreeMap<String, Value>>) -> io::Result<()> {
        self.pending().since = None;
        let bytes = serde_json::to_vec_pretty(&*storage).map_err(io::Error::other);
        let file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        drop(storage);
        let written = bytes.and_then(|bytes| write_atomically(&self.path, &bytes));
        drop(file);
        if written.is_err() {
            // Tried again a whole delay from now.
            self.pending().since.get_or_insert_with(Instant::now);
        }
        written
    }

    /// The writer thread: once the oldest unwritten change is a write delay old,
    /// writes everything changed by then; when closed, writes what is left.
    fn run(&self) {
        loop {
            let mut pending = self.pending();
            loop {
                match pending.since {
                    Some(_) if pending.closed => break,
                    Some(since) => {
                        let now = Instant::now();
                        let due = since + self.delay;
                        if now >= due {
                            break;
                        }
                        pending = self.wake.wait_timeout(pending, due - now).unwrap_or_else(PoisonError::into_inner).0;
                    }
                    None if pending.closed => return,
                    None => pending = self.wake.wait(pending).unwrap_or_else(PoisonError::into_inner),
                }
            }
            let closing = pending.closed;
            drop(pending);
            if let Err(error) = self.write(self.storage()) {
                tracing::warn!(%error, path = %self.path.display(), "could not write the storage; trying again");
                if closing {
                    return;
                }
            }
        }
    }
}

impl Store {
    /// Opens (and creates, if need be) the data directory, and reads its storage.
    /// Leftovers of an interrupted install are removed.
    #[cfg(test)]
    pub(crate) fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with(root, Duration::ZERO)
    }

    /// [`Store::open`], writing `storage.json` at most once per `delay` (zero: at
    /// every change, before it is applied).
    pub(crate) fn open_with(root: impl Into<PathBuf>, delay: Duration) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join(DICTIONARIES_DIR))?;
        let storage = match fs::read(root.join(STORAGE_FILE)) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, Value>>(&bytes).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a JSON object: {error}", root.join(STORAGE_FILE).display()))
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        let shared = Arc::new(Shared {
            path: root.join(STORAGE_FILE),
            storage: Mutex::new(storage),
            file: Mutex::new(()),
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            delay,
        });
        let writer = if delay.is_zero() {
            None
        } else {
            let shared = shared.clone();
            Some(std::thread::Builder::new().name("storage writer".to_owned()).spawn(move || shared.run())?)
        };
        let store = Self { root, shared, writer };
        for entry in fs::read_dir(store.dictionaries_dir())?.flatten() {
            if entry.file_name().to_string_lossy().ends_with(PARTIAL_SUFFIX) {
                let path = entry.path();
                let _ = if path.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
            }
        }
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Value>> {
        self.shared.storage()
    }

    /// Writes the changes not written yet, if any.
    pub(crate) fn flush(&self) -> io::Result<()> {
        let storage = self.lock();
        if self.shared.pending().since.is_none() {
            return Ok(());
        }
        self.shared.write(storage)
    }

    /// The data directory itself.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn dictionaries_dir(&self) -> PathBuf {
        self.root.join(DICTIONARIES_DIR)
    }

    /// One stored value.
    pub(crate) fn get(&self, key: &str) -> Option<Value> {
        self.lock().get(key).cloned()
    }

    /// Whether `key` holds `value` (`null` for one never set), compared without a copy.
    pub(crate) fn holds(&self, key: &str, value: &Value) -> bool {
        self.lock().get(key).unwrap_or(&Value::Null) == value
    }

    /// The hello snapshot: the six mirrored keys, `null` for one never set.
    pub(crate) fn snapshot(&self) -> Map<String, Value> {
        let storage = self.lock();
        SNAPSHOT_KEYS.iter().map(|key| ((*key).to_owned(), storage.get(*key).cloned().unwrap_or(Value::Null))).collect()
    }

    /// Writes `changes` (`null` removes a key) and saves them, then runs `after` with the
    /// keys that really changed while the store is still locked, so whoever publishes
    /// them does so in the order they were stored. Returns those changes.
    #[cfg(test)]
    pub(crate) fn apply(&self, incoming: Map<String, Value>, after: impl FnOnce(&Map<String, Value>)) -> io::Result<Map<String, Value>> {
        self.apply_with(|_| incoming, after)
    }

    /// [`Store::apply`] of the changes `derive` makes of what is stored, all under one lock.
    pub(crate) fn apply_with(
        &self,
        derive: impl FnOnce(&BTreeMap<String, Value>) -> Map<String, Value>,
        after: impl FnOnce(&Map<String, Value>),
    ) -> io::Result<Map<String, Value>> {
        let mut storage = self.lock();
        let incoming = derive(&storage);
        let changed: Map<String, Value> = incoming
            .into_iter()
            .filter(|(key, value)| match storage.get(key) {
                Some(current) => current != value,
                None => !value.is_null(),
            })
            .collect();
        if changed.is_empty() {
            return Ok(changed);
        }
        let set = |storage: &mut BTreeMap<String, Value>| {
            for (key, value) in &changed {
                if value.is_null() {
                    storage.remove(key);
                } else {
                    storage.insert(key.clone(), value.clone());
                }
            }
        };
        if self.writer.is_none() || changed.contains_key(DICTIONARY_STATE) {
            // On disk before it is applied, so a failed write changes nothing.
            let mut next = storage.clone();
            set(&mut next);
            let bytes = serde_json::to_vec_pretty(&next).map_err(io::Error::other)?;
            let file = self.shared.file.lock().unwrap_or_else(PoisonError::into_inner);
            write_atomically(&self.shared.path, &bytes)?;
            drop(file);
            // That write holds every change made before it as well.
            self.shared.pending().since = None;
            *storage = next;
        } else {
            set(&mut storage);
            let first = {
                let mut pending = self.shared.pending();
                pending.since.is_none().then(|| pending.since = Some(Instant::now())).is_some()
            };
            if first {
                self.shared.wake.notify_one();
            }
        }
        after(&changed);
        drop(storage);
        Ok(changed)
    }

    /// Where the dictionary generation `path` is (or would be) installed.
    pub(crate) fn dictionary_dir(&self, path: &str) -> PathBuf {
        self.dictionaries_dir().join(generation_name(path))
    }

    /// Whether generation `path` is installed.
    pub(crate) fn has_dictionary(&self, path: &str) -> bool {
        self.dictionary_dir(path).is_dir()
    }

    /// Where an archive downloaded under `key` waits to be unpacked. Removed on the next start if left over.
    pub(crate) fn download_file(&self, key: &str) -> PathBuf {
        self.dictionaries_dir().join(format!("{}.zip{PARTIAL_SUFFIX}", generation_name(key)))
    }

    /// A fresh, empty directory to unpack into under `key`, before [`Self::commit_dictionary`].
    pub(crate) fn staging_dir(&self, key: &str) -> io::Result<PathBuf> {
        let staging = self.dictionaries_dir().join(format!("{}{PARTIAL_SUFFIX}", generation_name(key)));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        Ok(staging)
    }

    /// Moves a fully unpacked staging directory into place as generation `path`.
    pub(crate) fn commit_dictionary(&self, path: &str, staging: &Path) -> io::Result<()> {
        let target = self.dictionary_dir(path);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        fs::rename(staging, target)
    }

    /// Removes installed generation `path`, if it is installed.
    pub(crate) fn remove_dictionary(&self, path: &str) -> io::Result<()> {
        match fs::remove_dir_all(self.dictionary_dir(path)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Removes every installed generation that is not in `keep`. Returns how many went.
    pub(crate) fn prune_dictionaries<'a>(&self, keep: impl IntoIterator<Item = &'a str>) -> io::Result<usize> {
        let keep: HashSet<String> = keep.into_iter().map(generation_name).collect();
        let mut removed = 0;
        for entry in fs::read_dir(self.dictionaries_dir())?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) && !name.ends_with(PARTIAL_SUFFIX) && entry.path().is_dir() {
                fs::remove_dir_all(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// The installed directory name for a generation path: a hash, so any path the
/// browser uses is a safe, fixed-length name here.
fn generation_name(path: &str) -> String {
    sha1_smol::Sha1::from(path.as_bytes()).digest().to_string()
}

/// Every generation `path` named in a `dictionaryState` value, with its dictionary `id`.
pub(crate) fn generations(state: &Value) -> Vec<(String, String)> {
    let Some(dictionaries) = state.get("dictionaries").and_then(Value::as_array) else { return Vec::new() };
    dictionaries.iter().filter_map(|entry| Some((entry.get("id")?.as_str()?.to_owned(), entry.get("path")?.as_str()?.to_owned()))).collect()
}

impl Drop for Store {
    /// Writes what is pending and stops the writer thread.
    fn drop(&mut self) {
        let Some(writer) = self.writer.take() else { return };
        self.shared.pending().closed = true;
        self.shared.wake.notify_one();
        let _ = writer.join();
    }
}

/// Replaces `path` with `bytes` so that a reader sees either the old file or the new one.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hachidori-relay-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    #[test]
    fn changes_persist_and_only_real_changes_are_reported() {
        let dir = scratch("store-changes");
        let store = Store::open(&dir).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(store.snapshot().get("options"), Some(&Value::Null));
        let mut seen = Vec::new();
        let changed = store
            .apply(object(json!({ "options": { "scanLength": 20 }, "lookupStats:g:a": { "lookupCount": 1 }, "absent": null })), |changed| {
                seen.extend(changed.keys().cloned());
            })
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(changed.len(), 2);
        assert_eq!(seen, ["lookupStats:g:a", "options"]);
        let again = store.apply(object(json!({ "options": { "scanLength": 20 } })), |_| panic!("nothing changed")).ok();
        assert_eq!(again.map(|changed| changed.len()), Some(0));
        drop(store);

        let reopened = Store::open(&dir).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(reopened.snapshot().get("options"), Some(&json!({ "scanLength": 20 })));
        let _ = reopened.apply(object(json!({ "options": null })), |_| {});
        assert_eq!(reopened.snapshot().get("options"), Some(&Value::Null));
        let _ = fs::remove_dir_all(dir);
    }

    fn on_disk(dir: &Path) -> Value {
        fs::read(dir.join(STORAGE_FILE)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(Value::Null)
    }

    #[test]
    fn changes_are_written_behind_together() {
        let dir = scratch("store-behind");
        let store = Store::open_with(&dir, Duration::from_millis(300)).unwrap_or_else(|error| panic!("{error}"));
        for count in 1..=50 {
            let _ = store.apply(object(json!({ "lookupStats:g:a": { "lookupCount": count } })), |_| {});
        }
        assert_eq!(store.get("lookupStats:g:a"), Some(json!({ "lookupCount": 50 })), "in memory at once");
        assert_eq!(on_disk(&dir), Value::Null, "not written yet");
        let deadline = Instant::now() + Duration::from_secs(10);
        while on_disk(&dir).get("lookupStats:g:a") != Some(&json!({ "lookupCount": 50 })) {
            assert!(Instant::now() < deadline, "written once the delay has passed");
            std::thread::sleep(Duration::from_millis(20));
        }

        // A new dictionary list is on disk before apply returns, with what was pending.
        let _ = store.apply(object(json!({ "options": { "scanLength": 20 } })), |_| {});
        let _ = store.apply(object(json!({ "dictionaryState": { "dictionaries": [] } })), |_| {});
        let written = on_disk(&dir);
        assert_eq!(written.get("dictionaryState"), Some(&json!({ "dictionaries": [] })));
        assert_eq!(written.get("options"), Some(&json!({ "scanLength": 20 })));

        // Flushing and dropping write what is pending.
        let _ = store.apply(object(json!({ "options": { "scanLength": 21 } })), |_| {});
        store.flush().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(on_disk(&dir).get("options"), Some(&json!({ "scanLength": 21 })));
        let _ = store.apply(object(json!({ "options": { "scanLength": 22 } })), |_| {});
        drop(store);
        assert_eq!(on_disk(&dir).get("options"), Some(&json!({ "scanLength": 22 })));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dictionaries_install_by_generation_and_prune() {
        let dir = scratch("store-dictionaries");
        let store = Store::open(&dir).unwrap_or_else(|error| panic!("{error}"));
        let staging = store.staging_dir("/dicts/.hdw-generation-1/A").unwrap_or_else(|error| panic!("{error}"));
        fs::write(staging.join("index.json"), "{}").unwrap_or_else(|error| panic!("{error}"));
        assert!(!store.has_dictionary("/dicts/.hdw-generation-1/A"));
        store.commit_dictionary("/dicts/.hdw-generation-1/A", &staging).unwrap_or_else(|error| panic!("{error}"));
        assert!(store.has_dictionary("/dicts/.hdw-generation-1/A"));
        let leftover = store.staging_dir("/dicts/.hdw-generation-2/A").unwrap_or_else(|error| panic!("{error}"));
        drop(store);
        let store = Store::open(&dir).unwrap_or_else(|error| panic!("{error}"));
        assert!(!leftover.exists(), "an interrupted install is cleaned up");
        assert_eq!(store.prune_dictionaries(["/dicts/.hdw-generation-1/A"]).ok(), Some(0));
        assert_eq!(store.prune_dictionaries([]).ok(), Some(1));
        assert!(!store.has_dictionary("/dicts/.hdw-generation-1/A"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn generations_read_id_and_path() {
        let state = json!({ "dictionaries": [{ "id": "a", "path": "/dicts/x/A" }, { "id": "b" }, { "path": "/p" }] });
        assert_eq!(generations(&state), [("a".to_owned(), "/dicts/x/A".to_owned())]);
        assert_eq!(generations(&Value::Null), Vec::<(String, String)>::new());
    }
}
