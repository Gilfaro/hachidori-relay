// SPDX-License-Identifier: GPL-3.0-or-later
//! The relay's WebSocket server. Only what the relay needs: text in both directions,
//! ping/pong and close, with the upgrade request checked before anything is accepted.

pub(crate) mod conn;
pub(crate) mod frame;
pub(crate) mod handshake;
pub(crate) mod peer;
