//! Ergot transport setup.
//!
//! This device is an edge node with exactly one interface: the BLE NUS link to
//! the host daemon. There is no routing to do, so the stack uses
//! [`DirectEdge`] rather than a router profile — the host is the only peer, and
//! its net id is learned from the first frame it sends.

use bbqueue::BBQueue;
use bbqueue::traits::coordination::cas::AtomicCoord;
use bbqueue::traits::notifier::maitake::MaiNotSpsc;
use bbqueue::traits::storage::Inline;
use ergot::NetStack;
use ergot::interface_manager::profiles::direct_edge::DirectEdge;
use mutex::raw_impls::cs::CriticalSectionRawMutex;

/// Outbound frame queue. A usage snapshot is a few dozen bytes and arrives
/// about once a minute, so this only has to absorb a reconnect burst.
pub const BLE_QUEUE_SIZE: usize = 1024;

/// Must match [`crate::ble_nus::NUS_MAX_PAYLOAD`]: frames larger than one
/// notification cannot cross the link.
pub const BLE_MTU: u16 = 505;

pub type BleQueue = BBQueue<Inline<BLE_QUEUE_SIZE>, AtomicCoord, MaiNotSpsc>;
type BleQueueRef = &'static BleQueue;

pub struct BleNusInterface;

impl ergot::interface_manager::Interface for BleNusInterface {
    type Sink = ergot::interface_manager::utils::framed_stream::Sink<BleQueueRef>;
}

pub type Stack = NetStack<CriticalSectionRawMutex, DirectEdge<BleNusInterface>>;

pub static BLE_OUTQ: BleQueue = BBQueue::new();
