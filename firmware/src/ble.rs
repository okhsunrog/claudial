//! BLE peripheral carrying the ergot link to the host daemon.
//!
//! The device advertises a Nordic UART Service and the host connects to it.
//! Everything above the framing is ergot's problem: this module only moves
//! bytes between the GATT characteristics and the stack's frame queues.
//!
//! There is deliberately no pairing or bonding. The link carries a usage
//! percentage, not credentials or HID reports, so an encrypted link would buy
//! nothing and cost the whole `security` feature plus bond storage.

use defmt::{info, warn};
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use ergot::interface_manager::profiles::direct_edge::{EDGE_NODE_ID, EdgeFrameProcessor};
use ergot::interface_manager::{FrameProcessor, InterfaceState, Profile};
use esp_radio::ble::controller::BleConnector;
use static_cell::StaticCell;
use trouble_host::prelude::*;

use crate::ble_nus::{NUS_MAX_PAYLOAD, NusServer};
use crate::events::{BleSignal, BleState, UsageSignal};
use crate::transport::{BLE_OUTQ, Stack};
use claudial_icd::UsageTopic;

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;

/// Advertised name. The host daemon looks for this.
const DEVICE_NAME: &str = "Claudial";

/// Fixed random static address, so the host sees the same device across
/// reboots without any bonding involved.
const DEVICE_ADDRESS: [u8; 6] = [0xC1, 0xA0, 0xD3, 0x11, 0xE7, 0xE6];

/// Receives usage snapshots pushed by the host and hands them to the UI.
#[embassy_executor::task]
pub async fn usage_task(stack: &'static Stack, usage: &'static UsageSignal) {
    let receiver = stack.topics().bounded_receiver::<UsageTopic, 2>(None);
    let receiver = core::pin::pin!(receiver);
    let mut receiver = receiver.subscribe();

    loop {
        let message = receiver.recv().await;
        info!(
            "Usage: session {}% (reset {} min), weekly {}% (reset {} min)",
            message.t.session_pct,
            message.t.session_reset_mins,
            message.t.weekly_pct,
            message.t.weekly_reset_mins
        );
        usage.signal(message.t);
    }
}

/// Runs the BLE peripheral for the lifetime of the device.
#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "Embassy stores the async task state statically rather than on the runtime call stack"
)]
pub async fn ble_task(
    stack: &'static Stack,
    connector: BleConnector<'static>,
    state: &'static BleSignal,
) {
    let controller = ExternalController::<_, CONNECTIONS_MAX>::new(connector);

    static RESOURCES: StaticCell<
        HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX>,
    > = StaticCell::new();
    let resources = RESOURCES.init(HostResources::new());

    let host = trouble_host::new(controller, resources)
        .set_random_address(Address::random(DEVICE_ADDRESS));
    let Host {
        mut peripheral,
        runner,
        ..
    } = host.build();

    let Ok(server) = NusServer::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: DEVICE_NAME,
        appearance: &appearance::human_interface_device::GENERIC_HUMAN_INTERFACE_DEVICE,
    })) else {
        warn!("[ble] GATT server init failed");
        state.signal(BleState::Error);
        return;
    };

    join(
        run_controller(runner),
        accept_loop(stack, &mut peripheral, &server, state),
    )
    .await;
}

/// Drives the host controller. It only returns on error, so restart it.
async fn run_controller<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if runner.run().await.is_err() {
            warn!("[ble] controller error, restarting");
        }
    }
}

/// Advertise, serve one connection, repeat.
async fn accept_loop<'values, C: Controller>(
    stack: &'static Stack,
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &NusServer<'values>,
    state: &'static BleSignal,
) {
    loop {
        state.signal(BleState::Advertising);
        match advertise(peripheral, server).await {
            Ok(connection) => {
                info!("[ble] host connected");
                state.signal(BleState::Connected);
                connection_task(stack, server, &connection).await;
                info!("[ble] host disconnected");
            }
            Err(_) => {
                state.signal(BleState::Error);
                warn!("[ble] advertise failed, retrying");
            }
        }
    }
}

async fn advertise<'values, 'server, C: Controller>(
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server NusServer<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut advertisement = [0_u8; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(DEVICE_NAME.as_bytes()),
        ],
        &mut advertisement[..],
    )?;

    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertisement[..len],
                scan_data: &[],
            },
        )
        .await?;

    info!("[ble] advertising as {}", DEVICE_NAME);
    let connection = advertiser.accept().await?.with_attribute_server(server)?;
    Ok(connection)
}

/// Pump frames both ways until the host goes away.
async fn connection_task<P: PacketPool>(
    stack: &'static Stack,
    server: &NusServer<'_>,
    connection: &GattConnection<'_, '_, P>,
) {
    let rx_handle = server.nus.rx.handle;
    let tx = &server.nus.tx;
    let consumer = BLE_OUTQ.framed_consumer();

    // net 0 is link-local: the host's real net id arrives with its first
    // frame, and the edge processor adopts it. Guessing one here would only
    // be wrong.
    stack.manage_profile(|manager| {
        let _ = manager.set_interface_state(
            (),
            InterfaceState::Active {
                net_id: 0,
                node_id: EDGE_NODE_ID,
            },
        );
    });

    let mut processor = EdgeFrameProcessor::new();

    loop {
        match select(connection.next(), consumer.wait_read()).await {
            Either::First(GattConnectionEvent::Disconnected { .. }) => break,
            Either::First(GattConnectionEvent::Gatt { event }) => {
                if let GattEvent::Write(ref write) = event
                    && write.handle() == rx_handle
                {
                    processor.process_frame(write.data(), &stack, ());
                }
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(_) => warn!("[ble] failed to reply to GATT event"),
                }
            }
            Either::First(_) => {}
            Either::Second(grant) => {
                let frame: heapless::Vec<u8, NUS_MAX_PAYLOAD> =
                    heapless::Vec::from_slice(&grant).unwrap_or_default();
                let failed = tx.notify(connection, &frame).await.is_err();
                grant.release();
                if failed {
                    warn!("[ble] notify failed, dropping connection");
                    break;
                }
            }
        }
    }

    stack.manage_profile(|manager| {
        let _ = manager.set_interface_state((), InterfaceState::Inactive);
    });
}
