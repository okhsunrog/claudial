//! Palette-indexed sprite animation.
//!
//! A frame is a 20 x 20 grid of palette indices rather than pixels, so one
//! frame costs 400 bytes plus a shared 10-entry palette. At 15 px per cell
//! that fills the panel from data small enough to keep many animations in
//! flash.
//!
//! Frames carry their own hold time, so an animation can linger on a pose
//! without padding the sequence with duplicate frames.

use embassy_time::Duration;

/// Cells per side.
pub const GRID: usize = 20;
/// Cells per frame.
pub const CELLS: usize = GRID * GRID;
/// Colours available to a frame, index 0 being the background.
pub const PALETTE_LEN: usize = 10;

/// One frame: a palette index per cell, and how long it stays up.
pub struct Frame {
    pub cells: [u8; CELLS],
    pub hold_ms: u16,
}

/// A named sequence of frames sharing one palette.
pub struct Animation {
    pub name: &'static str,
    /// RGB triples indexed by a frame's cell values.
    pub palette: [[u8; 3]; PALETTE_LEN],
    pub frames: &'static [Frame],
}

/// Walks an animation's frames, one at a time.
///
/// The player holds no timer of its own: the caller waits for [`Player::hold`]
/// and then calls [`Player::advance`], which keeps the animation on the same
/// event loop as everything else rather than on a private tick.
pub struct Player {
    animation: &'static Animation,
    frame: usize,
}

impl Player {
    pub fn new(animation: &'static Animation) -> Self {
        Self {
            animation,
            frame: 0,
        }
    }

    pub fn animation(&self) -> &'static Animation {
        self.animation
    }

    pub fn frame(&self) -> &'static Frame {
        &self.animation.frames[self.frame]
    }

    /// How long the current frame should stay on screen.
    pub fn hold(&self) -> Duration {
        Duration::from_millis(u64::from(self.frame().hold_ms))
    }

    /// Step to the next frame, wrapping at the end.
    pub fn advance(&mut self) {
        self.frame = (self.frame + 1) % self.animation.frames.len();
    }

    /// Switch to another animation, restarting from its first frame.
    pub fn set_animation(&mut self, animation: &'static Animation) {
        self.animation = animation;
        self.frame = 0;
    }
}
