//! Persistent display settings in the board's NVS partition.
//!
//! Claudial owns this entire partition: the bare-metal firmware does not use
//! ESP-IDF NVS. `sequential-storage` appends CRC-protected values and rotates
//! through every flash page, providing power-fail recovery and wear levelling
//! without requiring a custom partition table.

use claudial_icd::settings::{
    DisplaySettings, SETTINGS_VALUE_SIZE, decode_settings, encode_settings,
};
use embassy_embedded_hal::adapter::{BlockingAsync, YieldingAsync};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_bootloader_esp_idf::partitions::{
    DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType, read_partition_table,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use sequential_storage::cache::{Cache, Uncached};
use sequential_storage::map::{MapConfig, MapStorage};

const SETTINGS_KEY: u8 = 1;
const DATA_BUFFER_SIZE: usize = 32;

type AsyncFlash<'d> = YieldingAsync<BlockingAsync<FlashStorage<'d>>>;
type UncachedMap<'d> = MapStorage<u8, AsyncFlash<'d>, Cache<Uncached, Uncached, Uncached, u8>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum Error {
    PartitionTable,
    MissingPartition,
    PartitionTooSmall,
    Storage,
    Verification,
}

pub struct SettingsStore<'d> {
    storage: UncachedMap<'d>,
    last_saved: Option<DisplaySettings>,
}

impl<'d> SettingsStore<'d> {
    pub async fn new(flash: FLASH<'d>) -> Result<(Self, Option<DisplaySettings>), Error> {
        let mut flash = FlashStorage::new(flash).multicore_auto_park();
        let mut table_buffer = [0_u8; PARTITION_TABLE_MAX_LEN];
        let table = read_partition_table(&mut flash, &mut table_buffer)
            .map_err(|_| Error::PartitionTable)?;
        let partition = table
            .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
            .map_err(|_| Error::PartitionTable)?
            .ok_or(Error::MissingPartition)?;
        let partition_range = partition.offset()..partition.offset() + partition.len();

        // The immediately preceding firmware used a pair of records beginning
        // with CLDS. Do not migrate disposable settings, but do start the
        // sequential store from erased pages instead of mixing both layouts.
        let mut legacy_magic = [0_u8; 4];
        flash
            .read(partition.offset(), &mut legacy_magic)
            .map_err(|_| Error::Storage)?;
        if legacy_magic == *b"CLDS" {
            defmt::info!("Erasing legacy display settings storage");
            flash
                .erase(partition_range.start, partition_range.end)
                .map_err(|_| Error::Storage)?;
        }

        let flash = YieldingAsync::new(BlockingAsync::new(flash));
        let config = MapConfig::try_new(partition_range).map_err(|_| Error::PartitionTooSmall)?;

        let mut store = Self {
            storage: MapStorage::new(flash, config, Cache::new_uncached()),
            last_saved: None,
        };
        let settings = match store.load().await {
            Ok(settings) => settings,
            Err(()) => {
                // Settings are disposable, so an unrecoverable map starts
                // fresh instead of blocking the rest of the firmware.
                defmt::warn!("Resetting unrecognised display settings storage");
                store
                    .storage
                    .erase_all()
                    .await
                    .map_err(|_| Error::Storage)?;
                None
            }
        };
        store.last_saved = settings;
        Ok((store, settings))
    }

    /// Persist settings if they differ from the last verified record.
    pub async fn save(&mut self, settings: DisplaySettings) -> Result<bool, Error> {
        if self.last_saved == Some(settings) {
            return Ok(false);
        }

        let encoded = encode_settings(settings);
        let mut buffer = [0_u8; DATA_BUFFER_SIZE];
        self.storage
            .store_item(&mut buffer, &SETTINGS_KEY, &encoded)
            .await
            .map_err(|_| Error::Storage)?;

        if self.load().await.map_err(|_| Error::Storage)? != Some(settings) {
            return Err(Error::Verification);
        }

        self.last_saved = Some(settings);
        Ok(true)
    }

    async fn load(&mut self) -> Result<Option<DisplaySettings>, ()> {
        let mut buffer = [0_u8; DATA_BUFFER_SIZE];
        self.storage
            .fetch_item::<[u8; SETTINGS_VALUE_SIZE]>(&mut buffer, &SETTINGS_KEY)
            .await
            .map(|value| value.as_ref().and_then(decode_settings))
            .map_err(|_| ())
    }
}
