//! Display policy and its compact, versioned storage value.

pub const MINIMUM_BRIGHTNESS_PERCENT: u8 = 5;
pub const MAXIMUM_BRIGHTNESS_PERCENT: u8 = 100;
pub const BRIGHTNESS_STEP_PERCENT: u8 = 5;
pub const IDLE_TIMEOUT_OPTIONS_SECONDS: [u16; 6] = [30, 60, 120, 300, 600, 1800];

const SETTINGS_FORMAT_VERSION: u8 = 1;
const SETTINGS_FLAG_AUTO_DIM: u8 = 1 << 0;
const SETTINGS_FLAG_DIM_ON_USB: u8 = 1 << 1;
const SETTINGS_FLAGS_MASK: u8 = SETTINGS_FLAG_AUTO_DIM | SETTINGS_FLAG_DIM_ON_USB;

/// Bytes stored as the value of the display-settings map entry.
///
/// `sequential-storage` supplies the record framing, CRC and wear levelling;
/// this value only owns Claudial's schema version and data validation.
pub const SETTINGS_VALUE_SIZE: usize = 8;

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

pub fn encode_settings(settings: DisplaySettings) -> [u8; SETTINGS_VALUE_SIZE] {
    debug_assert!(settings.valid());

    let mut bytes = [0xff; SETTINGS_VALUE_SIZE];
    bytes[0] = SETTINGS_FORMAT_VERSION;
    bytes[1] = (u8::from(settings.auto_dim) * SETTINGS_FLAG_AUTO_DIM)
        | (u8::from(settings.dim_on_usb) * SETTINGS_FLAG_DIM_ON_USB);
    bytes[2] = settings.brightness_percent;
    bytes[3] = settings.dim_brightness_percent;
    bytes[4..6].copy_from_slice(&settings.idle_timeout_seconds.to_le_bytes());
    bytes
}

pub fn decode_settings(bytes: &[u8; SETTINGS_VALUE_SIZE]) -> Option<DisplaySettings> {
    if bytes[0] != SETTINGS_FORMAT_VERSION {
        return None;
    }
    let flags = bytes[1];
    if flags & !SETTINGS_FLAGS_MASK != 0 {
        return None;
    }

    let settings = DisplaySettings {
        brightness_percent: bytes[2],
        auto_dim: flags & SETTINGS_FLAG_AUTO_DIM != 0,
        dim_on_usb: flags & SETTINGS_FLAG_DIM_ON_USB != 0,
        idle_timeout_seconds: u16::from_le_bytes(bytes[4..6].try_into().ok()?),
        dim_brightness_percent: bytes[3],
    };
    settings.valid().then_some(settings)
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
    fn storage_value_round_trips() {
        let settings = DisplaySettings::default();
        let encoded = encode_settings(settings);
        assert_eq!(decode_settings(&encoded), Some(settings));
    }

    #[test]
    fn storage_value_rejects_unsupported_version_and_values() {
        let mut encoded = encode_settings(DisplaySettings::default());
        encoded[0] = SETTINGS_FORMAT_VERSION + 1;
        assert_eq!(decode_settings(&encoded), None);

        encoded = encode_settings(DisplaySettings::default());
        encoded[4..6].copy_from_slice(&17_u16.to_le_bytes());
        assert_eq!(decode_settings(&encoded), None);
    }
}
