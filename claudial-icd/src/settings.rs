//! Display policy and its compact persistent representation.

pub const MINIMUM_BRIGHTNESS_PERCENT: u8 = 5;
pub const MAXIMUM_BRIGHTNESS_PERCENT: u8 = 100;
pub const BRIGHTNESS_STEP_PERCENT: u8 = 5;
pub const IDLE_TIMEOUT_OPTIONS_SECONDS: [u16; 6] = [30, 60, 120, 300, 600, 1800];

const SETTINGS_MAGIC: [u8; 4] = *b"CLDS";
const SETTINGS_FORMAT_VERSION: u8 = 1;
const SETTINGS_FLAG_AUTO_DIM: u8 = 1 << 0;
const SETTINGS_FLAG_DIM_ON_USB: u8 = 1 << 1;
const SETTINGS_FLAGS_MASK: u8 = SETTINGS_FLAG_AUTO_DIM | SETTINGS_FLAG_DIM_ON_USB;
const SETTINGS_CRC_OFFSET: usize = 16;

pub const SETTINGS_RECORD_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplaySettings {
    pub brightness_percent: u8,
    pub auto_dim: bool,
    pub dim_on_usb: bool,
    pub idle_timeout_seconds: u16,
    pub dim_brightness_percent: u8,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            brightness_percent: 80,
            auto_dim: true,
            // A desk instrument should remain glanceable while it has USB
            // power. Battery operation retains the original two-minute dim.
            dim_on_usb: false,
            idle_timeout_seconds: 120,
            dim_brightness_percent: 12,
        }
    }
}

impl DisplaySettings {
    pub fn brightness_step(&mut self, direction: i8) {
        self.brightness_percent = stepped_percent(self.brightness_percent, direction);
        self.dim_brightness_percent = self
            .dim_brightness_percent
            .min(self.brightness_percent)
            .max(MINIMUM_BRIGHTNESS_PERCENT);
    }

    pub fn dim_brightness_step(&mut self, direction: i8) {
        self.dim_brightness_percent =
            stepped_percent(self.dim_brightness_percent, direction).min(self.brightness_percent);
    }

    pub fn idle_timeout_step(&mut self, direction: i8) {
        let index = IDLE_TIMEOUT_OPTIONS_SECONDS
            .iter()
            .position(|seconds| *seconds == self.idle_timeout_seconds)
            .unwrap_or(2);
        let next = if direction < 0 {
            index.saturating_sub(1)
        } else if direction > 0 {
            (index + 1).min(IDLE_TIMEOUT_OPTIONS_SECONDS.len() - 1)
        } else {
            index
        };
        self.idle_timeout_seconds = IDLE_TIMEOUT_OPTIONS_SECONDS[next];
    }

    pub fn should_dim(self, usb_connected: bool) -> bool {
        self.auto_dim && (!usb_connected || self.dim_on_usb)
    }

    fn valid(self) -> bool {
        (MINIMUM_BRIGHTNESS_PERCENT..=MAXIMUM_BRIGHTNESS_PERCENT).contains(&self.brightness_percent)
            && (MINIMUM_BRIGHTNESS_PERCENT..=self.brightness_percent)
                .contains(&self.dim_brightness_percent)
            && IDLE_TIMEOUT_OPTIONS_SECONDS.contains(&self.idle_timeout_seconds)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsRecord {
    pub settings: DisplaySettings,
    pub sequence: u32,
}

pub fn encode_record(record: SettingsRecord) -> [u8; SETTINGS_RECORD_SIZE] {
    debug_assert!(record.settings.valid());

    let mut bytes = [0xff; SETTINGS_RECORD_SIZE];
    bytes[..4].copy_from_slice(&SETTINGS_MAGIC);
    bytes[4] = SETTINGS_FORMAT_VERSION;
    bytes[5] = (u8::from(record.settings.auto_dim) * SETTINGS_FLAG_AUTO_DIM)
        | (u8::from(record.settings.dim_on_usb) * SETTINGS_FLAG_DIM_ON_USB);
    bytes[6] = record.settings.brightness_percent;
    bytes[7] = record.settings.dim_brightness_percent;
    bytes[8..10].copy_from_slice(&record.settings.idle_timeout_seconds.to_le_bytes());
    bytes[12..16].copy_from_slice(&record.sequence.to_le_bytes());
    let crc = crc32(&bytes[..SETTINGS_CRC_OFFSET]);
    bytes[SETTINGS_CRC_OFFSET..SETTINGS_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
    bytes
}

pub fn decode_record(bytes: &[u8; SETTINGS_RECORD_SIZE]) -> Option<SettingsRecord> {
    if bytes[..4] != SETTINGS_MAGIC || bytes[4] != SETTINGS_FORMAT_VERSION {
        return None;
    }
    let flags = bytes[5];
    if flags & !SETTINGS_FLAGS_MASK != 0 {
        return None;
    }
    let expected_crc = u32::from_le_bytes(
        bytes[SETTINGS_CRC_OFFSET..SETTINGS_CRC_OFFSET + 4]
            .try_into()
            .ok()?,
    );
    if crc32(&bytes[..SETTINGS_CRC_OFFSET]) != expected_crc {
        return None;
    }

    let settings = DisplaySettings {
        brightness_percent: bytes[6],
        auto_dim: flags & SETTINGS_FLAG_AUTO_DIM != 0,
        dim_on_usb: flags & SETTINGS_FLAG_DIM_ON_USB != 0,
        idle_timeout_seconds: u16::from_le_bytes(bytes[8..10].try_into().ok()?),
        dim_brightness_percent: bytes[7],
    };
    settings.valid().then_some(SettingsRecord {
        settings,
        sequence: u32::from_le_bytes(bytes[12..16].try_into().ok()?),
    })
}

fn stepped_percent(percent: u8, direction: i8) -> u8 {
    if direction < 0 {
        percent
            .saturating_sub(BRIGHTNESS_STEP_PERCENT)
            .max(MINIMUM_BRIGHTNESS_PERCENT)
    } else if direction > 0 {
        percent
            .saturating_add(BRIGHTNESS_STEP_PERCENT)
            .min(MAXIMUM_BRIGHTNESS_PERCENT)
    } else {
        percent
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let polynomial = 0xedb8_8320 & (0_u32.wrapping_sub(crc & 1));
            crc = (crc >> 1) ^ polynomial;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_only_dims_on_battery() {
        let settings = DisplaySettings::default();
        assert!(settings.should_dim(false));
        assert!(!settings.should_dim(true));
    }

    #[test]
    fn controls_clamp_to_supported_values() {
        let mut settings = DisplaySettings::default();
        for _ in 0..30 {
            settings.brightness_step(-1);
            settings.dim_brightness_step(-1);
            settings.idle_timeout_step(-1);
        }
        assert_eq!(settings.brightness_percent, MINIMUM_BRIGHTNESS_PERCENT);
        assert_eq!(settings.dim_brightness_percent, MINIMUM_BRIGHTNESS_PERCENT);
        assert_eq!(settings.idle_timeout_seconds, 30);

        for _ in 0..30 {
            settings.brightness_step(1);
            settings.dim_brightness_step(1);
            settings.idle_timeout_step(1);
        }
        assert_eq!(settings.brightness_percent, MAXIMUM_BRIGHTNESS_PERCENT);
        assert_eq!(settings.dim_brightness_percent, MAXIMUM_BRIGHTNESS_PERCENT);
        assert_eq!(settings.idle_timeout_seconds, 1800);
    }

    #[test]
    fn record_round_trips_and_detects_corruption() {
        let record = SettingsRecord {
            settings: DisplaySettings::default(),
            sequence: 42,
        };
        let mut encoded = encode_record(record);
        assert_eq!(decode_record(&encoded), Some(record));

        encoded[7] ^= 1;
        assert_eq!(decode_record(&encoded), None);
    }

    #[test]
    fn record_rejects_unsupported_values_even_with_a_valid_crc() {
        let record = SettingsRecord {
            settings: DisplaySettings::default(),
            sequence: 1,
        };
        let mut encoded = encode_record(record);
        encoded[8..10].copy_from_slice(&17_u16.to_le_bytes());
        let crc = crc32(&encoded[..SETTINGS_CRC_OFFSET]);
        encoded[SETTINGS_CRC_OFFSET..SETTINGS_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());

        assert_eq!(decode_record(&encoded), None);
    }
}
