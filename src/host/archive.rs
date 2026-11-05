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

/// Unpacks the dictionary in `archive` into the empty directory `into`, and returns
/// its manifest. Every entry's checksum is verified; an archive with paths outside
/// `dictionaries/0/` or with more than one dictionary is refused.
pub(crate) fn unpack(archive: &Path, into: &Path) -> io::Result<Manifest> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?).map_err(|error| invalid(format!("not a dictionary archive: {error}")))?;
    let mut manifest = None;
    let mut files = 0usize;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| invalid(error.to_string()))?;
        let name = entry.name().map_err(|error| invalid(error.to_string()))?.into_owned();
        if name == MANIFEST {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            manifest = Some(read_manifest(&text)?);
            continue;
        }
        let Some(relative) = name.strip_prefix(PAYLOAD).filter(|relative| !relative.is_empty()) else {
            return Err(invalid(format!("unexpected archive entry {name:?}")));
        };
        let relative = Path::new(relative);
        if relative == Path::new(KEPT_MANIFEST) {
            return Err(invalid(format!("unexpected archive entry {name:?}")));
        }
        if !relative.components().all(|part| matches!(part, Component::Normal(_))) {
            return Err(invalid(format!("unsafe archive path {name:?}")));
        }
        let target = into.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // Reading to the end checks the entry's CRC32.
        io::copy(&mut entry, &mut File::create(&target)?)?;
        files += 1;
    }
    let manifest = manifest.ok_or_else(|| invalid(format!("the archive has no {MANIFEST}")))?;
    if files == 0 {
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

fn read_manifest(text: &str) -> io::Result<Manifest> {
    let manifest: Value = serde_json::from_str(text).map_err(|error| invalid(format!("{MANIFEST}: {error}")))?;
    if manifest.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(invalid(format!("{MANIFEST} is not a {FORMAT}")));
    }
    let dictionaries = manifest.pointer("/snapshot/state/dictionaries").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let [dictionary] = dictionaries else { return Err(invalid(format!("the archive holds {} dictionaries, not one", dictionaries.len()))) };
    let field = |name: &str| dictionary.get(name).and_then(Value::as_str).map(str::to_owned);
    match (field("id"), field("path")) {
        (Some(id), Some(path)) => Ok(Manifest { id, path }),
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
        let manifest = serde_json::json!({
            "format": FORMAT, "version": 3,
            "snapshot": { "state": { "dictionaries": [{ "id": id, "path": path, "title": "T" }] } },
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
}
