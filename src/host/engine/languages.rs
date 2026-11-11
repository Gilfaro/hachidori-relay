// SPDX-License-Identifier: GPL-3.0-or-later
//! The language packs host mode ships, and loading them into the engine
//! (docs/languages.md 9.2 upstream).
//!
//! Japanese is built into the engine. The others are the packs Hachidori's
//! extension ships (its `vendor/languages`, which scripts/sync-languages.mjs
//! copies from Hoshidicts), deflated into the binary by build.rs and loaded on
//! demand with `hdw_language_load`, as `engine-service.js`'s `loadLanguagePack`
//! loads them from the extension's copy.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::OnceLock;

use serde_json::Value;

use super::ffi::Native;
use crate::host::profile::{JAPANESE, language_code};

/// build.rs's `language-packs.bin`: deflated `(code, file name, contents)` records.
static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/language-packs.bin"));

/// Each shipped pack's JSON files, by code and file name.
type Packs = BTreeMap<String, BTreeMap<String, String>>;

#[allow(clippy::expect_used, reason = "build.rs wrote the bundle; it cannot be malformed")]
fn shipped() -> &'static Packs {
    static PACKS: OnceLock<Packs> = OnceLock::new();
    PACKS.get_or_init(|| {
        let mut records = Vec::new();
        flate2::read::DeflateDecoder::new(BUNDLE).read_to_end(&mut records).expect("the shipped language packs inflate");
        let field = |rest: &mut &[u8]| {
            let (length, tail) = rest.split_first_chunk::<4>().expect("a language pack field length");
            let (bytes, tail) = tail.split_at(u32::from_le_bytes(*length) as usize);
            *rest = tail;
            String::from_utf8(bytes.to_vec()).expect("a UTF-8 language pack field")
        };
        let mut rest = records.as_slice();
        let mut packs = Packs::new();
        while !rest.is_empty() {
            let (code, name, contents) = (field(&mut rest), field(&mut rest), field(&mut rest));
            packs.entry(code).or_default().insert(name, contents);
        }
        packs
    })
}

/// `packCodes`: the language packs this host has, sorted, Japanese among them.
pub(crate) fn codes() -> Vec<&'static str> {
    shipped().keys().map(String::as_str).collect()
}

/// `packCodes`: the languages a linked browser is told this host has, Japanese
/// first and then the shipped packs in order.
pub(crate) fn host_languages() -> Vec<&'static str> {
    std::iter::once(JAPANESE).chain(codes().into_iter().filter(|code| *code != JAPANESE)).collect()
}

/// A shipped pack's `language.json` and the transforms file it names (empty when
/// it names none), as `loadLanguagePack` reads them.
fn files(code: &str) -> Result<(&'static str, &'static str), String> {
    let pack = shipped().get(code);
    let read =
        |name: &str| pack.and_then(|files| files.get(name)).map(String::as_str).ok_or_else(|| format!("{code}/{name} could not be read"));
    let language = read("language.json")?;
    let descriptor: Value = serde_json::from_str(language).map_err(|error| format!("{code}/language.json: {error}"))?;
    let transforms = match descriptor.pointer("/lookup/transforms").and_then(Value::as_str) {
        Some(name) if !name.is_empty() => read(name)?,
        _ => "",
    };
    Ok((language, transforms))
}

/// `loadLanguagePack`: loads the shipped pack for `code` unless it is loaded.
/// Japanese is built in.
pub(crate) fn load(native: &mut Native, code: &str) -> Result<(), String> {
    if code == JAPANESE || native.languages.contains(code) {
        return Ok(());
    }
    if !language_code(code) {
        return Err(format!("invalid language code {}", Value::from(code)));
    }
    let (language, transforms) = files(code)?;
    native.language_load(language, transforms)?;
    native.languages.insert(code.to_owned());
    Ok(())
}

/// Upstream's test/node-smoke-languages.mjs at bf1a78f, against the native
/// engine: its fixtures are in tests/fixtures/languages.
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::super::ffi::Kind;
    use super::*;

    const KINDS: [Kind; 4] = [Kind::Term, Kind::Frequency, Kind::Pitch, Kind::Kanji];

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/languages")
    }

    fn read(name: &str) -> String {
        fs::read_to_string(fixtures().join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
    }

    fn sha256(text: &str) -> String {
        Sha256::digest(text.as_bytes()).iter().fold(String::new(), |hex, byte| hex + &format!("{byte:02x}"))
    }

    /// Imports a fixture archive under `root` and adds it as each of `kinds`.
    fn import_and_add(native: &mut Native, root: &Path, file: &str, kinds: &[Kind]) -> String {
        let report: Value =
            serde_json::from_str(&native.import(&fixtures().join(file).to_string_lossy(), &root.to_string_lossy())).unwrap();
        assert_eq!(report["success"], true, "{file}: {report}");
        let path = root.join(report["title"].as_str().unwrap()).to_string_lossy().into_owned();
        add_kinds(native, &path, kinds);
        path
    }

    fn add_kinds(native: &mut Native, path: &str, kinds: &[Kind]) {
        for kind in kinds {
            native.add_dict(path, *kind, false, false).unwrap_or_else(|error| panic!("{path}: {error}"));
        }
    }

    /// The span texts of a segmentation, its offsets being UTF-16.
    fn words(text: &str, reply: &str) -> Vec<String> {
        let units: Vec<u16> = text.encode_utf16().collect();
        let reply: Value = serde_json::from_str(reply).unwrap();
        reply["spans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|span| {
                let offset = |key: &str| usize::try_from(span[key].as_u64().unwrap()).unwrap();
                let start = offset("start");
                String::from_utf16(&units[start..start + offset("length")]).unwrap()
            })
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one ported script, in its order")]
    fn the_language_abi_answers_as_the_extensions_engine_does() {
        let mut native = Native::lock();
        native.reset();
        let root = std::env::temp_dir().join(format!("hachidori-relay-languages-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        native.init_storage(&root.to_string_lossy()).unwrap();

        // Japanese requests without language keys: today's bytes.
        let options = json!({ "frequencyDictionary": "", "frequencyOrder": "auto", "primaryReading": "", "metadataOnlyTerms": false });
        let golden: Value = serde_json::from_str(&read("languages-japanese-replies.json")).unwrap();
        let japanese = import_and_add(&mut native, &root, "hachidori-fixture.zip", &KINDS);
        let japanese_replies = |native: &Native| -> Vec<String> {
            golden["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|request| {
                    let (text, options) = (request["text"].as_str().unwrap(), request["options"].as_str().unwrap());
                    match request["name"].as_str().unwrap() {
                        "hdw_lookup" => native.lookup(text, 32, 16, options),
                        "hdw_lookup_dictionary" => native.lookup_dictionary(text, &japanese, 32, 16, options),
                        "hdw_segment" => native.segment(text, 16, options),
                        _ => native.kanji(text),
                    }
                    .unwrap()
                })
                .collect()
        };
        let japanese_only = japanese_replies(&native);
        for (reply, request) in japanese_only.iter().zip(golden["requests"].as_array().unwrap()) {
            assert_eq!(sha256(reply), request["sha256"], "{} {}: {}", request["name"], request["text"], &reply[..reply.len().min(300)]);
        }
        let recorded: Value = serde_json::from_str(golden["requests"][1]["options"].as_str().unwrap()).unwrap();
        assert_eq!(recorded, options, "the recorded hover options");

        // hdw_language_load and hdw_languages.
        assert_eq!(native.languages().unwrap(), r#"["ja"]"#, "only the built-in Japanese pack at start");
        let pack = |code: &str| (read(&format!("packs/{code}/language.json")), read(&format!("packs/{code}/transforms.json")));
        assert!(native.language_load("{", "").is_err(), "malformed JSON");
        let (english, english_transforms) = pack("en");
        let mut unknown: Value = serde_json::from_str(&english).unwrap();
        unknown["unknownKey"] = json!(true);
        assert!(native.language_load(&unknown.to_string(), &english_transforms).is_err(), "an unknown key");
        for code in ["en", "ar"] {
            let (language, transforms) = pack(code);
            native.language_load(&language, &transforms).unwrap_or_else(|error| panic!("{code}: {error}"));
        }
        assert_eq!(native.languages().unwrap(), r#"["ar","en","ja"]"#);
        native.reset();
        assert_eq!(native.languages().unwrap(), r#"["ar","en","ja"]"#, "a pack survives hdw_reset");

        // hdw_deinflect.
        let deinflected: Value = serde_json::from_str(&native.deinflect("walked", "en").unwrap()).unwrap();
        assert!(
            deinflected["results"].as_array().unwrap().iter().any(|result| result["text"] == "walk" && result["trace"] == json!(["past"])),
            "{deinflected}"
        );
        assert!(native.deinflect("walked", "zz").is_err(), "an unknown language");

        // Lookups with language and dictionaries; the reset unloaded the Japanese fixture, whose files stay.
        add_kinds(&mut native, &japanese, &KINDS);
        let english = import_and_add(&mut native, &root, "hachidori-english-fixture.zip", &[Kind::Term]);
        let arabic = import_and_add(&mut native, &root, "hachidori-arabic-fixture.zip", &[Kind::Term]);
        let with_language = |language: &str, dictionaries: &[&str]| {
            let mut options = options.clone();
            options["language"] = json!(language);
            options["dictionaries"] = json!(dictionaries);
            options.to_string()
        };
        let lookup = |native: &Native, text: &str, options: &str| -> Value {
            serde_json::from_str(&native.lookup(text, 32, 16, options).unwrap()).unwrap()
        };
        let first = |reply: &Value| reply["results"][0]["term"]["expression"].clone();
        let running = lookup(&native, "running", &with_language("en", &[&english]));
        assert_eq!(first(&running), "run", "{running}");
        assert_eq!(
            running["results"][0]["trace"].as_array().unwrap().iter().map(|step| step["name"].clone()).collect::<Vec<_>>(),
            [json!("-ing")]
        );
        assert_eq!(first(&lookup(&native, "books", &with_language("en", &[&english]))), "book");
        assert_eq!(first(&lookup(&native, "went", &with_language("en", &[&english]))), "go");
        for text in ["\u{0627}\u{0644}\u{0643}\u{062A}\u{0627}\u{0628}", "\u{0643}\u{0650}\u{062A}\u{064E}\u{0627}\u{0628}"] {
            assert_eq!(first(&lookup(&native, text, &with_language("ar", &[&arabic]))), "\u{0643}\u{062A}\u{0627}\u{0628}", "{text}");
        }
        assert_eq!(lookup(&native, "running", &with_language("en", &[]))["results"], json!([]), "an empty selection");
        assert_eq!(lookup(&native, "食べた", &with_language("ja", &[]))["results"], json!([]));
        let alone: Value = serde_json::from_str(&japanese_only[1]).unwrap();
        assert_eq!(
            lookup(&native, "食べたかった", &with_language("ja", &[&japanese]))["results"],
            alone["results"],
            "a selection is as if only it were loaded"
        );
        assert_eq!(first(&lookup(&native, "running", &with_language("en", &[&english, &japanese]))), "run");
        assert_eq!(first(&lookup(&native, "running", &with_language("en", &["/dicts/not-loaded", &english]))), "run", "a path not loaded");
        assert!(native.lookup("running", 32, 16, &with_language("zz", &[&english])).is_err(), "an unknown language");
        let dictionary = |native: &Native, path: &str, selection: &str| -> Value {
            serde_json::from_str(&native.lookup_dictionary("running", path, 32, 16, &with_language("en", &[selection])).unwrap()).unwrap()
        };
        assert_eq!(first(&dictionary(&native, &english, &english)), "run");
        assert_eq!(dictionary(&native, &english, &arabic)["results"], json!([]), "outside the selection");
        let kanji = |native: &Native, selection: &[&str]| -> Value {
            serde_json::from_str(
                &native.kanji_with_options("食", &json!({ "language": "ja", "dictionaries": selection }).to_string()).unwrap(),
            )
            .unwrap()
        };
        assert_ne!(kanji(&native, &[&japanese])["entries"], json!([]));
        assert_eq!(kanji(&native, &[&english])["entries"], json!([]));
        assert_eq!(native.kanji_with_options("食", "").unwrap(), native.kanji("食").unwrap(), "no options is hdw_kanji");

        // Segmentation per language.
        let text = "she was running";
        assert!(words(text, &native.segment(text, 16, &with_language("en", &[&english])).unwrap()).contains(&"running".to_owned()));
        let text = "\u{0627}\u{0644}\u{0643}\u{062A}\u{0627}\u{0628} \u{0628}\u{064A}\u{062A}";
        assert_eq!(
            words(text, &native.segment(text, 16, &with_language("ar", &[&arabic])).unwrap()),
            ["\u{0627}\u{0644}\u{0643}\u{062A}\u{0627}\u{0628}", "\u{0628}\u{064A}\u{062A}"]
        );
        let spans = |reply: &str| serde_json::from_str::<Value>(reply).unwrap()["spans"].clone();
        assert_eq!(
            spans(&native.segment("漢字を読んで食べた。", 16, &with_language("ja", &[&japanese])).unwrap()),
            spans(&japanese_only[7])
        );

        // Every shipped pack loads, and deinflects its warm-up sample.
        let shipped = codes();
        assert!(shipped.contains(&JAPANESE), "{shipped:?}");
        assert!(shipped.len() > 20, "{shipped:?}");
        assert_eq!(host_languages()[0], JAPANESE);
        assert_eq!(host_languages().len(), shipped.len());
        for code in shipped.iter().copied().filter(|code| *code != JAPANESE) {
            native.languages.remove(code);
            load(&mut native, code).unwrap_or_else(|error| panic!("{code}: {error}"));
            let (language, _) = files(code).unwrap();
            let descriptor: Value = serde_json::from_str(language).unwrap();
            let sample = descriptor.pointer("/host/sample/warmup").and_then(Value::as_str).filter(|sample| !sample.is_empty());
            let sample = sample.unwrap_or_else(|| panic!("{code}: no host.sample.warmup"));
            let reply: Value =
                serde_json::from_str(&native.deinflect(sample, code).unwrap_or_else(|error| panic!("{code}: {error}"))).unwrap();
            assert!(reply["results"].as_array().is_some_and(|results| !results.is_empty()), "{code}: {reply}");
        }
        assert_eq!(load(&mut native, "x1"), Err("invalid language code \"x1\"".to_owned()));
        assert_eq!(load(&mut native, "qq"), Err("qq/language.json could not be read".to_owned()));
        assert_eq!(load(&mut native, JAPANESE), Ok(()));

        native.reset();
        drop(native);
        let _ = fs::remove_dir_all(&root);
    }
}
