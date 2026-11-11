// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings the relay keeps as the extension's service worker keeps them.

pub(crate) mod backup;
pub(crate) mod custom;
pub(crate) mod options;
pub(crate) mod profile_worker;
pub(crate) mod profiles;
pub(crate) mod worker;

/// Bytes as lower-case hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}
