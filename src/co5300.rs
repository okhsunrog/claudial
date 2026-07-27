//! Minimal CO5300 QSPI display transport.
//!
//! The controller uses an 8-bit QSPI operation followed by a 24-bit register
//! address. Commands and parameters are sent on SIO0. Pixel payloads are sent
//! over all four data lines.

use bytemuck::cast_slice;
use embedded_hal::delay::DelayNs;
use esp_hal::Blocking;
use esp_hal::gpio::Output;
use esp_hal::spi;
use esp_hal::spi::master::{Address, Command, DataMode, SpiDmaBus};
use slint::platform::software_renderer::{PhysicalRegion, Rgb565BigEndianPixel};

const QSPI_WRITE_COMMAND: u16 = 0x02;
const QSPI_WRITE_PIXELS: u16 = 0x32;
const QSPI_MEMORY_CONTINUE_ADDRESS: u32 = 0x00_3c_00;

const SLEEP_IN: u8 = 0x10;
const SLEEP_OUT: u8 = 0x11;
const DISPLAY_INVERSION_OFF: u8 = 0x20;
const DISPLAY_OFF: u8 = 0x28;
const DISPLAY_ON: u8 = 0x29;
const SET_COLUMN_ADDRESS: u8 = 0x2a;
const SET_PAGE_ADDRESS: u8 = 0x2b;
const MEMORY_WRITE_START: u8 = 0x2c;
const MEMORY_ACCESS_CONTROL: u8 = 0x36;
const INTERFACE_PIXEL_FORMAT: u8 = 0x3a;
const WRITE_CONTROL_DISPLAY_1: u8 = 0x53;
const WRITE_CONTRAST_ENHANCEMENT: u8 = 0x58;
const WRITE_BRIGHTNESS_NORMAL: u8 = 0x51;
const WRITE_BRIGHTNESS_HBM: u8 = 0x63;
const SPI_MODE_CONTROL: u8 = 0xc4;

#[derive(Debug)]
pub enum Error {
    Spi(spi::Error),
    InvalidRegion,
}

impl From<spi::Error> for Error {
    fn from(error: spi::Error) -> Self {
        Self::Spi(error)
    }
}

pub struct Co5300<'d> {
    spi: SpiDmaBus<'d, Blocking>,
    chip_select: Output<'d>,
    reset: Output<'d>,
    width: u16,
    height: u16,
}

impl<'d> Co5300<'d> {
    pub fn new(
        spi: SpiDmaBus<'d, Blocking>,
        chip_select: Output<'d>,
        reset: Output<'d>,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            spi,
            chip_select,
            reset,
            width,
            height,
        }
    }

    /// Reset and initialize the panel using Waveshare's CO5300 sequence.
    pub fn init(&mut self, delay: &mut impl DelayNs) -> Result<(), Error> {
        self.chip_select.set_high();
        self.reset.set_high();
        delay.delay_ms(10);
        self.reset.set_low();
        delay.delay_ms(200);
        self.reset.set_high();
        delay.delay_ms(200);

        self.write_command(SLEEP_OUT, &[])?;
        delay.delay_ms(120);

        self.write_command(0xfe, &[0x00])?;
        self.write_command(SPI_MODE_CONTROL, &[0x80])?;
        self.write_command(INTERFACE_PIXEL_FORMAT, &[0x55])?;
        self.write_command(WRITE_CONTROL_DISPLAY_1, &[0x20])?;
        self.write_command(WRITE_BRIGHTNESS_HBM, &[0xff])?;
        self.write_command(DISPLAY_INVERSION_OFF, &[])?;
        self.write_command(DISPLAY_ON, &[])?;
        self.write_command(WRITE_BRIGHTNESS_NORMAL, &[0xd0])?;
        self.write_command(WRITE_CONTRAST_ENHANCEMENT, &[0x00])?;

        // The board-specific Waveshare examples override MADCTL with 0xa0
        // after the generic CO5300 initialization.
        self.write_command(MEMORY_ACCESS_CONTROL, &[0xa0])?;
        delay.delay_ms(10);

        Ok(())
    }

    pub fn set_brightness(&mut self, brightness: u8) -> Result<(), Error> {
        self.write_command(WRITE_BRIGHTNESS_NORMAL, &[brightness])
    }

    /// Blank the panel and enter its low-power sleep mode.
    pub fn sleep(&mut self) -> Result<(), Error> {
        self.write_command(DISPLAY_OFF, &[])?;
        self.write_command(SLEEP_IN, &[])
    }

    /// Leave low-power sleep mode.
    ///
    /// The caller must wait at least 120 ms before calling [`Self::display_on`].
    pub fn wake(&mut self) -> Result<(), Error> {
        self.write_command(SLEEP_OUT, &[])
    }

    /// Enable panel output after the sleep-out delay has elapsed.
    pub fn display_on(&mut self) -> Result<(), Error> {
        self.write_command(DISPLAY_ON, &[])
    }

    /// Send every rectangle rendered by Slint to the panel.
    ///
    /// CO5300 requires even start coordinates and even dimensions. Slint's
    /// `DirtyRegionAlignment(2, 2)` guarantees that invariant; it is checked
    /// again here before any panel command is emitted.
    pub fn write_region(
        &mut self,
        framebuffer: &[Rgb565BigEndianPixel],
        stride: usize,
        region: &PhysicalRegion,
    ) -> Result<(), Error> {
        for (position, size) in region.iter() {
            let x = u16::try_from(position.x).map_err(|_| Error::InvalidRegion)?;
            let y = u16::try_from(position.y).map_err(|_| Error::InvalidRegion)?;
            let width = u16::try_from(size.width).map_err(|_| Error::InvalidRegion)?;
            let height = u16::try_from(size.height).map_err(|_| Error::InvalidRegion)?;

            self.validate_region(x, y, width, height, framebuffer.len(), stride)?;
            self.set_address_window(x, y, width, height)?;
            self.write_command(MEMORY_WRITE_START, &[])?;

            self.chip_select.set_low();
            let mut first_chunk = true;
            let transfer_result = (|| {
                for row in y as usize..(y + height) as usize {
                    let begin = row * stride + x as usize;
                    let end = begin + width as usize;
                    let bytes = cast_slice(&framebuffer[begin..end]);

                    let command = if first_chunk {
                        Command::_8Bit(QSPI_WRITE_PIXELS, DataMode::Single)
                    } else {
                        Command::None
                    };
                    let address = if first_chunk {
                        Address::_24Bit(QSPI_MEMORY_CONTINUE_ADDRESS, DataMode::Single)
                    } else {
                        Address::None
                    };

                    self.spi
                        .half_duplex_write(DataMode::Quad, command, address, 0, bytes)?;
                    first_chunk = false;
                }
                Ok::<(), spi::Error>(())
            })();
            self.chip_select.set_high();
            transfer_result?;
        }

        Ok(())
    }

    fn validate_region(
        &self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        framebuffer_len: usize,
        stride: usize,
    ) -> Result<(), Error> {
        let end_x = x.checked_add(width).ok_or(Error::InvalidRegion)?;
        let end_y = y.checked_add(height).ok_or(Error::InvalidRegion)?;

        if width == 0
            || height == 0
            || !x.is_multiple_of(2)
            || !y.is_multiple_of(2)
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || end_x > self.width
            || end_y > self.height
            || stride < self.width as usize
            || framebuffer_len < stride * self.height as usize
        {
            return Err(Error::InvalidRegion);
        }

        Ok(())
    }

    fn set_address_window(&mut self, x: u16, y: u16, width: u16, height: u16) -> Result<(), Error> {
        let end_x = x + width - 1;
        let end_y = y + height - 1;
        self.write_command(
            SET_COLUMN_ADDRESS,
            &[(x >> 8) as u8, x as u8, (end_x >> 8) as u8, end_x as u8],
        )?;
        self.write_command(
            SET_PAGE_ADDRESS,
            &[(y >> 8) as u8, y as u8, (end_y >> 8) as u8, end_y as u8],
        )
    }

    fn write_command(&mut self, command: u8, parameters: &[u8]) -> Result<(), Error> {
        self.chip_select.set_low();
        let result = self.spi.half_duplex_write(
            DataMode::Single,
            Command::_8Bit(QSPI_WRITE_COMMAND, DataMode::Single),
            Address::_24Bit((command as u32) << 8, DataMode::Single),
            0,
            parameters,
        );
        self.chip_select.set_high();
        result.map_err(Error::Spi)
    }
}
