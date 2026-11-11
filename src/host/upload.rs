// SPDX-License-Identifier: GPL-3.0-or-later
//! Dictionary uploads from linked browsers (`linked-import.js`' `createUploadHost`,
//! behind `linked-import-v1`): `hd_import_begin`, sequential `hd_import_chunk`s and
//! `hd_import_commit` (or `hd_import_abort`) to `hachidori-linked-import`. The bytes
//! wait in a file in the data directory until the commit imports them.
//!
//! An upload belongs to the linked browser that began it, is dropped when that
//! browser leaves, and is abandoned two minutes after its last chunk.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use super::js;
use super::settings::worker;
use super::store::Store;

/// `LINKED_IMPORT_CAPABILITY`.
pub(crate) const CAPABILITY: &str = "linked-import-v1";
/// `LINKED_IMPORT_TARGET`.
pub(crate) const TARGET: &str = "hachidori-linked-import";
/// `UPLOAD_CHUNK_BYTES`: raw bytes per chunk.
const CHUNK_BYTES: u64 = 1024 * 1024;
/// `UPLOAD_IDLE_MS`.
const IDLE: Duration = Duration::from_secs(120);
const NOT_OPEN: &str = "The dictionary upload is no longer open on the host. Import it again.";

type Result<T> = std::result::Result<T, String>;

/// One upload request, as `allowLinkedImportRequest` rebuilds it.
pub(crate) enum Request {
    /// `profile_id` is the profile the import joins (`selectedProfileId`).
    Begin {
        file_name: String,
        size: u64,
        replace: bool,
        profile_id: Option<String>,
    },
    Chunk {
        token: String,
        offset: Value,
        data: String,
    },
    Commit {
        token: String,
    },
    Abort {
        token: String,
    },
}

/// `requestIdOf`: a string or finite number, or `null`.
pub(crate) fn request_id(message: &Value) -> Value {
    match message.get("requestId") {
        Some(id @ Value::String(_)) => id.clone(),
        Some(id @ Value::Number(number)) if number.as_f64().is_some_and(f64::is_finite) => id.clone(),
        _ => Value::Null,
    }
}

/// `uploadToken`.
fn token(value: Option<&Value>) -> Result<String> {
    let token = value
        .and_then(Value::as_str)
        .filter(|token| (1..=64).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
    token.map(str::to_owned).ok_or_else(|| "malformed dictionary upload token".to_owned())
}

/// `allowLinkedImportRequest`: the fields an upload request needs, checked.
pub(crate) fn allow(message: &Value) -> Result<Request> {
    let field = |name: &str| message.get(name);
    match field("type").and_then(Value::as_str) {
        Some("hd_import_begin") => {
            let file_name = field("fileName").and_then(Value::as_str).filter(|name| name.encode_utf16().count() <= 255);
            let size = js::safe_integer(field("size"));
            match (file_name, size, field("replace").and_then(Value::as_bool)) {
                (Some(file_name), Some(size), Some(replace)) => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a safe integer, at most 2^53 - 1")]
                    let size = if size > 0.0 { size as u64 } else { 0 };
                    // The import joins the profile it names (docs/languages.md 5.2).
                    let profile_id = super::anki::selected_profile_id(message)?.and_then(|id| id.as_str().map(str::to_owned));
                    Ok(Request::Begin { file_name: file_name.to_owned(), size, replace, profile_id })
                }
                _ => Err("malformed dictionary upload request".to_owned()),
            }
        }
        Some("hd_import_chunk") => {
            let (Some(offset), Some(data)) =
                (field("offset").filter(|offset| js::safe_integer(Some(offset)).is_some()), field("data").and_then(Value::as_str))
            else {
                return Err("malformed dictionary upload chunk".to_owned());
            };
            Ok(Request::Chunk { token: token(field("token"))?, offset: offset.clone(), data: data.to_owned() })
        }
        Some("hd_import_commit") => Ok(Request::Commit { token: token(field("token"))? }),
        Some("hd_import_abort") => Ok(Request::Abort { token: token(field("token"))? }),
        _ => Err("unsupported dictionary upload request".to_owned()),
    }
}

/// `decodedLength`: the bytes a strict base64 text decodes to.
fn decoded_length(data: &str) -> Result<u64> {
    let bytes = data.as_bytes();
    let alphabet = |byte: &u8| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/';
    let tail = bytes.len().saturating_sub(4);
    let valid = bytes.len().is_multiple_of(4)
        && bytes[..tail].iter().all(alphabet)
        && match &bytes[tail..] {
            [] => true,
            [a, b, b'=', b'='] => alphabet(a) && alphabet(b),
            [a, b, c, b'='] => alphabet(a) && alphabet(b) && alphabet(c),
            last => last.iter().all(alphabet),
        };
    if !valid {
        return Err("the dictionary upload chunk is not base64".to_owned());
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    Ok((bytes.len() / 4 * 3 - padding) as u64)
}

/// Whether a chunk's `offset` (a safe integer) is byte `received`, as `===` compares them.
fn at(offset: &Value, received: u64) -> bool {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a non-negative safe integer, at most 2^53 - 1")]
    js::safe_integer(Some(offset)).is_some_and(|offset| offset >= 0.0 && offset as u64 == received)
}

/// One open upload.
struct Session {
    /// Which session this is, so a timer or a finished append finds its own.
    id: u64,
    owner: Arc<str>,
    file_name: String,
    size: u64,
    replace: bool,
    profile_id: Option<String>,
    received: u64,
    busy: bool,
    file: PathBuf,
    /// Bumped by every chunk; an idle timer only drops what it armed.
    armed: u64,
}

/// A complete upload, handed to the import. Its file is removed when this is dropped.
pub(crate) struct Complete {
    pub(crate) file: PathBuf,
    pub(crate) file_name: String,
    pub(crate) replace: bool,
    pub(crate) profile_id: Option<String>,
}

impl Drop for Complete {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
    }
}

/// The open uploads.
pub(crate) struct Uploads {
    store: Arc<Store>,
    sessions: Mutex<HashMap<String, Session>>,
    next: AtomicU64,
}

impl Uploads {
    pub(crate) fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self { store, sessions: Mutex::default(), next: AtomicU64::new(0) })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `drop`: forgets an upload and its bytes.
    fn drop_upload(&self, token: &str) {
        let gone = self.lock().remove(token);
        if let Some(gone) = gone {
            let _ = fs::remove_file(gone.file);
        }
    }

    /// `arm`: abandons the upload `IDLE` after now, unless a chunk arms it again.
    fn arm(self: &Arc<Self>, token: &str) {
        let armed = self.lock().get_mut(token).map(|session| {
            session.armed += 1;
            (session.id, session.armed)
        });
        let Some(armed) = armed else { return };
        let (uploads, token) = (Arc::downgrade(self), token.to_owned());
        tokio::spawn(async move {
            tokio::time::sleep(IDLE).await;
            let Some(uploads) = uploads.upgrade() else { return };
            let idle = uploads.lock().get(&token).is_some_and(|session| (session.id, session.armed) == armed);
            if idle {
                uploads.drop_upload(&token);
            }
        });
    }

    /// `owned`: checks that `token` is open and `owner`'s.
    fn owned<'a>(sessions: &'a mut HashMap<String, Session>, token: &str, owner: &str) -> Result<&'a mut Session> {
        sessions.get_mut(token).filter(|session| &*session.owner == owner).ok_or_else(|| NOT_OPEN.to_owned())
    }

    /// `begin`: `{token, chunkBytes}`.
    pub(crate) fn begin(
        self: &Arc<Self>,
        owner: &Arc<str>,
        file_name: &str,
        size: u64,
        replace: bool,
        profile_id: Option<String>,
    ) -> Result<Value> {
        if file_name.is_empty() || file_name == "." || file_name == ".." || file_name.contains(['/', '\\', '\0']) {
            return Err("The dictionary upload has no usable file name.".to_owned());
        }
        let lower = file_name.to_lowercase();
        #[allow(clippy::case_sensitive_file_extension_comparisons, reason = "the name is lower-cased, and a bare \".mdx\" counts too")]
        if lower.ends_with(".mdx") || lower.ends_with(".mdd") {
            return Err("MDX dictionaries cannot be sent to another Hachidori. Import them on that Hachidori itself.".to_owned());
        }
        if size == 0 {
            return Err(format!("{file_name} is empty."));
        }
        let token = worker::random_uuid()?;
        let file = self.store.download_file(&format!("upload:{token}"));
        fs::write(&file, b"").map_err(|error| format!("The dictionary upload could not be stored. {error}"))?;
        let session = Session {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            owner: owner.clone(),
            file_name: file_name.to_owned(),
            size,
            replace,
            profile_id,
            received: 0,
            busy: false,
            file,
            armed: 0,
        };
        self.lock().insert(token.clone(), session);
        self.arm(&token);
        Ok(json!({ "token": token, "chunkBytes": CHUNK_BYTES }))
    }

    /// `chunk`: the next bytes, at `offset`. `{received}`.
    pub(crate) fn chunk(self: &Arc<Self>, owner: &str, token: &str, offset: &Value, data: &str) -> Result<Value> {
        let (id, file, length) = {
            let mut sessions = self.lock();
            let session = Self::owned(&mut sessions, token, owner)?;
            if session.busy || !at(offset, session.received) {
                let error = format!(
                    "The dictionary upload expected byte {}, not {}. Import it again.",
                    session.received,
                    js::to_string(Some(offset))
                );
                drop(sessions);
                self.drop_upload(token);
                return Err(error);
            }
            let length = decoded_length(data).and_then(|length| {
                if length == 0 || length > CHUNK_BYTES || session.received + length > session.size {
                    return Err("the dictionary upload chunk does not fit the announced size".to_owned());
                }
                Ok(length)
            });
            let length = match length {
                Ok(length) => length,
                Err(error) => {
                    drop(sessions);
                    self.drop_upload(token);
                    return Err(error);
                }
            };
            session.busy = true;
            (session.id, session.file.clone(), length)
        };
        let engine = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
        );
        let appended = engine
            .decode(data)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                if bytes.len() as u64 != length {
                    return Err("the dictionary upload chunk did not decode to its length".to_owned());
                }
                let mut out = OpenOptions::new().append(true).open(&file).map_err(|error| error.to_string())?;
                out.write_all(&bytes).map_err(|error| error.to_string())
            })
            .map_err(|error| format!("The dictionary upload could not be stored. {error}"));
        if let Err(error) = appended {
            self.drop_upload(token);
            return Err(error);
        }
        // An abort, a timeout or a disconnect may have retired it meanwhile.
        let received = self.lock().get_mut(token).filter(|session| session.id == id).map(|session| {
            session.busy = false;
            session.received += length;
            session.received
        });
        let Some(received) = received else { return Err("The dictionary upload was cancelled.".to_owned()) };
        self.arm(token);
        Ok(json!({ "received": received }))
    }

    /// `commit`: the complete upload, which now belongs to the import.
    pub(crate) fn commit(&self, owner: &str, token: &str) -> Result<Complete> {
        let mut sessions = self.lock();
        let session = Self::owned(&mut sessions, token, owner)?;
        if session.busy || session.received != session.size {
            let error = format!("The dictionary upload ended after {} of {} bytes. Import it again.", session.received, session.size);
            drop(sessions);
            self.drop_upload(token);
            return Err(error);
        }
        let session = sessions.remove(token).ok_or_else(|| NOT_OPEN.to_owned())?;
        drop(sessions);
        Ok(Complete { file: session.file, file_name: session.file_name, replace: session.replace, profile_id: session.profile_id })
    }

    /// `abort`: drops `owner`'s upload `token`, if it is open.
    pub(crate) fn abort(&self, owner: &str, token: &str) -> Value {
        let mine = self.lock().get(token).is_some_and(|session| &*session.owner == owner);
        if mine {
            self.drop_upload(token);
        }
        json!({})
    }

    /// `dropWhere`: every upload of a browser that left.
    pub(crate) fn drop_owner(&self, owner: &str) {
        let tokens: Vec<String> =
            self.lock().iter().filter(|(_, session)| &*session.owner == owner).map(|(token, _)| token.clone()).collect();
        for token in tokens {
            self.drop_upload(&token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_rebuilt_from_the_fields_they_need() {
        let begin = allow(&json!({ "type": "hd_import_begin", "fileName": "a.zip", "size": 3, "replace": false }));
        assert!(matches!(begin, Ok(Request::Begin { size: 3, replace: false, .. })));
        assert!(allow(&json!({ "type": "hd_import_begin", "fileName": "a.zip", "size": 1.5, "replace": false })).is_err());
        let named = allow(&json!({ "type": "hd_import_begin", "fileName": "a.zip", "size": 3, "replace": false, "profileId": "en" }));
        assert!(matches!(named, Ok(Request::Begin { profile_id: Some(id), .. }) if id == "en"));
        for profile in [json!(""), json!(3), json!("a")] {
            let begin =
                allow(&json!({ "type": "hd_import_begin", "fileName": "a.zip", "size": 3, "replace": false, "profileId": profile }));
            assert_eq!(begin.err().as_deref(), Some("malformed linked profile"));
        }
        assert!(allow(&json!({ "type": "hd_import_chunk", "token": "a/b", "offset": 0, "data": "" })).is_err());
        assert_eq!(allow(&json!({ "type": "hd_import_x" })).err().as_deref(), Some("unsupported dictionary upload request"));
        assert_eq!(request_id(&json!({ "requestId": {} })), Value::Null);
    }

    #[test]
    fn chunks_are_strict_base64() {
        assert_eq!(decoded_length("YWJj").ok(), Some(3));
        assert_eq!(decoded_length("YWI=").ok(), Some(2));
        assert_eq!(decoded_length("YQ==").ok(), Some(1));
        assert_eq!(decoded_length("").ok(), Some(0));
        assert!(decoded_length("YQ=").is_err() && decoded_length("Y===").is_err() && decoded_length("YW=j").is_err());
    }

    /// Upstream's linked-import.test.mjs "abort, the idle timeout and a
    /// disconnect drop held uploads".
    #[tokio::test(start_paused = true)]
    async fn abort_the_idle_timeout_and_a_disconnect_drop_held_uploads() {
        let dir = std::env::temp_dir().join(format!("hachidori-relay-test-upload-idle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let uploads = Uploads::new(Arc::new(Store::open(&dir).unwrap_or_else(|error| panic!("{error}"))));
        let (a, b, local): (Arc<str>, Arc<str>, Arc<str>) = (Arc::from("remote:a"), Arc::from("remote:b"), Arc::from("local"));
        let begin = |owner: &Arc<str>, name: &str| {
            uploads.begin(owner, name, 4, true, None).unwrap_or_else(|error| panic!("{error}"))["token"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        let aborted = begin(&a, "a.zip");
        assert_eq!(uploads.abort("remote:a", &aborted), json!({}));
        assert!(uploads.chunk("remote:a", &aborted, &json!(0), "YWJj").is_err());

        // A chunk renews the wait; silence for the whole of it drops the upload.
        let idle = begin(&a, "b.zip");
        tokio::time::sleep(IDLE.saturating_sub(Duration::from_secs(1))).await;
        assert_eq!(uploads.chunk("remote:a", &idle, &json!(0), "YQ==").ok(), Some(json!({ "received": 1 })));
        tokio::time::sleep(IDLE.saturating_sub(Duration::from_secs(1))).await;
        assert_eq!(uploads.chunk("remote:a", &idle, &json!(1), "Yg==").ok(), Some(json!({ "received": 2 })));
        tokio::time::sleep(IDLE + Duration::from_secs(1)).await;
        assert_eq!(uploads.chunk("remote:a", &idle, &json!(2), "Yw==").err().as_deref(), Some(NOT_OPEN));

        let gone = begin(&b, "c.zip");
        let kept = begin(&local, "d.zip");
        uploads.drop_owner("remote:b");
        assert!(uploads.chunk("remote:b", &gone, &json!(0), "YQ==").is_err());
        assert_eq!(uploads.lock().len(), 1);
        assert!(uploads.lock().contains_key(&kept));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_upload_arrives_in_order_and_belongs_to_its_sender() {
        let dir = std::env::temp_dir().join(format!("hachidori-relay-test-upload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let uploads = Uploads::new(Arc::new(Store::open(&dir).unwrap_or_else(|error| panic!("{error}"))));
        let owner: Arc<str> = Arc::from("one");
        assert!(uploads.begin(&owner, "a.mdx", 3, false, None).is_err());
        assert!(uploads.begin(&owner, "a.zip", 0, false, None).is_err());
        let begun = uploads.begin(&owner, "a.zip", 3, false, None).unwrap_or_else(|error| panic!("{error}"));
        let token = begun["token"].as_str().unwrap_or_default().to_owned();
        assert!(uploads.chunk("two", &token, &json!(0), "YWI=").is_err(), "another browser's upload");
        assert_eq!(uploads.chunk("one", &token, &json!(0), "YWI=").ok(), Some(json!({ "received": 2 })));
        assert!(uploads.commit("one", &token).is_err(), "one byte is missing");
        assert!(uploads.chunk("one", &token, &json!(2), "Yw==").is_err(), "the failed commit dropped it");

        let token = uploads.begin(&owner, "a.zip", 3, true, None).unwrap_or_else(|error| panic!("{error}"))["token"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(uploads.chunk("one", &token, &json!(0), "YWJj").ok(), Some(json!({ "received": 3 })));
        let complete = uploads.commit("one", &token).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(fs::read(&complete.file).ok(), Some(b"abc".to_vec()));
        let file = complete.file.clone();
        drop(complete);
        assert!(!file.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
