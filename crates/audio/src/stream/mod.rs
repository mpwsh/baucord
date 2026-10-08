pub mod decoder;
/// Phones: stereo to mono without cancelling out-of-phase sound.
pub(crate) mod downmix;
pub mod input;
/// Phones: stream audio through the voice output on the call path.
pub(crate) mod mix;
pub mod output;
/// Windows process-exclusive loopback backend for [`input::SystemAudioCapture`].
#[cfg(target_os = "windows")]
mod process_loopback;

pub use decoder::{StreamAudioPacket, StreamAudioStats, StreamDecoder};
pub use output::{StreamAudioFrame, StreamAudioOutput};
