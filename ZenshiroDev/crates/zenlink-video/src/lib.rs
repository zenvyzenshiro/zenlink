//! Native video for ZenLink (ADR-0001, Stage 1).
//!
//! stream.bin -> Annex B -> Media Foundation decoder (DXVA when the GPU has it)
//! -> NV12 texture ring -> D3D11 video processor (NV12 -> BGRA, scale, rotate)
//! -> flip-model swapchain on a child HWND.
//!
//! The render thread waits on the swapchain's frame-latency object and
//! presents with sync interval 1: exactly one present per display refresh,
//! always the newest decoded frame. DWM (and so OBS Window / Display
//! Capture) sees an evenly paced window instead of bursts.

mod d3d;
mod decoder;
mod stats;
mod view;

pub use decoder::{decoder_available, media_foundation_present};
pub use stats::{Stats, StatsSnapshot};
pub use view::{Fit, PointerEvent, Snapshot, VideoView};
