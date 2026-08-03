//! Minimal CO5300 QSPI display transport.
//!
//! The controller uses an 8-bit QSPI operation followed by a 24-bit register
//! address. Commands and parameters are sent on SIO0. Pixel payloads are sent
//! over all four data lines.
//!
//! # No tearing control on this board
//!
//! Uploads cannot be synchronised with the panel's scan-out, so a large enough
//! damaged region shows a visible boundary between the old and new frame while
//! it is being written. This was chased on hardware and is a dead end:
//!
//! - the TE line is tied to GND on this board, so there is no tear signal;
//! - reading the scanline register `GETSL` (0x45) always returns 0, as does a
//!   control read of `ID1`, on SIO0 and SIO1 alike and with the official
//!   `SPIRON` (0x47) read enable, at 10 MHz. No SPI errors, just no data.
//!
//! So there is no way to poll the current scanline or wait for vblank, and the
//! only lever left is keeping damaged regions small — which is a property of
//! what the UI animates, not of this transport. Resist the temptation to add
//! upload-order tricks here: writing in vertical strips does remove the
//! diagonal boundary, but replaces it with a visible left-to-right assembly of
//! the frame, which reads worse.
//!
//! The 40 MHz clock is deliberate. The datasheet's minimum write clock period
//! of 20 ns puts the ceiling at 50 MHz, so 80 MHz would be out of spec.

use core::mem::size_of;
use core::ops::Range;

use bytemuck::cast_slice_mut;
use embedded_hal::delay::DelayNs;
use esp_hal::Blocking;
use esp_hal::dma::DmaTxBuf;
use esp_hal::gpio::Output;
use esp_hal::spi;
use esp_hal::spi::master::{Address, Command, DataMode, SpiDma, SpiDmaTransfer};
use heapless::Vec;
use slint::platform::software_renderer::{LineBufferProvider, Rgb565BigEndianPixel};

/// Dimmest setting the UI offers. The panel is legible below this, but the
/// steppers would otherwise walk it down to fully black.
pub const MINIMUM_BRIGHTNESS_PERCENT: u8 = 5;

/// Map a percentage onto the panel's `0x51` brightness register.
pub fn brightness_register(percent: u8) -> u8 {
    let percent = percent.clamp(MINIMUM_BRIGHTNESS_PERCENT, 100);
    ((u16::from(percent) * u16::from(u8::MAX) + 50) / 100) as u8
}

/// Number of display rows accumulated before a QSPI upload.
///
/// Eight is deliberately even, preserving the CO5300's 2x2 address-window
/// requirement when an aligned dirty rectangle crosses a tile boundary.
pub const TILE_LINES: usize = 8;
/// Size of each of the two internal DMA-capable buffers: 480 x 8 x RGB565.
pub const TILE_BUFFER_BYTES: usize = 7680;

const BYTES_PER_PIXEL: usize = size_of::<Rgb565BigEndianPixel>();
// A Slint PhysicalRegion currently contains at most three rectangles, hence at
// most three callbacks per line. Keep spare capacity so a future increase is a
// recoverable frame error instead of a memory overwrite.
const MAX_DIRTY_SPANS: usize = TILE_LINES * 4;
const MAX_DIRTY_RECTS: usize = MAX_DIRTY_SPANS;

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
    BufferTooSmall,
    InvalidRegion,
    MissingResource,
}

impl From<spi::Error> for Error {
    fn from(error: spi::Error) -> Self {
        Self::Spi(error)
    }
}

pub struct Co5300<'d> {
    spi: Option<SpiDma<'d, Blocking>>,
    render_buffer: DmaTxBuf,
    transmit_buffer: Option<DmaTxBuf>,
    pending: Option<SpiDmaTransfer<'d, Blocking, DmaTxBuf>>,
    chip_select: Output<'d>,
    reset: Output<'d>,
    width: u16,
    height: u16,
    band_y: Option<u16>,
    dirty_spans: Vec<DirtySpan, MAX_DIRTY_SPANS>,
    frame_transfers: u32,
    frame_error: Option<Error>,
}

#[derive(Clone, Copy)]
struct DirtySpan {
    line: u16,
    start: u16,
    end: u16,
}

#[derive(Clone, Copy)]
struct DirtyRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl<'d> Co5300<'d> {
    pub fn new(
        spi: SpiDma<'d, Blocking>,
        chip_select: Output<'d>,
        reset: Output<'d>,
        render_buffer: DmaTxBuf,
        transmit_buffer: DmaTxBuf,
        width: u16,
        height: u16,
    ) -> Result<Self, Error> {
        let required_capacity = width as usize * TILE_LINES * BYTES_PER_PIXEL;
        if required_capacity > TILE_BUFFER_BYTES
            || render_buffer.capacity() < required_capacity
            || transmit_buffer.capacity() < required_capacity
        {
            return Err(Error::BufferTooSmall);
        }
        debug_assert!(
            (render_buffer.as_slice().as_ptr() as usize).is_multiple_of(BYTES_PER_PIXEL),
            "render DMA buffer must be aligned for RGB565 pixels"
        );

        Ok(Self {
            spi: Some(spi),
            render_buffer,
            transmit_buffer: Some(transmit_buffer),
            pending: None,
            chip_select,
            reset,
            width,
            height,
            band_y: None,
            dirty_spans: Vec::new(),
            frame_transfers: 0,
            frame_error: None,
        })
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

    /// Flush the final tile, wait for its DMA transfer, and finish one Slint frame.
    pub fn finish_frame(&mut self) -> Result<u32, Error> {
        let result = if let Some(error) = self.frame_error.take() {
            Err(error)
        } else {
            self.flush_band()
        };
        self.wait_pending();

        let transfers = self.frame_transfers;
        self.frame_transfers = 0;
        self.band_y = None;
        self.dirty_spans.clear();

        result.map(|()| transfers)
    }

    fn set_frame_error(&mut self, error: Error) {
        if self.frame_error.is_none() {
            self.frame_error = Some(error);
        }
    }

    fn flush_band(&mut self) -> Result<(), Error> {
        let Some(band_y) = self.band_y else {
            return Ok(());
        };

        let mut rectangles = Vec::<DirtyRect, MAX_DIRTY_RECTS>::new();
        for span in self.dirty_spans.iter().copied() {
            let width = span
                .end
                .checked_sub(span.start)
                .ok_or(Error::InvalidRegion)?;
            let mut extended = false;
            for rectangle in rectangles.iter_mut().rev() {
                if rectangle.x == span.start
                    && rectangle.width == width
                    && rectangle.y + rectangle.height == span.line
                {
                    rectangle.height += 1;
                    extended = true;
                    break;
                }
            }
            if !extended {
                rectangles
                    .push(DirtyRect {
                        x: span.start,
                        y: span.line,
                        width,
                        height: 1,
                    })
                    .map_err(|_| Error::InvalidRegion)?;
            }
        }

        for rectangle in rectangles {
            self.write_rectangle(band_y, rectangle)?;
        }

        self.band_y = None;
        self.dirty_spans.clear();
        Ok(())
    }

    fn write_rectangle(&mut self, band_y: u16, rectangle: DirtyRect) -> Result<(), Error> {
        let DirtyRect {
            x,
            y,
            width,
            height,
        } = rectangle;
        if width == 0
            || height == 0
            || (x | y | width | height) & 1 != 0
            || x.checked_add(width).is_none_or(|end| end > self.width)
            || y.checked_add(height).is_none_or(|end| end > self.height)
            || y < band_y
            || usize::from(y - band_y) + usize::from(height) > TILE_LINES
        {
            return Err(Error::InvalidRegion);
        }

        self.wait_pending();
        self.set_address_window(x, y, width, height)?;
        self.write_command(MEMORY_WRITE_START, &[])?;

        let row_bytes = usize::from(width) * BYTES_PER_PIXEL;
        let byte_len = row_bytes * usize::from(height);
        if byte_len > TILE_BUFFER_BYTES {
            return Err(Error::BufferTooSmall);
        }

        let source_stride = usize::from(self.width) * BYTES_PER_PIXEL;
        let source_x = usize::from(x) * BYTES_PER_PIXEL;
        let source_row = usize::from(y - band_y);
        let render_bytes = self.render_buffer.as_slice();
        let transmit_buffer = self
            .transmit_buffer
            .as_mut()
            .ok_or(Error::MissingResource)?;
        let transmit_bytes = transmit_buffer.as_mut_slice();
        for row in 0..usize::from(height) {
            let source_begin = (source_row + row) * source_stride + source_x;
            let source_end = source_begin + row_bytes;
            let target_begin = row * row_bytes;
            transmit_bytes[target_begin..target_begin + row_bytes]
                .copy_from_slice(&render_bytes[source_begin..source_end]);
        }

        self.start_pixel_transfer(byte_len)?;
        self.frame_transfers += 1;
        Ok(())
    }

    fn start_pixel_transfer(&mut self, byte_len: usize) -> Result<(), Error> {
        let spi = self.spi.take().ok_or(Error::MissingResource)?;
        let mut transmit_buffer = self.transmit_buffer.take().ok_or(Error::MissingResource)?;
        transmit_buffer.set_length(byte_len);
        self.chip_select.set_low();

        match spi.half_duplex_write(
            DataMode::Quad,
            Command::_8Bit(QSPI_WRITE_PIXELS, DataMode::Single),
            Address::_24Bit(QSPI_MEMORY_CONTINUE_ADDRESS, DataMode::Single),
            0,
            byte_len,
            transmit_buffer,
        ) {
            Ok(transfer) => {
                self.pending = Some(transfer);
                Ok(())
            }
            Err((error, spi, transmit_buffer)) => {
                self.chip_select.set_high();
                self.spi = Some(spi);
                self.transmit_buffer = Some(transmit_buffer);
                Err(Error::Spi(error))
            }
        }
    }

    fn wait_pending(&mut self) {
        if let Some(transfer) = self.pending.take() {
            let (spi, transmit_buffer) = transfer.wait();
            self.spi = Some(spi);
            self.transmit_buffer = Some(transmit_buffer);
            self.chip_select.set_high();
        }
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
        self.wait_pending();

        let spi = self.spi.take().ok_or(Error::MissingResource)?;
        let mut transmit_buffer = self.transmit_buffer.take().ok_or(Error::MissingResource)?;
        transmit_buffer.fill(parameters);
        self.chip_select.set_low();
        let result = spi.half_duplex_write(
            DataMode::Single,
            Command::_8Bit(QSPI_WRITE_COMMAND, DataMode::Single),
            Address::_24Bit(u32::from(command) << 8, DataMode::Single),
            0,
            parameters.len(),
            transmit_buffer,
        );
        match result {
            Ok(transfer) => {
                let (spi, transmit_buffer) = transfer.wait();
                self.chip_select.set_high();
                self.spi = Some(spi);
                self.transmit_buffer = Some(transmit_buffer);
                Ok(())
            }
            Err((error, spi, transmit_buffer)) => {
                self.chip_select.set_high();
                self.spi = Some(spi);
                self.transmit_buffer = Some(transmit_buffer);
                Err(Error::Spi(error))
            }
        }
    }
}

impl LineBufferProvider for &mut Co5300<'_> {
    type TargetPixel = Rgb565BigEndianPixel;

    fn process_line(
        &mut self,
        line: usize,
        range: Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        if self.frame_error.is_some() || range.is_empty() {
            return;
        }
        if line >= usize::from(self.height) || range.end > usize::from(self.width) {
            self.set_frame_error(Error::InvalidRegion);
            return;
        }

        let band_y = line / TILE_LINES * TILE_LINES;
        if self
            .band_y
            .is_some_and(|current| usize::from(current) != band_y)
            && let Err(error) = self.flush_band()
        {
            self.set_frame_error(error);
            return;
        }
        self.band_y.get_or_insert(band_y as u16);

        let row = line - band_y;
        let pixel_begin = row * usize::from(self.width) + range.start;
        let pixel_end = pixel_begin + range.len();
        let pixel_capacity = self.render_buffer.capacity() / BYTES_PER_PIXEL;
        if pixel_end > pixel_capacity {
            self.set_frame_error(Error::BufferTooSmall);
            return;
        }

        let pixels = cast_slice_mut::<u8, Rgb565BigEndianPixel>(
            &mut self.render_buffer.as_mut_slice()[..pixel_capacity * BYTES_PER_PIXEL],
        );
        render_fn(&mut pixels[pixel_begin..pixel_end]);

        let span = DirtySpan {
            line: line as u16,
            start: range.start as u16,
            end: range.end as u16,
        };
        if self.dirty_spans.push(span).is_err() {
            self.set_frame_error(Error::InvalidRegion);
        }
    }
}
