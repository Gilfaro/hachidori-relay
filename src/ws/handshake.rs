// SPDX-License-Identifier: GPL-3.0-or-later
//! The HTTP upgrade request: parsing, the refusal rules, and the `101` answer.

use std::net::IpAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub(crate) const HOST_PATH: &str = "/host";
pub(crate) const LINK_PATH: &str = "/link";
/// Hachidori in Chrome, and in Firefox.
const EXTENSION_ORIGIN_PREFIXES: [&str; 2] = ["chrome-extension://", "moz-extension://"];
/// Native Hoshidicts apps share this one exact origin, and may only link.
const HOSHIDICTS_ORIGIN: &str = "hoshi://hoshidicts";
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// What the relay reads from an upgrade request.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Request {
    /// The request target without its query string.
    pub(crate) path: String,
    pub(crate) origin: Option<String>,
    pub(crate) key: Option<String>,
}

/// Which endpoint a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Host,
    Link,
}

/// Parses the head of an upgrade request: the bytes up to, but not including, the blank line.
/// Bytes are read as Latin-1, as HTTP/1.1 heads are. `None` means it is not a request
/// line at all, and the connection just closes.
pub(crate) fn parse(head: &[u8]) -> Option<Request> {
    let text: String = head.iter().copied().map(char::from).collect();
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.splitn(3, ' ');
    let (_method, target, _version) = (request_line.next()?, request_line.next()?, request_line.next()?);
    let path = target.split('?').next().unwrap_or_default().to_owned();
    let mut request = Request { path, ..Request::default() };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim();
        // A header that repeats keeps its last value.
        if name.eq_ignore_ascii_case("origin") {
            request.origin = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("sec-websocket-key") {
            request.key = Some(value.trim().to_owned());
        }
    }
    Some(request)
}

/// The endpoint the request may use, or the HTTP status that turns it away.
///
/// Hachidori extensions may use either endpoint. Native Hoshidicts apps may only
/// link, and ordinary web pages may do neither. Only the Hachidori on this computer
/// may host; other computers only link. A link is refused while no host is connected.
pub(crate) fn check(request: &Request, peer: IpAddr, has_host: impl FnOnce() -> bool) -> Result<Endpoint, &'static str> {
    let endpoint = match request.path.as_str() {
        HOST_PATH => Endpoint::Host,
        LINK_PATH => Endpoint::Link,
        _ => return Err("404 Not Found"),
    };
    let origin = request.origin.as_deref().unwrap_or_default();
    let extension = EXTENSION_ORIGIN_PREFIXES.iter().any(|prefix| origin.starts_with(prefix));
    if !extension && (endpoint != Endpoint::Link || origin != HOSHIDICTS_ORIGIN) {
        return Err("403 Forbidden");
    }
    if endpoint == Endpoint::Host && !peer.is_loopback() {
        return Err("403 Forbidden");
    }
    if request.key.is_none() {
        return Err("400 Bad Request");
    }
    if endpoint == Endpoint::Link && !has_host() {
        return Err("503 Service Unavailable");
    }
    Ok(endpoint)
}

/// The answer that turns a request away.
pub(crate) fn refusal(status: &str) -> String {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n")
}

/// The `101 Switching Protocols` answer for a request with this `Sec-WebSocket-Key`.
pub(crate) fn accept(key: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    )
}

/// RFC 6455's accept value: base64 of SHA-1 over the key and the GUID. It protects nothing.
fn accept_key(key: &str) -> String {
    let mut sha = sha1_smol::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(WEBSOCKET_GUID.as_bytes());
    STANDARD.encode(sha.digest().bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const REMOTE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));

    fn request(path: &str, origin: Option<&str>, key: bool) -> Request {
        Request { path: path.to_owned(), origin: origin.map(str::to_owned), key: key.then(|| "dGhlIHNhbXBsZSBub25jZQ==".to_owned()) }
    }

    #[test]
    fn parses_path_without_query_and_last_header_wins() {
        let head = b"GET /link?x=1 HTTP/1.1\r\nHost: a\r\norigin: one\r\nOrigin:  chrome-extension://abc \r\nSec-WebSocket-Key: k==";
        let parsed = parse(head);
        assert_eq!(parsed, Some(Request { path: "/link".into(), origin: Some("chrome-extension://abc".into()), key: Some("k==".into()) }));
    }

    #[test]
    fn a_request_line_without_three_parts_is_not_a_request() {
        assert_eq!(parse(b"GET /host"), None);
        assert_eq!(parse(b""), None);
    }

    #[test]
    fn refusal_order_matches_upstream() {
        let chrome = Some("chrome-extension://id");
        assert_eq!(check(&request("/x", chrome, true), LOCAL, || true), Err("404 Not Found"));
        assert_eq!(check(&request("/host", Some("https://example.com"), true), LOCAL, || true), Err("403 Forbidden"));
        assert_eq!(check(&request("/host", None, true), LOCAL, || true), Err("403 Forbidden"));
        assert_eq!(check(&request("/host", chrome, true), REMOTE, || true), Err("403 Forbidden"));
        assert_eq!(check(&request("/host", chrome, false), LOCAL, || true), Err("400 Bad Request"));
        assert_eq!(check(&request("/link", chrome, true), LOCAL, || false), Err("503 Service Unavailable"));
        assert_eq!(check(&request("/link", chrome, true), REMOTE, || true), Ok(Endpoint::Link));
        assert_eq!(check(&request("/host", Some("moz-extension://uuid"), true), LOCAL, || true), Ok(Endpoint::Host));
    }

    #[test]
    fn hoshidicts_origin_links_only_and_must_match_exactly() {
        let hoshi = Some(HOSHIDICTS_ORIGIN);
        assert_eq!(check(&request("/link", hoshi, true), LOCAL, || true), Ok(Endpoint::Link));
        assert_eq!(check(&request("/host", hoshi, true), LOCAL, || true), Err("403 Forbidden"));
        for origin in ["hoshi://hoshidicts.evil", "https://hoshidicts", "moz-extension:", "resource://gre"] {
            assert_eq!(check(&request("/link", Some(origin), true), LOCAL, || true), Err("403 Forbidden"), "{origin}");
        }
    }

    #[test]
    fn accept_key_matches_rfc_6455_example() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
