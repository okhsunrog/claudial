//! Persistent display settings in the board's unused NVS partition.
//!
//! The bare-metal BLE stack does not use ESP-IDF NVS. We still resolve the
//! partition by type instead of baking its address into the firmware, then use
//! its first two sectors as alternating, CRC-protected records. The older
//! record remains intact until the newer one has been erased, written, and
//! verified, so a power loss can lose at most the latest edit.

use claudial_icd::settings::{
    DisplaySettings, SETTINGS_RECORD_SIZE, SettingsRecord, decode_record, encode_record,
};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_bootloader_esp_idf::partitions::{
    DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType, read_partition_table,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;

const SLOT_SIZE: u32 = FlashStorage::SECTOR_SIZE;
const SLOT_COUNT: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum Error {
    PartitionTable,
    MissingPartition,
    PartitionTooSmall,
    Flash,
    Verification,
}

pub struct SettingsStore<'d> {
    flash: FlashStorage<'d>,
    partition_offset: u32,
    active_slot: Option<u32>,
    sequence: u32,
    last_saved: Option<DisplaySettings>,
}

impl<'d> SettingsStore<'d> {
    pub fn new(flash: FLASH<'d>) -> Result<(Self, Option<DisplaySettings>), Error> {
        let mut flash = FlashStorage::new(flash).multicore_auto_park();
        let mut table_buffer = [0_u8; PARTITION_TABLE_MAX_LEN];
        let table = read_partition_table(&mut flash, &mut table_buffer)
            .map_err(|_| Error::PartitionTable)?;
        let partition = table
            .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
            .map_err(|_| Error::PartitionTable)?
            .ok_or(Error::MissingPartition)?;
        if partition.len() < SLOT_SIZE * SLOT_COUNT {
            return Err(Error::PartitionTooSmall);
        }

        let mut store = Self {
            flash,
            partition_offset: partition.offset(),
            active_slot: None,
            sequence: 0,
            last_saved: None,
        };
        let selected = store.select_latest()?;
        if let Some((slot, record)) = selected {
            store.active_slot = Some(slot);
            store.sequence = record.sequence;
            store.last_saved = Some(record.settings);
            Ok((store, Some(record.settings)))
        } else {
            Ok((store, None))
        }
    }

    /// Persist settings if they differ from the last verified record.
    pub fn save(&mut self, settings: DisplaySettings) -> Result<bool, Error> {
        if self.last_saved == Some(settings) {
            return Ok(false);
        }

        let target_slot = self.active_slot.map_or(0, |slot| (slot + 1) % SLOT_COUNT);
        let target = self.partition_offset + target_slot * SLOT_SIZE;
        let record = SettingsRecord {
            settings,
            sequence: self.sequence.wrapping_add(1),
        };
        let encoded = encode_record(record);

        self.flash
            .erase(target, target + SLOT_SIZE)
            .map_err(|_| Error::Flash)?;
        self.flash
            .write(target, &encoded)
            .map_err(|_| Error::Flash)?;

        let observed = self.read_slot(target_slot)?;
        if observed != Some(record) {
            return Err(Error::Verification);
        }

        self.active_slot = Some(target_slot);
        self.sequence = record.sequence;
        self.last_saved = Some(settings);
        Ok(true)
    }

    fn select_latest(&mut self) -> Result<Option<(u32, SettingsRecord)>, Error> {
        let first = self.read_slot(0)?;
        let second = self.read_slot(1)?;
        Ok(match (first, second) {
            (None, None) => None,
            (Some(record), None) => Some((0, record)),
            (None, Some(record)) => Some((1, record)),
            (Some(first), Some(second)) => {
                if sequence_is_newer(second.sequence, first.sequence) {
                    Some((1, second))
                } else {
                    Some((0, first))
                }
            }
        })
    }

    fn read_slot(&mut self, slot: u32) -> Result<Option<SettingsRecord>, Error> {
        let mut bytes = [0_u8; SETTINGS_RECORD_SIZE];
        self.flash
            .read(self.partition_offset + slot * SLOT_SIZE, &mut bytes)
            .map_err(|_| Error::Flash)?;
        Ok(decode_record(&bytes))
    }
}

fn sequence_is_newer(candidate: u32, current: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < 1 << 31
}
