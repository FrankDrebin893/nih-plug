use super::Plugin;
use crate::prelude::Vst3SubCategory;

/// Provides auxiliary metadata needed for a VST3 plugin.
pub trait Vst3Plugin: Plugin {
    /// The unique class ID that identifies this particular plugin. You can use the
    /// `*b"fooofooofooofooo"` syntax for this.
    ///
    /// This will be shuffled into a different byte order on Windows for project-compatibility.
    const VST3_CLASS_ID: [u8; 16];
    /// One or more subcategories. The host may use these to categorize the plugin. Internally this
    /// slice will be converted to a string where each character is separated by a pipe character
    /// (`|`). This string has a limit of 127 characters, and anything longer than that will be
    /// truncated.
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory];

    /// Whether `IAudioProcessor::getTailSamples()` always reports an infinite tail, instead of
    /// the tail implied by the last [`ProcessStatus`][crate::prelude::ProcessStatus]. Hosts may
    /// read the tail length only once when the plugin is activated, before the first `process()`
    /// call, at which point the last status is `Normal` and the tail would be reported as 0. A
    /// plugin with tails of unknown length (delay feedback, reverb) can set this so such hosts
    /// keep processing it after its input has stopped.
    const VST3_INFINITE_TAIL: bool = false;

    /// [`VST3_CLASS_ID`][Self::VST3_CLASS_ID`] in the correct order for the current platform so
    /// projects and presets can be shared between platforms. This should not be overridden.
    const PLATFORM_VST3_CLASS_ID: [u8; 16] = swap_vst3_uid_byte_order(Self::VST3_CLASS_ID);
}

#[cfg(not(target_os = "windows"))]
const fn swap_vst3_uid_byte_order(uid: [u8; 16]) -> [u8; 16] {
    uid
}

#[cfg(target_os = "windows")]
const fn swap_vst3_uid_byte_order(mut uid: [u8; 16]) -> [u8; 16] {
    // No mutable references in const functions, so we can't use `uid.swap()`
    let original_uid = uid;

    uid[0] = original_uid[3];
    uid[1] = original_uid[2];
    uid[2] = original_uid[1];
    uid[3] = original_uid[0];

    uid[4] = original_uid[5];
    uid[5] = original_uid[4];
    uid[6] = original_uid[7];
    uid[7] = original_uid[6];

    uid
}
