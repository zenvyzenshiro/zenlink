//! Native video for ZenLink.
//!
//! Windows:
//! stream.bin -> Annex B -> Media Foundation decoder (DXVA when the GPU has it)
//! -> NV12 texture ring -> D3D11 video processor (NV12 -> BGRA, scale, rotate)
//! -> flip-model swapchain on a child HWND.
//!
//! Linux:
//! stream.bin -> Annex B -> FFmpeg decoder (VA-API / multithreaded)
//! -> BGRA frame ring -> Slint & snapshot presentation.

#[cfg(windows)]
mod d3d;
#[cfg(windows)]
mod decoder;
#[cfg(windows)]
pub mod glass;
#[cfg(windows)]
mod view;

#[cfg(target_os = "linux")]
mod linux_decoder;
#[cfg(target_os = "linux")]
mod linux_view;

mod stats;

pub use stats::{Stats, StatsSnapshot};

#[cfg(windows)]
pub use decoder::{decoder_available, media_foundation_present};
#[cfg(windows)]
pub use view::{Fit, PointerEvent, Snapshot, VideoView};

#[cfg(target_os = "linux")]
pub use linux_decoder::{decoder_available, media_foundation_present};
#[cfg(target_os = "linux")]
pub use linux_view::{Fit, PointerEvent, Snapshot, VideoView};
