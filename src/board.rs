//! Hardware constants for the Waveshare ESP32-S3-Touch-AMOLED-2.16.

pub const DISPLAY_WIDTH: u16 = 480;
pub const DISPLAY_HEIGHT: u16 = 480;
pub const DISPLAY_SPI_MHZ: u32 = 40;

pub const LCD_SIO0_GPIO: u8 = 4;
pub const LCD_SIO1_GPIO: u8 = 5;
pub const LCD_SIO2_GPIO: u8 = 6;
pub const LCD_SIO3_GPIO: u8 = 7;
pub const LCD_CS_GPIO: u8 = 12;
pub const LCD_SCK_GPIO: u8 = 38;
pub const LCD_RESET_GPIO: u8 = 39;

pub const I2C_SCL_GPIO: u8 = 14;
pub const I2C_SDA_GPIO: u8 = 15;
pub const TOUCH_INTERRUPT_GPIO: u8 = 11;
pub const TOUCH_RESET_GPIO: u8 = 40;

pub const USER_BUTTON_GPIO: u8 = 18;
