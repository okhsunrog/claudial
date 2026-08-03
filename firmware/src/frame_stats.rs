//! Per-frame line-rendering and upload cost.
//!
//! Slint rendering and QSPI DMA overlap inside `render_by_line`, so the useful
//! split is the main pipeline and the final tile flush/drain.

use defmt::{debug, info};

/// Frames reported individually before switching to batch summaries. The first
/// frame is the only full-screen upload and the single most interesting sample.
const DETAILED_FRAMES: u32 = 8;
/// Frames per summary once individual reporting stops.
const SUMMARY_FRAMES: u32 = 16;

/// What one line-rendered frame cost.
#[derive(Clone, Copy, Default)]
pub struct FrameTiming {
    pub pipeline_us: u32,
    pub finish_us: u32,
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
    pipeline_us: u64,
    finish_us: u64,
    pixels: u64,
    transfers: u64,
    worst_pipeline_us: u32,
    worst_finish_us: u32,
}

impl FrameStats {
    pub fn record(&mut self, frame_number: u32, frame: FrameTiming) {
        if frame_number <= DETAILED_FRAMES {
            info!(
                "frame {}: pipeline {} us, finish {} us, {} px in {} rect(s), {} transfer(s)",
                frame_number,
                frame.pipeline_us,
                frame.finish_us,
                frame.pixels,
                frame.rects,
                frame.transfers
            );
            return;
        }

        self.frames += 1;
        self.pipeline_us += u64::from(frame.pipeline_us);
        self.finish_us += u64::from(frame.finish_us);
        self.pixels += frame.pixels;
        self.transfers += u64::from(frame.transfers);
        self.worst_pipeline_us = self.worst_pipeline_us.max(frame.pipeline_us);
        self.worst_finish_us = self.worst_finish_us.max(frame.finish_us);

        if self.frames < SUMMARY_FRAMES {
            return;
        }

        debug!(
            "{} frames: pipeline {} us avg / {} us worst, finish {} us avg / {} us worst, {} px, {} transfers",
            self.frames,
            self.pipeline_us / u64::from(self.frames),
            self.worst_pipeline_us,
            self.finish_us / u64::from(self.frames),
            self.worst_finish_us,
            self.pixels,
            self.transfers
        );
        *self = Self::default();
    }
}
