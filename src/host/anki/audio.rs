// SPDX-License-Identifier: GPL-3.0-or-later
//! A note's pronunciation, downloaded from the enabled audio sources: the
//! offscreen document's `exportAnkiAudio` (`anki-audio.js`), with the audio
//! repository's candidates (`audio-repository.js`) and the sources' URL rules
//! (`audio-sources.js`).
//!
//! The browser decodes each recording before accepting it; the relay has no
//! decoder, so it accepts what is served as audio or starts like an audio file.
//! Browser text-to-speech never reaches here: this host's options drop it.

use std::io::Read as _;
use std::time::{Duration, Instant};

use base64::Engine as _;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::gateway::{Failure, Result};
use crate::host::js;
use crate::host::settings::hex;

/// `exportAnkiAudio`'s `AbortSignal.timeout(30_000)`.
const DEADLINE: Duration = Duration::from_secs(30);
/// The largest recording or pronunciation list the relay downloads.
pub(crate) const MAX_RECORDING_BYTES: u64 = 32 * 1024 * 1024;
const TIMED_OUT: &str = "signal timed out";
const MIME_EXTENSIONS: [(&str, &str); 9] = [
    ("audio/aac", "aac"),
    ("audio/flac", "flac"),
    ("audio/mp4", "m4a"),
    ("audio/mpeg", "mp3"),
    ("audio/ogg", "ogg"),
    ("audio/wav", "wav"),
    ("audio/webm", "webm"),
    ("audio/x-wav", "wav"),
    ("application/ogg", "ogg"),
];
/// `encodeURIComponent`, and `!'()*` as well.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// A downloaded pronunciation: its Anki filename, its bytes in base64, and the
/// source it came from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AudioFile {
    pub(crate) filename: String,
    pub(crate) data: String,
    pub(crate) source_id: Value,
}

/// One pronunciation a source offers.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    url: Option<String>,
    name: String,
}

/// `httpUrl`.
fn http_url(value: &str) -> Result<String> {
    if value.chars().any(|c| c <= '\u{1f}') {
        return Err(Failure::new("Audio URLs must be absolute HTTP(S) URLs."));
    }
    let url = url::Url::parse(value).map_err(|_| Failure::new("Invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() || url.port() == Some(0) {
        return Err(Failure::new("Audio URLs must use HTTP(S) without a username or password."));
    }
    if url.host_str().is_some_and(|host| host.contains(['{', '}'])) {
        return Err(Failure::new("Audio URL placeholders cannot appear in the host."));
    }
    Ok(url.to_string())
}

fn encode_value(value: &str) -> String {
    utf8_percent_encode(value, COMPONENT).to_string()
}

/// `pronunciationLanguage`: the speech name and URL language of a profile's
/// pack (upstream docs/languages.md 13). A language without a pack, or a
/// request that names none, keeps Japanese.
#[derive(Clone, Copy)]
pub(crate) struct Pronunciation {
    name: &'static str,
    url: &'static str,
}

impl Pronunciation {
    pub(crate) const JAPANESE: Self = Self { name: "Japanese", url: "ja" };

    pub(crate) fn of(code: Option<&str>) -> Self {
        let Some(pack) = code.and_then(crate::host::registry::get) else { return Self::JAPANESE };
        Self { name: pack["name"].as_str().unwrap_or_default(), url: pack["host"]["audioLanguage"].as_str().unwrap_or_default() }
    }
}

/// `audioSourceUrl`: the source's URL Template with the term's placeholders
/// filled, `{language}` with the pack's `host.audioLanguage`.
fn audio_source_url(template: &str, term: &Value, language: Pronunciation) -> Result<String> {
    http_url(template)?;
    let expression = encode_value(&js::to_string(term.get("expression")));
    let reading = encode_value(&js::to_string(term.get("reading")));
    let mut out = String::new();
    let mut rest = template;
    // `/\{([^{}]*)\}/gu`: the innermost braces, left to right.
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after.find(['{', '}']);
        match close {
            Some(close) if after.as_bytes()[close] == b'}' => {
                let key = &after[..close];
                out.push_str(&rest[..open]);
                match key {
                    "term" | "expression" => out.push_str(&expression),
                    "reading" => out.push_str(&reading),
                    "language" => out.push_str(&encode_value(language.url)),
                    _ => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    http_url(&out)
}

/// `parseAudioSourceList`: a Yomitan `audioSourceList`.
fn parse_audio_source_list(value: &Value) -> Result<Vec<Candidate>> {
    let list = value.as_object().filter(|list| {
        list.get("type").and_then(Value::as_str) == Some("audioSourceList")
            && list.get("audioSources").is_some_and(Value::is_array)
            && list.keys().all(|key| key == "type" || key == "audioSources")
    });
    let Some(list) = list else {
        return Err(Failure::new("The audio provider returned an invalid Yomitan audioSourceList."));
    };
    list["audioSources"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|candidate| {
            let valid = candidate.as_object().filter(|candidate| {
                candidate.get("url").is_some_and(Value::is_string)
                    && candidate.get("name").is_none_or(Value::is_string)
                    && candidate.keys().all(|key| key == "url" || key == "name")
            });
            let Some(candidate) = valid else {
                return Err(Failure::new("The audio provider returned an invalid pronunciation candidate."));
            };
            Ok(Candidate {
                url: Some(http_url(candidate["url"].as_str().unwrap_or_default())?),
                name: candidate.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// A finished download: the body, and its `Content-Type`.
struct Downloaded {
    bytes: Vec<u8>,
    content_type: Option<String>,
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()).ok_or_else(|| Failure::new(TIMED_OUT))
}

/// `response`: `fetch(url, { credentials: "omit" })`, its body read whole.
fn download(url: &str, deadline: Instant, what: &str) -> Result<Downloaded> {
    let left = remaining(deadline)?;
    let agent = crate::host::loopback::agent(
        ureq::Agent::config_builder().max_redirects(20).http_status_as_error(false).timeout_global(Some(left)).build(),
    );
    let failed = |error: &ureq::Error| {
        if matches!(error, ureq::Error::Timeout(_)) {
            Failure::new(TIMED_OUT)
        } else {
            Failure::new(format!("{what} could not be downloaded (Failed to fetch)."))
        }
    };
    let response = agent.get(url).call().map_err(|error| failed(&error))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Failure::new(format!("{what} returned HTTP {}.", status.as_u16())));
    }
    let content_type = response.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_lowercase);
    let mut bytes = Vec::new();
    let read = response.into_body().into_reader().take(MAX_RECORDING_BYTES + 1).read_to_end(&mut bytes);
    if read.is_err() {
        remaining(deadline)?;
        return Err(Failure::new(format!("{what} could not be downloaded (Failed to fetch).")));
    }
    if bytes.len() as u64 > MAX_RECORDING_BYTES {
        return Err(Failure::new(format!("{what} is larger than the {} MiB the Hachidori relay downloads.", MAX_RECORDING_BYTES >> 20)));
    }
    Ok(Downloaded { bytes, content_type })
}

/// `repository.candidates`: what `source` offers for `term`.
fn candidates(source: &Map<String, Value>, term: &Value, language: Pronunciation, deadline: Instant) -> Result<Vec<Candidate>> {
    remaining(deadline)?;
    let kind = js::to_string(source.get("type"));
    if kind.starts_with("text-to-speech") {
        let voice = source.get("voice").filter(|voice| js::truthy(Some(voice)));
        return Ok(vec![Candidate {
            url: None,
            name: voice.map_or_else(|| format!("Automatic {}", language.name), |voice| js::to_string(Some(voice))),
        }]);
    }
    let Some(template) = source.get("url").filter(|url| js::truthy(Some(url))) else { return Ok(Vec::new()) };
    let url = audio_source_url(&js::to_string(Some(template)), term, language)?;
    if kind == "custom" {
        return Ok(vec![Candidate { url: Some(url), name: String::new() }]);
    }
    let list = download(&url, deadline, "The pronunciation list")?;
    let Ok(value) = serde_json::from_slice::<Value>(&list.bytes) else {
        let detail = list.content_type.map(|kind| format!(" ({kind})")).unwrap_or_default();
        return Err(Failure::new(format!(
            "The pronunciation list is not JSON{detail}. If this URL plays a recording itself, set this source's type to Audio URL in Audio \
             Settings."
        )));
    };
    parse_audio_source_list(&value)
}

/// Whether `bytes` start like a recording a browser would play.
fn looks_like_audio(bytes: &[u8]) -> bool {
    let starts = |prefix: &[u8]| bytes.starts_with(prefix);
    starts(b"ID3")
        || starts(b"RIFF")
        || starts(b"OggS")
        || starts(b"fLaC")
        || starts(b"ADIF")
        || starts(&[0x1a, 0x45, 0xdf, 0xa3])
        || bytes.get(4..8) == Some(b"ftyp")
        || (bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
}

/// `undecodableRecording`, for a body the relay does not recognise as audio.
fn unusable_recording(download: &Downloaded) -> Failure {
    let head = &download.bytes[..download.bytes.len().min(64)];
    if String::from_utf8_lossy(head).trim_start().starts_with('{')
        && serde_json::from_slice::<Value>(&download.bytes)
            .is_ok_and(|value| value.get("type").and_then(Value::as_str) == Some("audioSourceList"))
    {
        return Failure::new(
            "The URL returned a Yomitan audio list, not a recording. If it is a list link, set this source's type to Yomitan JSON in Audio \
             Settings.",
        );
    }
    let kind = download
        .content_type
        .as_ref()
        .filter(|kind| !kind.is_empty())
        .map_or_else(|| "with no content type".to_owned(), |kind| format!("of {kind}"));
    Failure::new(format!(
        "The Hachidori relay could not recognise the recording as audio. The provider sent {} bytes {kind}.",
        download.bytes.len()
    ))
}

/// `candidateFile`: the candidate's recording, named by its bytes.
fn candidate_file(candidate: &Candidate, deadline: Instant) -> Result<(String, String)> {
    remaining(deadline)?;
    let url = candidate.url.as_deref().ok_or_else(|| Failure::new("The pronunciation could not be decoded."))?;
    let download = download(url, deadline, "The recording")?;
    let mime = download.content_type.as_deref().unwrap_or_default().split(';').next().unwrap_or_default().trim().to_owned();
    let served_as_audio = mime.starts_with("audio/") || mime == "application/ogg";
    if download.bytes.is_empty() || !(served_as_audio || looks_like_audio(&download.bytes)) {
        return Err(unusable_recording(&download));
    }
    let suffix = url::Url::parse(url).map(|url| url.path().rsplit('.').next().unwrap_or_default().to_lowercase()).unwrap_or_default();
    let fallback = if !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()) {
        suffix
    } else {
        "bin".to_owned()
    };
    let extension = MIME_EXTENSIONS.iter().find(|(known, _)| *known == mime).map_or(fallback, |(_, extension)| (*extension).to_owned());
    let filename = format!("hachidori_{}.{extension}", hex(&Sha256::digest(&download.bytes)));
    Ok((filename, base64::engine::general_purpose::STANDARD.encode(&download.bytes)))
}

/// `selectedAudioPlan`: the reader's chosen pronunciation, if it is still offered.
///
/// The selection's `sourceKey` is the browser's `JSON.stringify(source)`, in the
/// order the browser keeps the source's keys; `serde_json` keeps them sorted, so
/// the source is compared as JSON (`sameJsonValue`) rather than as that text.
fn selected_plan(
    sources: &[Value],
    term: &Value,
    selection: &Value,
    language: Pronunciation,
    deadline: Instant,
) -> Result<(Value, Candidate)> {
    let key = selection.get("sourceKey").and_then(Value::as_str).and_then(|key| serde_json::from_str::<Value>(key).ok());
    let source = sources.iter().find(|source| {
        js::strict_eq(source.get("id"), selection.get("sourceId")) && key.as_ref().is_some_and(|key| js::same_json(key, source))
    });
    let current = |field: &str| js::strict_eq(selection.get(field), term.get(field));
    let Some(source) = source.filter(|_| current("expression") && current("reading")) else {
        return Err(Failure::new("This pronunciation selection is no longer current. Choose it again."));
    };
    let offered = candidates(source.as_object().unwrap_or(&Map::new()), term, language, deadline)?;
    let candidate = selection
        .get("index")
        .and_then(Value::as_f64)
        .filter(|index| index.fract() == 0.0 && *index >= 0.0 && *index < 4_294_967_296.0)
        .and_then(|index| {
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a whole array index")]
            offered.get(index as usize)
        })
        .filter(|candidate| {
            let url = candidate.url.as_ref().map_or(Value::Null, |url| Value::String(url.clone()));
            js::strict_eq(Some(&url), selection.get("url"))
                && selection.get("name").and_then(Value::as_str) == Some(candidate.name.as_str())
        });
    let Some(candidate) = candidate else { return Err(Failure::new("The provider's pronunciation choices changed. Choose again.")) };
    Ok((source.clone(), candidate.clone()))
}

/// `exportAnkiAudio`: the first pronunciation the sources (or the reader's
/// selection) offer that downloads, in the pronunciation language of the
/// note's profile.
pub(crate) fn export(sources: &[Value], term: &Value, selection: Option<&Value>, language: Pronunciation) -> Result<AudioFile> {
    let deadline = Instant::now() + DEADLINE;
    let (sources, chosen) = match selection.filter(|selection| js::truthy(Some(selection))) {
        Some(selection) => {
            let (source, candidate) = selected_plan(sources, term, selection, language, deadline)?;
            (vec![source], Some(candidate))
        }
        None => (sources.to_vec(), None),
    };
    let mut failure = None;
    for source in &sources {
        let empty = Map::new();
        let fields = source.as_object().unwrap_or(&empty);
        if js::to_string(fields.get("type")).starts_with("text-to-speech") {
            failure = Some(Failure::new("Browser text-to-speech recording is unavailable."));
            continue;
        }
        let offered = match &chosen {
            Some(candidate) => vec![candidate.clone()],
            None => match candidates(fields, term, language, deadline) {
                Ok(offered) => offered,
                Err(error) => {
                    remaining(deadline)?;
                    failure = Some(error);
                    continue;
                }
            },
        };
        for candidate in &offered {
            match candidate_file(candidate, deadline) {
                Ok((filename, data)) => {
                    return Ok(AudioFile { filename, data, source_id: fields.get("id").cloned().unwrap_or(Value::Null) });
                }
                Err(error) => {
                    remaining(deadline)?;
                    failure = Some(error);
                }
            }
        }
    }
    Err(failure.unwrap_or_else(|| Failure::new("No downloadable pronunciation is available for this result.")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A loopback server answering one request with `status`, `content_type`
    /// and `body`; `None` for a URL nothing listens at.
    fn answer(status: Option<u16>, content_type: &str, body: &'static [u8]) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/a", listener.local_addr().unwrap());
        let Some(status) = status else { return url };
        let content_type = content_type.to_owned();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0u8; 4096]);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(body);
        });
        url
    }

    /// Upstream's audio-repository.test.mjs and anki-audio.test.mjs (#499):
    /// each failure says which request failed, with its status or network error,
    /// and a list and a recording saved as each other's type are explained.
    #[test]
    fn failures_say_which_request_failed() {
        let term = json!({ "expression": "聞く", "reading": "きく" });
        let deadline = || Instant::now() + DEADLINE;
        let list = |url: String| json!({ "id": "list", "type": "url", "url": url });
        let listed =
            |source: Value| candidates(source.as_object().unwrap(), &term, Pronunciation::JAPANESE, deadline()).unwrap_err().message;
        assert_eq!(listed(list(answer(Some(403), "text/plain", b""))), "The pronunciation list returned HTTP 403.");
        assert_eq!(
            listed(list(answer(Some(200), "audio/mpeg", b"ID3\x04"))),
            "The pronunciation list is not JSON (audio/mpeg). If this URL plays a recording itself, set this source's type to Audio URL \
             in Audio Settings."
        );
        let recording = |url: String| {
            let source = json!({ "id": "rec", "type": "custom", "url": url });
            export(&[source], &term, None, Pronunciation::JAPANESE).unwrap_err().message
        };
        assert_eq!(recording(answer(Some(404), "audio/mpeg", b"")), "The recording returned HTTP 404.");
        // Nothing listens once the listener is dropped.
        assert_eq!(recording(answer(None, "", b"")), "The recording could not be downloaded (Failed to fetch).");
        assert_eq!(
            recording(answer(Some(200), "application/json", br#"{"type":"audioSourceList","audioSources":[]}"#)),
            "The URL returned a Yomitan audio list, not a recording. If it is a list link, set this source's type to Yomitan JSON in \
             Audio Settings."
        );
    }

    #[test]
    fn fills_url_templates() {
        let term = json!({ "expression": "猫 (cat)!", "reading": "ねこ" });
        assert_eq!(
            audio_source_url("https://a.test/{term}/{reading}?l={language}&x={other}&y={", &term, Pronunciation::JAPANESE).unwrap(),
            "https://a.test/%E7%8C%AB%20%28cat%29%21/%E3%81%AD%E3%81%93?l=ja&x={other}&y={"
        );
        // A profile's pack fills {language}; a language without one keeps Japanese.
        for (code, expected) in [(Some("ar"), "ar"), (Some("grc"), "grc"), (Some("xx"), "ja"), (None, "ja")] {
            assert_eq!(
                audio_source_url("https://a.test/?l={language}", &term, Pronunciation::of(code)).unwrap(),
                format!("https://a.test/?l={expected}")
            );
        }
        let speech = json!({ "type": "text-to-speech", "voice": "" });
        let names = |code| {
            candidates(speech.as_object().unwrap(), &term, Pronunciation::of(code), Instant::now() + DEADLINE).unwrap()[0].name.clone()
        };
        assert_eq!(names(Some("en")), "Automatic English");
        assert_eq!(names(None), "Automatic Japanese");
        assert_eq!(
            audio_source_url("ftp://a.test/{term}", &term, Pronunciation::JAPANESE).unwrap_err().message,
            "Audio URLs must use HTTP(S) without a username or password."
        );
        assert_eq!(
            audio_source_url("https://u:p@a.test/", &term, Pronunciation::JAPANESE).unwrap_err().message,
            "Audio URLs must use HTTP(S) without a username or password."
        );
        assert_eq!(audio_source_url("nope", &term, Pronunciation::JAPANESE).unwrap_err().message, "Invalid URL");
    }

    #[test]
    fn a_played_pronunciation_is_the_one_chosen_whatever_the_key_order() {
        // As the browser keeps a source, and as its JSON.stringify writes it.
        let source = json!({ "id": "s1", "type": "custom", "url": "https://a.test/{term}.mp3", "enabled": true, "volume": 1.0 });
        let key = r#"{"id":"s1","type":"custom","url":"https://a.test/{term}.mp3","enabled":true,"volume":1}"#;
        assert_ne!(source.to_string(), key, "serde_json sorts the keys");
        let term = json!({ "expression": "猫", "reading": "ねこ" });
        let selection = json!({ "sourceId": "s1", "sourceKey": key, "expression": "猫", "reading": "ねこ", "index": 0,
            "url": "https://a.test/%E7%8C%AB.mp3", "name": "" });
        let deadline = Instant::now() + DEADLINE;
        let (chosen, candidate) =
            selected_plan(std::slice::from_ref(&source), &term, &selection, Pronunciation::JAPANESE, deadline).unwrap();
        assert_eq!((chosen, candidate.url.as_deref()), (source, Some("https://a.test/%E7%8C%AB.mp3")));
        // A source changed since the choice is not it.
        let changed = json!({ "id": "s1", "type": "custom", "url": "https://b.test/{term}.mp3", "enabled": true, "volume": 1 });
        let error = selected_plan(&[changed], &term, &selection, Pronunciation::JAPANESE, deadline).unwrap_err();
        assert_eq!(error.message, "This pronunciation selection is no longer current. Choose it again.");
    }

    #[test]
    fn reads_audio_source_lists() {
        let list = json!({ "type": "audioSourceList", "audioSources": [{ "url": "https://a.test/1.mp3", "name": "One" }, { "url": "https://a.test/2" }] });
        let found = parse_audio_source_list(&list).unwrap();
        assert_eq!(found[1], Candidate { url: Some("https://a.test/2".to_owned()), name: String::new() });
        assert!(parse_audio_source_list(&json!({ "type": "audioSourceList", "audioSources": [], "x": 1 })).is_err());
        assert!(parse_audio_source_list(&json!({ "type": "audioSourceList", "audioSources": [{ "url": 1 }] })).is_err());
    }

    #[test]
    fn recognises_recordings() {
        assert!(looks_like_audio(b"ID3\x04rest"));
        assert!(looks_like_audio(&[0xff, 0xfb, 0x90]));
        assert!(looks_like_audio(b"\0\0\0\x18ftypM4A "));
        assert!(!looks_like_audio(b"<html>"));
        let list = Downloaded { bytes: br#" {"type":"audioSourceList","audioSources":[]}"#.to_vec(), content_type: None };
        assert!(unusable_recording(&list).message.contains("Yomitan audio list"));
        let page = Downloaded { bytes: b"<html>".to_vec(), content_type: Some("text/html".to_owned()) };
        assert!(unusable_recording(&page).message.ends_with("sent 6 bytes of text/html."));
    }
}
