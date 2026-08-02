//! Nordic UART Service (NUS) GATT definition.
//!
//! One ergot frame maps to one GATT write (host → device) or one notification
//! (device → host). NUS is used rather than a bespoke service because every
//! host BLE stack already knows how to talk to it, and ergot supplies its own
//! framing on top.

// The gatt_server/gatt_service macros re-qualify types and borrow the value
// type in their expansions, tripping these on spans no item-level allow can
// reach — suppress for the whole (small) module.
#![allow(unused_qualifications, clippy::needless_borrows_for_generic_args)]

use heapless::Vec;
use trouble_host::prelude::*;

/// Maximum payload per GATT characteristic value.
///
/// A physical cap, not a tunable: a notification carries at most ATT_MTU - 3
/// bytes, and with the packet pool MTU set to 512 (see `.cargo/config.toml`)
/// the negotiated ATT_MTU is 508.
pub const NUS_MAX_PAYLOAD: usize = 505;

#[gatt_server]
pub struct NusServer {
    pub nus: NusService,
}

#[gatt_service(uuid = "6e400001-b5a3-f393-e0a9-e50e24dcca9e")]
pub struct NusService {
    /// RX characteristic — the host writes ergot frames here.
    #[characteristic(uuid = "6e400002-b5a3-f393-e0a9-e50e24dcca9e", write_without_response)]
    pub rx: Vec<u8, NUS_MAX_PAYLOAD>,

    /// TX characteristic — the device notifies ergot frames to the host.
    #[characteristic(uuid = "6e400003-b5a3-f393-e0a9-e50e24dcca9e", notify)]
    pub tx: Vec<u8, NUS_MAX_PAYLOAD>,
}
