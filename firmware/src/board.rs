//! Hardware constants for the Waveshare ESP32-S3-Touch-AMOLED-2.16.
//!
//! # Pin assignments
//!
//! `esp-hal` addresses pins as distinct types (`peripherals.GPIO4`), not by
//! number, so these cannot be expressed as constants that the code actually
//! uses. They are recorded here as documentation; the wiring itself lives in
//! `main`, which is the only place that can be authoritative.
//!
//! | Signal | GPIO | | Signal | GPIO |
//! |---|---|---|---|---|
//! | `LCD_SIO0` | 4 | | `I2C_SCL` | 14 |
//! | `LCD_SIO1` | 5 | | `I2C_SDA` | 15 |
//! | `LCD_SIO2` | 6 | | `TP_INT` | 11 |
//! | `LCD_SIO3` | 7 | | `TP_RST` | 40 |
//! | `LCD_CS` | 12 | | `RTC_INT` | 13 |
//! | `LCD_SCK` | 38 | | `USER_BTN` | 18 |
//! | `LCD_RST` | 39 | | | |

pub const DISPLAY_WIDTH: u16 = 480;
pub const DISPLAY_HEIGHT: u16 = 480;
pub const DISPLAY_SPI_MHZ: u32 = 40;
