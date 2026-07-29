//! Per-frame render and upload cost.
//!
//! Rendering into the PSRAM framebuffer and pushing damaged rectangles over
//! QSPI are timed separately because they are optimized independently.

use defmt::{debug, info};

/// Frames reported individually before switching to batch summaries. The first
/// frame is the only full-screen upload and the single most interesting sample.
const DETAILED_FRAMES: u32 = 8;
/// Frames per summary once individual reporting stops.
const SUMMARY_FRAMES: u32 = 16;

/// What one frame cost, split into the two halves that are optimized
/// independently: rendering into the PSRAM framebuffer, and pushing the damaged
/// rectangles out over QSPI.
#[derive(Clone, Copy, Default)]
pub struct FrameTiming {
    pub render_us: u32,
    pub upload_us: u32,
    pub pixels: u64,
    pub rects: u32,
    pub transfers: u32,
}

/// Rolling frame-cost accumulator.
///
/// Logging every frame would both flood RTT and perturb what it measures, so
/// only the first few frames are reported individually, at `info`, as a boot
/// benchmark. The ongoing batch summaries are logged at `debug` so they stay
/// out of the way until someone is actually looking for them.
#[derive(Default)]
pub struct FrameStats {
    frames: u32,
    render_us: u64,
    upload_us: u64,
    pixels: u64,
    transfers: u64,
    worst_render_us: u32,
    worst_upload_us: u32,
}

impl FrameStats {
    pub fn record(&mut self, frame_number: u32, frame: FrameTiming) {
        if frame_number <= DETAILED_FRAMES {
            info!(
                "frame {}: render {} us, upload {} us, {} px in {} rect(s), {} transfer(s)",
                frame_number,
                frame.render_us,
                frame.upload_us,
                frame.pixels,
                frame.rects,
                frame.transfers
            );
            return;
        }

        self.frames += 1;
        self.render_us += u64::from(frame.render_us);
        self.upload_us += u64::from(frame.upload_us);
        self.pixels += frame.pixels;
        self.transfers += u64::from(frame.transfers);
        self.worst_render_us = self.worst_render_us.max(frame.render_us);
        self.worst_upload_us = self.worst_upload_us.max(frame.upload_us);

        if self.frames < SUMMARY_FRAMES {
            return;
        }

        debug!(
            "{} frames: render {} us avg / {} us worst, upload {} us avg / {} us worst, {} px, {} transfers",
            self.frames,
            self.render_us / u64::from(self.frames),
            self.worst_render_us,
            self.upload_us / u64::from(self.frames),
            self.worst_upload_us,
            self.pixels,
            self.transfers
        );
        *self = Self::default();
    }
}
