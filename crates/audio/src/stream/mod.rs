pub mod decoder;
pub mod input;
pub mod output;
/// Windows process-exclusive loopback backend for [`input::SystemAudioCapture`].
#[cfg(target_os = "windows")]
mod process_loopback;

pub use decoder::{StreamAudioPacket, StreamDecoder};
pub use output::{StreamAudioFrame, StreamAudioOutput};
