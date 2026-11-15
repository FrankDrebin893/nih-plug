use super::config::WrapperConfig;
use crate::prelude::{AuxiliaryBuffers, PluginNoteEvent, Transport};

mod cpal;
mod dummy;
mod jack;

pub use self::cpal::CpalMidir;
pub use self::dummy::Dummy;
pub use self::jack::Jack;
pub use crate::buffer::Buffer;
pub use crate::plugin::Plugin;

/// An audio+MIDI backend for the standalone wrapper.
pub trait Backend<P: Plugin>: 'static + Send + Sync {
    /// Brings the configuration the wrapper was started with in line with what this backend
    /// opened: a sample rate taken from the device and a period size brought into its range. The
    /// wrapper tells the plugin these values, so they have to be the stream's own.
    fn resolve_config(&self, _config: &mut WrapperConfig) {}

    /// Start processing audio and MIDI on this thread. The process callback will be called whenever
    /// there's a new block of audio to be processed. The process callback receives the audio
    /// buffers for the wrapped plugin's outputs. Any inputs will have already been copied to this
    /// buffer. This will block until the process callback returns `false`.
    fn run(
        &mut self,
        cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    );
}
