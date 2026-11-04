// SPDX-License-Identifier: GPL-3.0-or-later
//! The safety limits, which upstream does not have. Each limit is tested just under,
//! at and over its value. Going over closes only the offending peer, and the relay,
//! the host and the other clients carry on. With the default limits, realistically
//! large traffic never trips one.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{Ws, connect, connect_padded};
use hachidori_relay::{Limits, Relay};
use serde_json::json;

fn limits(handshake: usize, message: usize, queued: usize) -> Limits {
    Limits { max_handshake_bytes: handshake, max_message_bytes: message, max_queued_bytes: queued, ..Limits::default() }
}

fn start(limits: Limits) -> Relay {
    Relay::builder().port(0).limits(limits).start().expect("relay starts")
}

async fn host(port: u16) -> Ws {
    let mut host = connect(port, "/host").await;
    assert_eq!(host.status, 101);
    assert_eq!(host.until_kind("listening").await["port"], port);
    host
}

async fn link(port: u16, host: &mut Ws) -> (Ws, String) {
    let client = connect(port, "/link").await;
    assert_eq!(client.status, 101);
    let id = host.until_kind("client-open").await["clientId"].as_str().unwrap().to_owned();
    (client, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_request_over_the_limit_gets_431_and_the_relay_carries_on() {
    let relay = start(limits(1024, 1 << 20, 1 << 20));
    let port = relay.port();
    let _first = host(port).await;
    // About 260 bytes of fixed headers, plus the padding.
    assert_eq!(connect_padded(port, "/link", 600).await.status, 101, "well under the limit");
    let mut refused = connect_padded(port, "/link", 2000).await;
    assert_eq!(refused.status, 431);
    assert!(refused.next().await.is_none(), "the refused connection is closed");
    // The first host still holds the slot, so a second one is told so: the relay still works.
    let mut second = connect(port, "/host").await;
    assert_eq!(second.status, 101);
    assert_eq!(second.until_kind("listen-failed").await["kind"], "listen-failed");
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_at_the_limit_passes_and_one_over_it_closes_only_its_sender() {
    const LIMIT: usize = 64 * 1024;
    let relay = start(limits(64 * 1024, LIMIT, 64 << 20));
    let port = relay.port();
    let mut host = host(port).await;
    let (mut big, big_id) = link(port, &mut host).await;
    let (mut other, other_id) = link(port, &mut host).await;

    let exact = "a".repeat(LIMIT);
    big.text(&exact).await;
    let relayed = host.until_kind("client-text").await;
    assert_eq!(relayed["clientId"], big_id.as_str());
    assert_eq!(relayed["text"].as_str().map(str::len), Some(LIMIT), "exactly at the limit is fine");

    // Fragments count together: two halves that add up to one byte over the limit.
    big.frame(0x1, false, &vec![b'b'; LIMIT / 2]).await;
    big.frame(0x0, true, &vec![b'b'; LIMIT / 2 + 1]).await;
    big.until_closed().await;
    assert_eq!(host.until_kind("client-close").await["clientId"], big_id.as_str());

    other.text("still here").await;
    let relayed = host.until_kind("client-text").await;
    assert_eq!((relayed["clientId"].as_str(), relayed["text"].as_str()), (Some(other_id.as_str()), Some("still here")));

    // One whole frame over the limit is refused from its header alone.
    let (mut single, single_id) = link(port, &mut host).await;
    single.frame(0x1, true, &vec![b'c'; LIMIT + 1]).await;
    single.until_closed().await;
    assert_eq!(host.until_kind("client-close").await["clientId"], single_id.as_str());

    // A host that goes over the limit is closed too. Its clients go with it, and a new host is accepted.
    host.text(&"h".repeat(LIMIT + 1)).await;
    host.until_closed().await;
    other.until_closed().await;
    let mut next = connect(port, "/host").await;
    assert_eq!(next.until_kind("listening").await["port"], port);
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_client_over_its_queue_limit_is_dropped_and_everyone_else_carries_on() {
    const QUEUE: usize = 4 * 1024 * 1024;
    let relay = start(limits(64 * 1024, 64 << 20, QUEUE));
    let port = relay.port();
    let mut host = host(port).await;
    let (_stalled, stalled_id) = link(port, &mut host).await; // never read
    let (mut healthy, healthy_id) = link(port, &mut host).await;

    // 64 × 512 KiB = 32 MiB toward a client that never reads: far more than the kernel's
    // socket buffers plus the 4 MiB queue.
    let chunk = "x".repeat(512 * 1024);
    for _ in 0..64 {
        host.json(&json!({ "kind": "send", "clientId": stalled_id, "text": chunk })).await;
    }
    assert_eq!(host.until_kind("client-close").await["clientId"], stalled_id.as_str(), "the stalled client is dropped");

    host.json(&json!({ "kind": "send", "clientId": healthy_id, "text": "{\"kind\":\"reply\",\"ok\":true}" })).await;
    assert_eq!(healthy.until_kind("reply").await["ok"], true, "the healthy client is unaffected");
    healthy.text("lookup").await;
    assert_eq!(host.until_kind("client-text").await["text"], "lookup", "the host is unaffected");
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queue_just_under_the_limit_is_delivered_whole_once_the_client_reads() {
    const QUEUE: usize = 4 * 1024 * 1024;
    let relay = start(limits(64 * 1024, 64 << 20, QUEUE));
    let port = relay.port();
    let mut host = host(port).await;
    let (mut slow, slow_id) = link(port, &mut host).await;
    // Seven 512 KiB messages: 3.5 MiB, under the 4 MiB limit even if the kernel buffered none of it.
    let chunk = "y".repeat(512 * 1024);
    for sequence in 0..7 {
        let text = json!({ "kind": "large", "sequence": sequence, "text": chunk }).to_string();
        host.json(&json!({ "kind": "send", "clientId": slow_id, "text": text })).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for sequence in 0..7 {
        let frame = slow.until_kind("large").await;
        assert_eq!(frame["sequence"], sequence);
        assert_eq!(frame["text"].as_str().map(str::len), Some(chunk.len()));
    }
    relay.shutdown().await;
}

/// The default limits must never trip on large but legitimate traffic. Runs a 32 MiB
/// message in each direction, plus a broadcast while a client is paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_limits_do_not_trip_on_32_mib_messages() {
    large_traffic(32).await;
}

/// The same with 128 MiB messages. It is heavy, so it only runs on request:
/// `cargo test --release --test limits -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heavy: 128 MiB messages; run with --ignored"]
async fn default_limits_do_not_trip_on_128_mib_messages() {
    large_traffic(128).await;
}

async fn large_traffic(mebibytes: usize) {
    let relay = Relay::builder().port(0).start().expect("relay starts");
    let port = relay.port();
    let mut host = host(port).await;
    let (mut client, client_id) = link(port, &mut host).await;
    let (mut paused, paused_id) = link(port, &mut host).await;
    let big = "大".repeat(mebibytes * 1024 * 1024 / 3);

    client.text(&big).await;
    let relayed = host.until_kind("client-text").await;
    assert_eq!(relayed["text"].as_str().map(str::len), Some(big.len()));

    let reply = json!({ "kind": "reply", "text": big }).to_string();
    host.json(&json!({ "kind": "send", "clientId": client_id, "text": reply })).await;
    host.json(&json!({ "kind": "send", "clientId": paused_id, "text": reply })).await;
    host.json(&json!({ "kind": "broadcast", "text": "{\"kind\":\"marker\"}" })).await;
    assert_eq!(client.until_kind("reply").await["text"].as_str().map(str::len), Some(big.len()));
    client.until_kind("marker").await;
    // The paused client reads only now, after everything was queued for it.
    assert_eq!(paused.until_kind("reply").await["text"].as_str().map(str::len), Some(big.len()));
    paused.until_kind("marker").await;

    client.text("after").await;
    assert_eq!(host.until_kind("client-text").await["text"], "after", "nobody was dropped");
    relay.shutdown().await;
}
