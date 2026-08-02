//! BLE NUS transport to the Clawdmeter device.
//!
//! Each ergot frame maps to one GATT write (host → device) or one notification
//! (device → host).
//!
//! Unlike an oxifoc host, which is an edge of the bridge's segment, this
//! daemon is the **controller** of a two-node link: it owns [`NET_ID`] and the
//! device adopts it from the first frame. Both ends running in target mode
//! would leave neither able to assign a net.

use anyhow::{Context, Result, anyhow};
use bluest::{Adapter, Characteristic, Device, Uuid};
use ergot::interface_manager::profiles::direct_edge::{
    CENTRAL_NODE_ID, DirectEdge, EdgeFrameProcessor,
};
use ergot::interface_manager::utils::framed_stream;
use ergot::interface_manager::{FrameProcessor, InterfaceState, Profile};
use ergot::net_stack::ArcNetStack;
use futures::StreamExt;
use std::time::Duration;
use tracing::{debug, error, info, warn};

type StdQueue = ergot::interface_manager::utils::std::StdQueue;

const NUS_SERVICE_UUID: Uuid = Uuid::from_u128(0x6e400001_b5a3_f393_e0a9_e50e24dcca9e);
const NUS_RX_UUID: Uuid = Uuid::from_u128(0x6e400002_b5a3_f393_e0a9_e50e24dcca9e);
const NUS_TX_UUID: Uuid = Uuid::from_u128(0x6e400003_b5a3_f393_e0a9_e50e24dcca9e);

/// Must match the firmware's `NUS_MAX_PAYLOAD`: a notification carries at most
/// ATT_MTU - 3 bytes.
const BLE_MTU: u16 = 505;
const OUT_BUFFER_SIZE: usize = 4096;

/// The net this daemon hands to the device. Any non-zero value works; the link
/// has exactly two nodes.
pub const NET_ID: u16 = 1;

/// Name the firmware advertises.
pub const DEVICE_NAME: &str = "Clawdmeter";

pub struct BleNusInterface;

impl ergot::interface_manager::Interface for BleNusInterface {
    type Sink = framed_stream::Sink<StdQueue>;
}

pub type Stack = ArcNetStack<
    ergot::exports::mutex::raw_impls::cs::CriticalSectionRawMutex,
    DirectEdge<BleNusInterface>,
>;

pub fn new_stack() -> (Stack, StdQueue) {
    let queue = ergot::interface_manager::utils::std::new_std_queue(OUT_BUFFER_SIZE);
    // Starts Down; `connect` flips it Active once the NUS link is attached.
    let stack = Stack::new_with_profile(DirectEdge::new_controller(
        framed_stream::Sink::new_from_handle(queue.clone(), BLE_MTU),
        InterfaceState::Down,
    ));
    (stack, queue)
}

/// Scan until a device advertising [`DEVICE_NAME`] shows up.
pub async fn find_device(timeout: Duration) -> Result<Device> {
    let adapter = Adapter::default()
        .await
        .context("no BLE adapter available")?;
    adapter.wait_available().await?;

    info!("scanning for {DEVICE_NAME}...");
    let mut scan = Box::pin(adapter.scan(&[]).await?);

    let search = async {
        while let Some(advertisement) = scan.next().await {
            if advertisement.adv_data.local_name.as_deref() == Some(DEVICE_NAME) {
                return Ok(advertisement.device);
            }
        }
        Err(anyhow!("scan ended before {DEVICE_NAME} appeared"))
    };

    tokio::time::timeout(timeout, search)
        .await
        .map_err(|_| anyhow!("no {DEVICE_NAME} found within {timeout:?}"))?
}

/// Connect, discover NUS, and attach the link to the stack.
///
/// Returns once both pump tasks are running; they exit when the link drops,
/// which puts the interface back into `Down` so the caller can retry.
pub async fn connect(
    stack: &Stack,
    queue: &StdQueue,
    device: &Device,
    workers: &mut Vec<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    for handle in workers.drain(..) {
        handle.abort();
    }

    let is_down = stack
        .manage_profile(|im| matches!(im.interface_state(()), Some(InterfaceState::Down) | None));
    if !is_down {
        return Err(anyhow!("BLE interface is not Down"));
    }

    let adapter = Adapter::default()
        .await
        .context("no BLE adapter available")?;
    adapter
        .connect_device(device)
        .await
        .context("BLE connect failed")?;
    info!("connected");

    let services = device
        .discover_services_with_uuid(NUS_SERVICE_UUID)
        .await
        .context("NUS service discovery failed")?;
    let nus = services
        .first()
        .ok_or_else(|| anyhow!("device has no NUS service"))?;

    let characteristics = nus
        .discover_characteristics()
        .await
        .context("NUS characteristic discovery failed")?;
    let rx_char = characteristics
        .iter()
        .find(|c| c.uuid() == NUS_RX_UUID)
        .ok_or_else(|| anyhow!("NUS RX characteristic missing"))?
        .clone();
    let tx_char = characteristics
        .iter()
        .find(|c| c.uuid() == NUS_TX_UUID)
        .ok_or_else(|| anyhow!("NUS TX characteristic missing"))?
        .clone();

    stack.manage_profile(|im| {
        let _ = im.set_interface_state(
            (),
            InterfaceState::Active {
                net_id: NET_ID,
                node_id: CENTRAL_NODE_ID,
            },
        );
    });

    let rx_stack = stack.clone();
    workers.push(tokio::spawn(async move {
        match tx_char.notify().await {
            Ok(notifications) => rx_worker(rx_stack, notifications).await,
            Err(e) => error!("failed to subscribe to NUS TX: {e:?}"),
        }
    }));

    let tx_queue = queue.clone();
    workers.push(tokio::spawn(async move {
        tx_worker(tx_queue, rx_char).await;
    }));

    info!("NUS link up (net {NET_ID})");
    Ok(())
}

/// Feed device notifications into the stack.
async fn rx_worker(
    stack: Stack,
    mut notifications: impl futures::Stream<Item = Result<Vec<u8>, bluest::Error>> + Unpin + Send,
) {
    let mut processor = EdgeFrameProcessor::new_controller(NET_ID);

    loop {
        match notifications.next().await {
            Some(Ok(data)) => {
                debug!("rx {} bytes", data.len());
                processor.process_frame(&data, &stack, ());
            }
            Some(Err(e)) => {
                error!("notification error: {e:?}");
                break;
            }
            None => {
                info!("notification stream ended");
                break;
            }
        }
    }

    stack.manage_profile(|im| {
        let _ = im.set_interface_state((), InterfaceState::Down);
    });
    warn!("rx worker exited");
}

/// Drain outbound ergot frames onto the device's RX characteristic.
async fn tx_worker(queue: StdQueue, rx_char: Characteristic) {
    use ergot::exports::bbqueue::traits::bbqhdl::BbqHandle;
    let consumer: ergot::exports::bbqueue::prod_cons::framed::FramedConsumer<StdQueue> =
        queue.framed_consumer();

    loop {
        let frame = consumer.wait_read().await;
        debug!("tx {} bytes", frame.len());
        if let Err(e) = rx_char.write_without_response(&frame).await {
            error!("write error: {e:?}");
            frame.release();
            break;
        }
        frame.release();
    }

    warn!("tx worker exited");
}
