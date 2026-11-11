// SPDX-License-Identifier: GPL-3.0-or-later
//! The one-dictionary archive a dictionary download serves: read from a browser
//! host, and written again for the relay's own downloads.
//!
//! The archive is the extension's backup format (its `docs/backup-format.md`): a
//! stored ZIP64 with `hachidori-backup.json` and the dictionary's Hoshidicts files
//! under `dictionaries/0/`. Nothing here needs the engine: the files are already the
//! native index, and installing them is unpacking them.
//!
//! The relay serves an installed dictionary back ([`pack`]) with a manifest built,
//! as the extension builds it, from what is stored now.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path};

use serde_json::Value;

use crate::host::js;

const MANIFEST: &str = "hachidori-backup.json";
const FORMAT: &str = "hachidori-backup";
const PAYLOAD: &str = "dictionaries/0/";
/// Where an older relay kept an installed dictionary's download manifest. Such a
/// file is not part of the dictionary, and is never served.
const KEPT_MANIFEST: &str = ".hachidori-relay-manifest.json";
/// `createBackupArchive`'s manifest version.
const VERSION: u32 = 3;

/// What the archive says about the dictionary it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// The dictionary's `id`.
    pub(crate) id: String,
    /// The generation `path` the files belong to.
    pub(crate) path: String,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// `assertBackupPath`.
fn backup_path(path: &str) -> io::Result<()> {
    if path.chars().any(|c| c == '\\' || c <= '\u{1f}' || c == '\u{7f}')
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(format!("Invalid backup file path: {path}")));
    }
    Ok(())
}

/// `dictionaries/<n>/…`, `n` without leading zeros.
fn payload_path(path: &str) -> bool {
    path.strip_prefix("dictionaries/").and_then(|rest| rest.split_once('/')).is_some_and(|(index, rest)| {
        !rest.is_empty() && !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) && (index == "0" || !index.starts_with('0'))
    })
}

/// `assertFileList`: the manifest's `files`, as `(path, size)`.
fn file_list(manifest: &Value) -> io::Result<Vec<(String, u64)>> {
    let files = manifest.get("files").and_then(Value::as_array).ok_or_else(|| invalid("The backup has no file list."))?;
    let mut listed: Vec<(String, u64)> = Vec::with_capacity(files.len());
    let mut paths = std::collections::HashSet::new();
    for file in files {
        let path = file.get("path").and_then(Value::as_str).ok_or_else(|| invalid("Invalid backup file path: undefined"))?;
        backup_path(path)?;
        if !payload_path(path) {
            return Err(invalid(format!("Unexpected backup payload path: {path}")));
        }
        let size = js::safe_integer(file.get("size")).filter(|size| *size >= 0.0).ok_or_else(|| invalid("Invalid backup file size."))?;
        if !paths.insert(path) {
            return Err(invalid(format!("Duplicate backup file: {path}")));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a non-negative safe integer")]
        listed.push((path.to_owned(), size as u64));
    }
    for (path, _) in &listed {
        let mut parent = path.as_str();
        while let Some((above, _)) = parent.rsplit_once('/') {
            if paths.contains(above) {
                return Err(invalid(format!("Backup file is also a directory: {path}")));
            }
            parent = above;
        }
    }
    Ok(listed)
}

/// Unpacks the dictionary in `archive` into the empty directory `into`, and returns
/// its manifest. The archive is checked as `openBackupArchive` checks a backup:
/// regular, stored, unencrypted entries with safe and distinct paths, exactly the
/// files its manifest lists at their listed sizes, each CRC verified. Beyond that,
/// it must carry one dictionary, under `dictionaries/0/`.
pub(crate) fn unpack(archive: &Path, into: &Path) -> io::Result<Manifest> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?).map_err(|error| invalid(format!("not a dictionary archive: {error}")))?;
    let mut names = std::collections::HashSet::new();
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).map_err(|error| invalid(error.to_string()))?;
        let name = entry.name().map_err(|error| invalid(error.to_string()))?.into_owned();
        backup_path(&name)?;
        let kind = entry.unix_mode().unwrap_or(0) & 0o170_000;
        if entry.is_dir() || entry.is_symlink() || (kind != 0 && kind != 0o100_000) {
            return Err(invalid(format!("The backup entry is not a regular file: {name}")));
        }
        if entry.encrypted() || entry.compression() != zip::CompressionMethod::Stored {
            return Err(invalid("Hachidori backups contain only unencrypted, stored ZIP entries."));
        }
        if !names.insert(name.clone()) {
            return Err(invalid(format!("Duplicate backup file: {name}")));
        }
    }
    let text = {
        let mut entry = zip.by_name(MANIFEST).map_err(|_| invalid("The selected archive is not a Hachidori backup."))?;
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        text
    };
    let (manifest, files) = read_manifest(&text)?;
    if zip.len() != files.len() + 1 {
        return Err(invalid("The backup contains unlisted or missing files."));
    }
    for (name, size) in &files {
        let mut entry = zip.by_name(name).map_err(|_| invalid(format!("The backup is missing {name}")))?;
        if entry.size() != *size {
            return Err(invalid(format!("Incorrect backup file size: {name}")));
        }
        let relative = Path::new(name.strip_prefix(PAYLOAD).unwrap_or(name));
        if !name.starts_with(PAYLOAD)
            || relative == Path::new(KEPT_MANIFEST)
            || !relative.components().all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid(format!("unexpected archive entry {name:?}")));
        }
        let target = into.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // Reading to the end checks the entry's CRC32.
        if io::copy(&mut entry, &mut File::create(&target)?)? != *size {
            return Err(invalid(format!("Incorrect extracted backup file size: {name}")));
        }
    }
    if files.is_empty() {
        return Err(invalid("the archive has no dictionary files"));
    }
    Ok(manifest)
}

/// Every file of the dictionary installed in `dir`, as `(archive path, file)`, in the
/// extension's order (`collectBackupFiles`: each directory's names sorted).
fn payload(dir: &Path, prefix: &str, files: &mut Vec<(String, std::path::PathBuf)>) -> io::Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.file_name())).collect::<io::Result<_>>()?;
    names.sort_by(|left, right| left.to_string_lossy().encode_utf16().cmp(right.to_string_lossy().encode_utf16()));
    for name in names {
        let name_text = name.to_string_lossy().into_owned();
        if prefix.is_empty() && name_text == KEPT_MANIFEST {
            continue;
        }
        let path = dir.join(&name);
        let archived = format!("{prefix}{name_text}");
        if path.is_dir() {
            payload(&path, &format!("{archived}/"), files)?;
        } else {
            files.push((archived, path));
        }
    }
    Ok(())
}

/// Writes the one-dictionary archive for the dictionary installed in `dir` to `out`,
/// as `createBackupArchive(snapshot, files, [], createdAt)` writes it: the manifest,
/// then the files, stored. Returns the archive's size.
pub(crate) fn pack(dir: &Path, snapshot: &Value, created_at: &str, out: &Path) -> io::Result<u64> {
    let mut files = Vec::new();
    payload(dir, "", &mut files)?;
    let mut list = Vec::with_capacity(files.len());
    for (name, path) in &files {
        list.push(serde_json::json!({ "path": format!("{PAYLOAD}{name}"), "size": fs::metadata(path)?.len() }));
    }
    let manifest = serde_json::json!({
        "format": FORMAT, "version": VERSION, "createdAt": created_at, "snapshot": snapshot, "lookupStatsRows": [], "files": list,
    });
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true)
        .last_modified_time(zip::DateTime::default());
    let mut writer = zip::ZipWriter::new(io::BufWriter::new(File::create(out)?));
    let failed = |error: zip::result::ZipError| io::Error::other(error.to_string());
    writer.start_file(MANIFEST, stored).map_err(failed)?;
    writer.write_all(manifest.to_string().as_bytes())?;
    for (name, path) in &files {
        writer.start_file(format!("{PAYLOAD}{name}"), stored).map_err(failed)?;
        io::copy(&mut File::open(path)?, &mut writer)?;
    }
    writer.finish().map_err(failed)?.flush()?;
    Ok(fs::metadata(out)?.len())
}

/// `!Number.isNaN(Date.parse(text))` for what hosts write, `toISOString()`: a
/// date, optionally with a time.
fn iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes.get(range).is_some_and(|part| part.iter().all(u8::is_ascii_digit));
    digits(0..4)
        && bytes.get(4) == Some(&b'-')
        && digits(5..7)
        && bytes.get(7) == Some(&b'-')
        && digits(8..10)
        && (bytes.len() == 10 || bytes.get(10) == Some(&b'T'))
}

fn read_manifest(text: &str) -> io::Result<(Manifest, Vec<(String, u64)>)> {
    let manifest: Value = serde_json::from_str(text).map_err(|error| invalid(format!("{MANIFEST}: {error}")))?;
    // Any version: a newer one is read like this one, skipping what it added.
    let version = js::safe_integer(manifest.get("version"));
    let created = manifest.get("createdAt").and_then(Value::as_str);
    if manifest.get("format").and_then(Value::as_str) != Some(FORMAT)
        || version.is_none_or(|version| version < 1.0)
        || created.is_none_or(|created| !iso_date(created))
    {
        return Err(invalid("The selected archive is not a supported Hachidori backup."));
    }
    let files = file_list(&manifest)?;
    let dictionaries = manifest.pointer("/snapshot/state/dictionaries").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let [dictionary] = dictionaries else { return Err(invalid(format!("the archive holds {} dictionaries, not one", dictionaries.len()))) };
    let field = |name: &str| dictionary.get(name).and_then(Value::as_str).map(str::to_owned);
    match (field("id"), field("path")) {
        (Some(id), Some(path)) => Ok((Manifest { id, path }, files)),
        _ => Err(invalid(format!("{MANIFEST} names no dictionary id and path"))),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// A one-dictionary archive as a browser host serves it.
    pub(crate) fn archive(id: &str, path: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let listed: Vec<Value> =
            files.iter().map(|(name, bytes)| serde_json::json!({ "path": format!("{PAYLOAD}{name}"), "size": bytes.len() })).collect();
        let manifest = serde_json::json!({
            "format": FORMAT, "version": 3, "createdAt": "2026-11-06T00:00:00.000Z",
            "snapshot": { "state": { "dictionaries": [{ "id": id, "path": path, "title": "T" }] } }, "files": listed,
        });
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let started = writer.start_file(MANIFEST, stored).and_then(|()| Ok(writer.write_all(manifest.to_string().as_bytes())?));
        started.unwrap_or_else(|error| panic!("{error}"));
        for (name, bytes) in files {
            writer.start_file(format!("{PAYLOAD}{name}"), stored).unwrap_or_else(|error| panic!("{error}"));
            writer.write_all(bytes).unwrap_or_else(|error| panic!("{error}"));
        }
        writer.finish().unwrap_or_else(|error| panic!("{error}")).into_inner()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hachidori-relay-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("out")).unwrap_or_else(|error| panic!("{error}"));
        dir
    }

    #[test]
    fn unpacks_the_payload_and_reads_the_manifest() {
        let dir = scratch("archive-ok");
        let bytes =
            archive("abc", "/dicts/.hdw-generation-1/A", &[(".hoshidicts_5", b""), ("index.json", b"{}"), ("sub/blobs.bin", b"xyz")]);
        fs::write(dir.join("a.zip"), bytes).unwrap_or_else(|error| panic!("{error}"));
        let manifest = unpack(&dir.join("a.zip"), &dir.join("out")).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(manifest, Manifest { id: "abc".into(), path: "/dicts/.hdw-generation-1/A".into() });
        assert_eq!(fs::read(dir.join("out/sub/blobs.bin")).ok(), Some(b"xyz".to_vec()));
        assert!(dir.join("out/.hoshidicts_5").is_file());

        // Served again: the same files, with the manifest for the given snapshot.
        fs::write(dir.join("out").join(KEPT_MANIFEST), "{}").unwrap_or_else(|error| panic!("{error}"));
        let entry = serde_json::json!({ "id": "abc", "path": "/dicts/.hdw-generation-1/A", "title": "T", "enabled": false });
        let snapshot = serde_json::json!({ "state": { "dictionaries": [entry] } });
        let size =
            pack(&dir.join("out"), &snapshot, "2026-11-05T00:00:00.000Z", &dir.join("b.zip")).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(fs::metadata(dir.join("b.zip")).map(|metadata| metadata.len()).ok(), Some(size));
        let mut zip = zip::ZipArchive::new(File::open(dir.join("b.zip")).unwrap_or_else(|error| panic!("{error}")))
            .unwrap_or_else(|error| panic!("{error}"));
        let names: Vec<String> = (0..zip.len()).filter_map(|index| Some(zip.by_index(index).ok()?.name().ok()?.into_owned())).collect();
        assert_eq!(names, [MANIFEST, "dictionaries/0/.hoshidicts_5", "dictionaries/0/index.json", "dictionaries/0/sub/blobs.bin"]);
        let mut text = String::new();
        zip.by_name(MANIFEST).and_then(|mut file| Ok(file.read_to_string(&mut text)?)).unwrap_or_else(|error| panic!("{error}"));
        let manifest: Value = serde_json::from_str(&text).unwrap_or_default();
        assert_eq!(manifest["snapshot"]["state"]["dictionaries"], serde_json::json!([entry]));
        assert_eq!(manifest["files"][2], serde_json::json!({ "path": "dictionaries/0/sub/blobs.bin", "size": 3 }));
        fs::create_dir_all(dir.join("again")).unwrap_or_else(|error| panic!("{error}"));
        let again = unpack(&dir.join("b.zip"), &dir.join("again"));
        assert_eq!(again.ok(), Some(Manifest { id: "abc".into(), path: "/dicts/.hdw-generation-1/A".into() }));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_escaping_paths_and_missing_manifests() {
        let dir = scratch("archive-bad");
        fs::write(dir.join("a.zip"), archive("a", "/p", &[("../evil", b"x")])).unwrap_or_else(|error| panic!("{error}"));
        assert!(unpack(&dir.join("a.zip"), &dir.join("out")).is_err());
        assert!(!dir.join("evil").exists());
        fs::write(dir.join("b.zip"), b"not a zip").unwrap_or_else(|error| panic!("{error}"));
        assert!(unpack(&dir.join("b.zip"), &dir.join("out")).is_err());
        assert!(read_manifest(r#"{"format":"hachidori-backup","snapshot":{"state":{"dictionaries":[]}}}"#).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    /// An archive of `entries` (name, bytes, deflated) with `files` as the
    /// manifest's list.
    fn listed(files: &Value, entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let manifest = serde_json::json!({
            "format": FORMAT, "version": 3, "createdAt": "2026-11-06T00:00:00.000Z",
            "snapshot": { "state": { "dictionaries": [{ "id": "a", "path": "/p", "title": "T" }] } }, "files": files,
        });
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file(MANIFEST, stored).unwrap();
        writer.write_all(manifest.to_string().as_bytes()).unwrap();
        for (name, bytes, deflated) in entries {
            let options =
                if *deflated { SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated) } else { stored };
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    // `openBackupArchive`'s refusals, with its wording.
    #[test]
    fn refuses_what_the_extension_refuses_in_a_backup() {
        let dir = scratch("archive-listed");
        let refused = |bytes: Vec<u8>| {
            fs::write(dir.join("x.zip"), bytes).unwrap();
            let _ = fs::remove_dir_all(dir.join("out"));
            fs::create_dir_all(dir.join("out")).unwrap();
            unpack(&dir.join("x.zip"), &dir.join("out")).unwrap_err().to_string()
        };
        let one = serde_json::json!([{ "path": "dictionaries/0/a.bin", "size": 3 }]);
        assert_eq!(
            refused(listed(&one, &[("dictionaries/0/a.bin", b"abc", false), ("dictionaries/0/b.bin", b"x", false)])),
            "The backup contains unlisted or missing files."
        );
        assert_eq!(refused(listed(&one, &[("dictionaries/0/c.bin", b"abc", false)])), "The backup is missing dictionaries/0/a.bin");
        assert_eq!(refused(listed(&one, &[("dictionaries/0/a.bin", b"abcd", false)])), "Incorrect backup file size: dictionaries/0/a.bin");
        assert_eq!(
            refused(listed(&one, &[("dictionaries/0/a.bin", b"abc", true)])),
            "Hachidori backups contain only unencrypted, stored ZIP entries."
        );
        assert_eq!(
            refused(listed(&serde_json::json!([{ "path": "dictionaries/0/a.bin", "size": -1 }]), &[("dictionaries/0/a.bin", b"", false)])),
            "Invalid backup file size."
        );
        assert_eq!(
            refused(listed(&serde_json::json!([{ "path": "other/a.bin", "size": 0 }]), &[("other/a.bin", b"", false)])),
            "Unexpected backup payload path: other/a.bin"
        );
        let nested = serde_json::json!([{ "path": "dictionaries/0/a", "size": 0 }, { "path": "dictionaries/0/a/b", "size": 0 }]);
        assert_eq!(
            refused(listed(&nested, &[("dictionaries/0/a", b"", false), ("dictionaries/0/a/b", b"", false)])),
            "Backup file is also a directory: dictionaries/0/a/b"
        );
        let twice = serde_json::json!([{ "path": "dictionaries/0/a.bin", "size": 0 }, { "path": "dictionaries/0/a.bin", "size": 0 }]);
        assert_eq!(refused(listed(&twice, &[("dictionaries/0/a.bin", b"", false)])), "Duplicate backup file: dictionaries/0/a.bin");
        assert_eq!(refused(listed(&Value::Null, &[("dictionaries/0/a.bin", b"", false)])), "The backup has no file list.");
        let mut undated: Value = serde_json::from_str(r#"{"format":"hachidori-backup","version":3,"files":[]}"#).unwrap();
        assert_eq!(
            read_manifest(&undated.to_string()).unwrap_err().to_string(),
            "The selected archive is not a supported Hachidori backup."
        );
        undated["createdAt"] = Value::from("yesterday");
        assert!(read_manifest(&undated.to_string()).is_err());
        assert!(!payload_path("dictionaries/01/a") && payload_path("dictionaries/12/a") && !payload_path("dictionaries/0/"));
        let _ = fs::remove_dir_all(dir);
    }
}
