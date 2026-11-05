// SPDX-License-Identifier: GPL-3.0-or-later
//! Routing, input validation and the reshaping of host replies into Yomitan's answers.
//!
//! Every answer is JSON except a dictionary download. Every answer allows any
//! origin. Errors are `{"error": ...}` with the statuses listed in the README.

use std::io;
use std::sync::Arc;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{GeneralPurpose, GeneralPurposeConfig};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::body::Body;
use super::session::{Late, Session};
use super::{ApiError, Server};
use crate::json;

/// The largest request body the API reads.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// How much of a dictionary each read asks the host for.
const CHUNK_LENGTH: u64 = 4 * 1024 * 1024;

/// Python's `b64decode(validate=True)`: the standard alphabet with padding, and lenient
/// about the unused trailing bits.
const BASE64: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true));

/// What `urllib.parse.quote(name, safe="")` leaves alone.
const FILENAME_ESCAPES: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    ServerVersion,
    YomitanVersion,
    TermEntries,
    KanjiEntries,
    AnkiFields,
    AnkiCardFormats,
    Tokenize,
    Dictionaries,
}

/// The fixed paths. `/dictionaries/<id>` is matched separately.
const ROUTES: [(&str, Endpoint); 8] = [
    ("/serverVersion", Endpoint::ServerVersion),
    ("/yomitanVersion", Endpoint::YomitanVersion),
    ("/termEntries", Endpoint::TermEntries),
    ("/kanjiEntries", Endpoint::KanjiEntries),
    ("/ankiFields", Endpoint::AnkiFields),
    ("/ankiCardFormats", Endpoint::AnkiCardFormats),
    ("/tokenize", Endpoint::Tokenize),
    ("/dictionaries", Endpoint::Dictionaries),
];

impl Endpoint {
    fn method(self) -> Method {
        if self == Self::Dictionaries { Method::GET } else { Method::POST }
    }
}

/// Answers one request. Never fails: every error becomes a JSON answer.
pub(crate) async fn handle(server: &Server, request: Request<Incoming>) -> Response<Body> {
    if request.method() == Method::OPTIONS {
        return preflight();
    }
    // Upstream's handler has do_GET/POST/HEAD/PUT/DELETE/PATCH/OPTIONS. Any other
    // method is a 501 from http.server, whatever the path.
    if ![Method::GET, Method::POST, Method::HEAD, Method::PUT, Method::DELETE, Method::PATCH].contains(request.method()) {
        let message = format!("Unsupported method ('{}')", request.method());
        return error_response(&ApiError::new(StatusCode::NOT_IMPLEMENTED, message));
    }
    route(server, request).await.unwrap_or_else(|error| error_response(&error))
}

async fn route(server: &Server, request: Request<Incoming>) -> Result<Response<Body>, ApiError> {
    let path = request.uri().path().to_owned();
    if let Some(&(_, endpoint)) = ROUTES.iter().find(|(known, _)| *known == path) {
        let expected = endpoint.method();
        if request.method() != expected {
            return Err(ApiError::new(StatusCode::METHOD_NOT_ALLOWED, format!("{path} accepts {expected} only")));
        }
        if endpoint == Endpoint::Dictionaries {
            return dictionaries(server).await;
        }
        let body = read_json(request).await?;
        return match endpoint {
            Endpoint::ServerVersion => Ok(json_response(StatusCode::OK, &json!({ "version": server.version }))),
            Endpoint::YomitanVersion => yomitan_version(server).await,
            Endpoint::TermEntries => term_entries(server, &body).await,
            Endpoint::KanjiEntries => kanji_entries(server, &body).await,
            Endpoint::AnkiFields => anki_fields(server, &body).await,
            Endpoint::AnkiCardFormats => anki_card_formats(server, &body).await,
            Endpoint::Tokenize => tokenize(server, &body).await,
            Endpoint::Dictionaries => Err(ApiError::unknown_path()),
        };
    }
    if let Some(rest) = path.strip_prefix("/dictionaries/") {
        if request.method() != Method::GET {
            return Err(ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "/dictionaries/<id> accepts GET only"));
        }
        let id = percent_decode_str(rest).decode_utf8_lossy();
        if id.is_empty() || id.contains('/') {
            return Err(ApiError::unknown_path());
        }
        return download(server, &id).await;
    }
    Err(ApiError::unknown_path())
}

// Responses

fn base(status: StatusCode) -> hyper::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::CACHE_CONTROL, "no-store")
}

fn finish(builder: hyper::http::response::Builder, body: Body) -> Response<Body> {
    // Every header set here is a valid value, so building cannot fail. Fall back to a
    // bare 500 rather than panicking anyway.
    builder.body(body).unwrap_or_else(|_| {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        response
    })
}

fn json_bytes(status: StatusCode, bytes: Bytes) -> Response<Body> {
    let builder = base(status).header(header::CONTENT_TYPE, "application/json; charset=utf-8").header(header::CONTENT_LENGTH, bytes.len());
    finish(builder, Body::full(bytes))
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    json_bytes(status, serde_json::to_vec(value).map(Bytes::from).unwrap_or_default())
}

/// A raw slice of a host reply, passed on unchanged.
fn raw_response(raw: &RawValue) -> Response<Body> {
    json_bytes(StatusCode::OK, Bytes::copy_from_slice(raw.get().as_bytes()))
}

fn error_response(error: &ApiError) -> Response<Body> {
    json_response(error.status, &json!({ "error": error.message }))
}

fn preflight() -> Response<Body> {
    finish(base(StatusCode::NO_CONTENT).header(header::ALLOW, "GET, POST, OPTIONS").header(header::CONTENT_LENGTH, 0), Body::empty())
}

// Input

async fn read_json(request: Request<Incoming>) -> Result<Map<String, Value>, ApiError> {
    let too_large = || ApiError::bad_request("request body too large");
    let declared = request.headers().get(header::CONTENT_LENGTH).and_then(|value| value.to_str().ok()?.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_BODY_BYTES as u64) {
        return Err(too_large());
    }
    let collected = Limited::new(request.into_body(), MAX_BODY_BYTES).collect().await;
    let raw = match collected {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.is::<http_body_util::LengthLimitError>() => return Err(too_large()),
        Err(_) => return Err(ApiError::bad_request("malformed request body")),
    };
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(body)) => Ok(body),
        Ok(_) => Err(ApiError::bad_request("expected a JSON object body")),
        Err(_) => Err(ApiError::bad_request("malformed JSON body")),
    }
}

/// Yomitan's `string | string[]` input as a list, and whether it was a single string.
fn strings(body: &Map<String, Value>, name: &str) -> Result<(Vec<Value>, bool), ApiError> {
    match body.get(name) {
        Some(single @ Value::String(_)) => Ok((vec![single.clone()], true)),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => Ok((items.clone(), false)),
        _ => Err(ApiError::bad_request(format!("expected \"{name}\" to be a string or a string array"))),
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn is_integer(value: &Value) -> bool {
    matches!(value, Value::Number(number) if number.is_i64() || number.is_u64())
}

// Host replies

async fn ask(server: &Server, kind: &str, payload: Value) -> Result<Box<RawValue>, ApiError> {
    server.session().await?.request(kind, &object(payload), server.lookup_timeout, None).await
}

/// `response[name]`, which must be an array.
fn array_field<'a>(response: &'a RawValue, name: &str) -> Result<&'a RawValue, ApiError> {
    json::fields(response).and_then(|fields| fields.get(name).copied()).filter(|raw| json::is_array(raw)).ok_or_else(ApiError::malformed)
}

/// `response.results`, which must have one element per input.
fn results(response: &RawValue, count: usize) -> Result<(&RawValue, Vec<&RawValue>), ApiError> {
    let raw = array_field(response, "results")?;
    let items = json::elements(raw).filter(|items| items.len() == count).ok_or_else(ApiError::malformed)?;
    Ok((raw, items))
}

// Routes

async fn yomitan_version(server: &Server) -> Result<Response<Body>, ApiError> {
    Ok(raw_response(&ask(server, "hd_api_version", json!({})).await?))
}

async fn term_entries(server: &Server, body: &Map<String, Value>) -> Result<Response<Body>, ApiError> {
    let (terms, single) = strings(body, "term")?;
    let count = terms.len();
    let response = ask(server, "hd_api_term_entries", json!({ "terms": terms })).await?;
    let (all, items) = results(&response, count)?;
    Ok(raw_response(if single { items[0] } else { all }))
}

async fn kanji_entries(server: &Server, body: &Map<String, Value>) -> Result<Response<Body>, ApiError> {
    let (characters, single) = strings(body, "character")?;
    let count = characters.len();
    let response = ask(server, "hd_api_kanji_entries", json!({ "characters": characters })).await?;
    let (all, items) = results(&response, count)?;
    if !single {
        return Ok(raw_response(all));
    }
    // Yomitan answers a string input with the bare entry list.
    let entries = json::fields(items[0]).and_then(|fields| fields.get("dictionaryEntries").copied()).filter(|raw| json::is_array(raw));
    Ok(entries.map_or_else(|| json_bytes(StatusCode::OK, Bytes::from_static(b"[]")), raw_response))
}

async fn anki_fields(server: &Server, body: &Map<String, Value>) -> Result<Response<Body>, ApiError> {
    let term = Value::from("term");
    let Some(text @ Value::String(_)) = body.get("text") else {
        return Err(ApiError::bad_request("expected \"text\" to be a string"));
    };
    let kind = body.get("type").unwrap_or(&term);
    if !matches!(kind.as_str(), Some("term" | "kanji")) {
        return Err(ApiError::bad_request("expected \"type\" to be \"term\" or \"kanji\""));
    }
    let markers = body.get("markers").filter(|value| value.as_array().is_some_and(|items| items.iter().all(Value::is_string)));
    let Some(markers) = markers else {
        return Err(ApiError::bad_request("expected \"markers\" to be a string array"));
    };
    let max_entries = body.get("maxEntries").cloned().unwrap_or_else(|| Value::from(0));
    if !is_integer(&max_entries) {
        return Err(ApiError::bad_request("expected \"maxEntries\" to be an integer"));
    }
    let include_media = body.get("includeMedia").cloned().unwrap_or(Value::Bool(false));
    if !include_media.is_boolean() {
        return Err(ApiError::bad_request("expected \"includeMedia\" to be a boolean"));
    }
    // Yomitan's "type" travels as entryType, because "type" names the message itself.
    let payload = json!({ "text": text, "entryType": kind, "markers": markers, "maxEntries": max_entries, "includeMedia": include_media });
    let response = ask(server, "hd_api_anki_fields", payload).await?;
    array_field(&response, "fields")?;
    Ok(raw_response(&response))
}

async fn anki_card_formats(server: &Server, body: &Map<String, Value>) -> Result<Response<Body>, ApiError> {
    // Yomitan reads a number as a profile index, and anything else as the active profile.
    let payload = match body.get("profileIndex") {
        Some(index @ Value::Number(_)) => json!({ "profileIndex": index }),
        _ => json!({}),
    };
    let response = ask(server, "hd_api_anki_card_formats", payload).await?;
    Ok(raw_response(array_field(&response, "cardFormats")?))
}

async fn tokenize(server: &Server, body: &Map<String, Value>) -> Result<Response<Body>, ApiError> {
    let (texts, _) = strings(body, "text")?;
    let Some(scan_length @ Value::Number(_)) = body.get("scanLength") else {
        return Err(ApiError::bad_request("expected \"scanLength\" to be a number"));
    };
    let default_parser = Value::from("scanning-parser");
    let parser = body.get("parser").unwrap_or(&default_parser);
    if !parser.is_string() {
        return Err(ApiError::bad_request("expected \"parser\" to be a string"));
    }
    let response = ask(server, "hd_api_tokenize", json!({ "texts": texts, "scanLength": scan_length, "parser": parser })).await?;
    Ok(raw_response(array_field(&response, "results")?))
}

async fn dictionaries(server: &Server) -> Result<Response<Body>, ApiError> {
    let response = ask(server, "hd_api_dictionaries", json!({})).await?;
    array_field(&response, "dictionaries")?;
    Ok(raw_response(&response))
}

// Downloads

/// A token the host granted: a string or an integer. As in upstream (Python's bool is
/// an int), `true` and `false` count as integers.
fn token_of(response: &RawValue) -> Option<Value> {
    let raw = *json::fields(response)?.get("token")?;
    if let Some(text) = json::as_str(raw) {
        return Some(Value::String(text.into_owned()));
    }
    serde_json::from_str::<Value>(raw.get()).ok().filter(|value| is_integer(value) || value.is_boolean())
}

/// `Content-Disposition` with an ASCII-safe `filename`, plus RFC 5987's `filename*` when that changed the name.
fn content_disposition(file_name: &str) -> String {
    let safe: String = file_name
        .chars()
        .map(|character| if (' '..='~').contains(&character) && !matches!(character, '"' | '\\') { character } else { '_' })
        .collect();
    let safe = if safe.is_empty() { "dictionary.zip".to_owned() } else { safe };
    let mut header = format!("attachment; filename=\"{safe}\"");
    if safe != file_name {
        header.push_str("; filename*=UTF-8''");
        header.extend(utf8_percent_encode(file_name, FILENAME_ESCAPES));
    }
    header
}

/// `chunk.data`, base64. A missing or empty value is no data. Anything that is not
/// valid base64 is `None`.
fn chunk_data(data: Option<&RawValue>) -> Option<Vec<u8>> {
    let Some(raw) = data else { return Some(Vec::new()) };
    if let Some(text) = json::as_str(raw) {
        return BASE64.decode(text.as_bytes()).ok();
    }
    // Python's `chunk.get("data") or ""`: null, false, 0, [] and {} are all empty.
    let empty = match raw.get().trim() {
        "null" | "false" | "[]" | "{}" => true,
        number => number.parse::<f64>().is_ok_and(|value| value == 0.0),
    };
    empty.then(Vec::new)
}

/// A download the host has opened: its lease token, its length when known, and its file name.
pub(crate) struct Opened {
    pub(crate) token: Value,
    pub(crate) size: Option<u64>,
    pub(crate) file_name: String,
}

/// Asks the host to open dictionary `id` for download. A lease the host grants after
/// the wait is over is released at once.
pub(crate) async fn open_download(session: &Arc<Session>, id: &str, timeout: std::time::Duration) -> Result<Opened, ApiError> {
    let late: Late = {
        let session = session.clone();
        // The host granted the lease after we stopped waiting. Release it anyway.
        Box::new(move |response| {
            if let Some(token) = token_of(response) {
                session.notify("hd_api_dictionary_close", &object(json!({ "token": token })));
            }
        })
    };
    let opened = session.request("hd_api_dictionary_open", &object(json!({ "id": id })), timeout, Some(late)).await?;
    let token = token_of(&opened).ok_or_else(ApiError::malformed)?;
    let fields = json::fields(&opened).unwrap_or_default();
    let size = fields.get("size").and_then(|raw| raw.get().trim().parse::<u64>().ok());
    let file_name = fields
        .get("fileName")
        .and_then(|raw| json::as_str(raw))
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("{id}.zip"), std::borrow::Cow::into_owned);
    Ok(Opened { token, size, file_name })
}

async fn download(server: &Server, id: &str) -> Result<Response<Body>, ApiError> {
    let session = server.session().await?;
    let Opened { token, size, file_name } = open_download(&session, id, server.lookup_timeout).await?;
    let (chunks, receiver) = mpsc::channel(1);
    tokio::spawn(stream_chunks(session, token, size, chunks, server.chunk_timeout));
    let mut builder = base(StatusCode::OK).header(header::CONTENT_TYPE, "application/zip");
    if let Ok(disposition) = HeaderValue::from_str(&content_disposition(&file_name)) {
        builder = builder.header(header::CONTENT_DISPOSITION, disposition);
    }
    builder = match size {
        Some(size) => builder.header(header::CONTENT_LENGTH, size),
        // Without a length, the body ends where the connection ends.
        None => builder.header(header::CONNECTION, "close"),
    };
    Ok(finish(builder, Body::Stream(receiver)))
}

/// Releases a download's token on every way out: finished, failed, timed out, or abandoned by the client.
struct CloseToken {
    session: Arc<Session>,
    token: Value,
}

impl Drop for CloseToken {
    fn drop(&mut self) {
        self.session.notify("hd_api_dictionary_close", &object(json!({ "token": self.token })));
    }
}

/// Reads the dictionary in order, 4 MiB at a time, and passes each chunk on as it arrives.
/// At most one chunk waits for a slow client, so a download never holds the whole file.
pub(crate) async fn stream_chunks(
    session: Arc<Session>,
    token: Value,
    size: Option<u64>,
    chunks: mpsc::Sender<io::Result<Bytes>>,
    timeout: std::time::Duration,
) {
    let _close = CloseToken { session: session.clone(), token: token.clone() };
    let mut offset: u64 = 0;
    let failure = loop {
        if size.is_some_and(|size| offset >= size) {
            break None;
        }
        let payload = object(json!({ "token": token, "offset": offset, "length": CHUNK_LENGTH }));
        let chunk = tokio::select! {
            biased;
            () = chunks.closed() => return,
            chunk = session.request("hd_api_dictionary_read", &payload, timeout, None) => chunk,
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => break Some(error.message.into_owned()),
        };
        let Some(fields) = json::fields(&chunk) else { break Some("malformed chunk".to_owned()) };
        let Some(data) = chunk_data(fields.get("data").copied()) else { break Some("malformed chunk".to_owned()) };
        let eof = fields.get("eof").is_some_and(|raw| json::is_true(raw));
        let mut data = Bytes::from(data);
        if let Some(size) = size {
            let room = usize::try_from(size - offset).unwrap_or(usize::MAX);
            data.truncate(room);
        }
        if !data.is_empty() {
            offset += data.len() as u64;
            if chunks.send(Ok(data)).await.is_err() {
                return;
            }
        } else if !eof {
            break Some("empty chunk before eof".to_owned());
        }
        if eof {
            break None;
        }
    };
    // The headers are already out. Cutting the body short is the only signal left.
    let short = size.is_some_and(|size| offset < size);
    if let Some(reason) = failure.or_else(|| short.then(|| "the host ended the file early".to_owned())) {
        tracing::debug!(%reason, offset, "download ended short");
        let _ = chunks.send(Err(io::Error::other(reason))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_matches_upstream() {
        assert_eq!(content_disposition("jitendex.hachidori.zip"), "attachment; filename=\"jitendex.hachidori.zip\"");
        assert_eq!(content_disposition("辞書.zip"), "attachment; filename=\"__.zip\"; filename*=UTF-8''%E8%BE%9E%E6%9B%B8.zip");
        assert_eq!(content_disposition("a \"b\".zip"), "attachment; filename=\"a _b_.zip\"; filename*=UTF-8''a%20%22b%22.zip");
        assert_eq!(content_disposition(""), "attachment; filename=\"dictionary.zip\"; filename*=UTF-8''");
    }

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_owned()).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn chunk_data_decodes_base64_and_treats_falsy_values_as_empty() {
        assert_eq!(chunk_data(Some(&raw(r#""UEsDBBQAAAAIAA==""#))), Some(b"PK\x03\x04\x14\x00\x00\x00\x08\x00".to_vec()));
        for empty in ["null", "false", "0", "\"\"", "[]", "{}"] {
            assert_eq!(chunk_data(Some(&raw(empty))), Some(Vec::new()), "{empty}");
        }
        assert_eq!(chunk_data(None), Some(Vec::new()));
        assert_eq!(chunk_data(Some(&raw(r#""not base64!""#))), None);
        assert_eq!(chunk_data(Some(&raw(r#""QUJD""#))), Some(b"ABC".to_vec()));
        assert_eq!(chunk_data(Some(&raw(r#""QUI""#))), None, "padding is required");
        assert_eq!(chunk_data(Some(&raw("5"))), None);
    }

    #[test]
    fn tokens_are_strings_or_integers() {
        assert_eq!(token_of(&raw(r#"{"token": "dl-1"}"#)), Some(Value::from("dl-1")));
        assert_eq!(token_of(&raw(r#"{"token": 7}"#)), Some(Value::from(7)));
        assert_eq!(token_of(&raw(r#"{"token": true}"#)), Some(Value::Bool(true)), "Python's bool is an int");
        assert_eq!(token_of(&raw(r#"{"token": 1.5}"#)), None);
        assert_eq!(token_of(&raw(r#"{"size": 1}"#)), None);
    }

    #[test]
    fn strings_accept_a_string_or_a_string_array_only() {
        let body = object(json!({ "a": "x", "b": ["x", "y"], "c": ["x", 1], "d": 5 }));
        assert_eq!(strings(&body, "a").ok(), Some((vec![Value::from("x")], true)));
        assert_eq!(strings(&body, "b").map(|(items, single)| (items.len(), single)).ok(), Some((2, false)));
        assert_eq!(strings(&body, "c").err().map(|error| error.message), Some("expected \"c\" to be a string or a string array".into()));
        assert!(strings(&body, "d").is_err());
        assert!(strings(&body, "missing").is_err());
    }

    #[test]
    fn results_must_have_one_element_per_input() {
        let response = raw(r#"{"results": [{"index": 0}, {"index": 1}]}"#);
        assert!(results(&response, 2).is_ok());
        assert_eq!(results(&response, 1).err(), Some(ApiError::malformed()));
        assert_eq!(results(&raw(r#"{"results": {}}"#), 0).err(), Some(ApiError::malformed()));
        assert_eq!(results(&raw("[]"), 0).err(), Some(ApiError::malformed()));
    }
}
