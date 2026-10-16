pub mod decoder;
/// Phones: stereo to mono without cancelling out-of-phase sound.
pub(crate) mod downmix;
pub mod input;
/// Phones: stream audio through the voice output on the call path.
pub(crate) mod mix;
pub mod output;
/// Reading a stream's ring at a steady pace (the mix and the own output).
pub(crate) mod pace;
/// Windows process-exclusive loopback backend for [`input::SystemAudioCapture`].
#[cfg(target_os = "windows")]
mod process_loopback;

pub use decoder::{
    OutputSelection, StreamAudioPacket, StreamAudioSender, StreamAudioStats, StreamDecoder,
};
