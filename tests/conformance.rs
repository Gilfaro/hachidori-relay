// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs upstream's Node socket and HTTP suites (vendored in `tests/conformance`)
//! against the built binary. They are the acceptance tests for 1:1 behaviour.
//! It also runs `limits.test.mjs`, which tests the relay's own safety limits, and
//! `host.test.mjs`, which tests host mode (skipped in a build without it).
//! Needs Node 22 or later. Without Node, this test reports that and passes.
#![cfg(feature = "cli")]
#![allow(clippy::print_stderr, missing_docs)]

use std::path::Path;
use std::process::Command;

#[test]
fn upstream_node_suites_pass_against_the_binary() {
    let Ok(version) = Command::new("node").arg("--version").output() else {
        eprintln!("node is not installed; the upstream conformance suites did not run");
        return;
    };
    eprintln!("node {}", String::from_utf8_lossy(&version.stdout).trim());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new("node")
        .args([
            "--test",
            "--test-concurrency=1",
            "tests/conformance/sharing-relay.test.mjs",
            "tests/conformance/api.test.mjs",
            "tests/conformance/limits.test.mjs",
            "tests/conformance/host.test.mjs",
        ])
        .env("HACHIDORI_RELAY_BIN", env!("CARGO_BIN_EXE_hachidori-relay"))
        .current_dir(root)
        .status()
        .unwrap_or_else(|error| panic!("cannot run node: {error}"));
    assert!(status.success(), "the upstream conformance suites failed: {status}");
}
