//! Safe wrapper around the C++ microWakeWord detector (`ricardo/microwakeword.cpp`).
//!
//! The detector is fed the same 16 kHz, VQE-cleaned `VQE_OMNI_WITH_GAIN` stream
//! that the native Yandex spotter uses. It owns its streaming feature state and
//! resets it after a hit; callers only add the 0.5 s pre-roll.

use std::ffi::{CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::NonNull;

unsafe extern "C" {
    fn mww_create(model_path: *const c_char) -> *mut c_void;
    fn mww_destroy(handle: *mut c_void);
    fn mww_feed(handle: *mut c_void, pcm: *const i16, samples: usize, score: *mut f32) -> i32;
    fn mww_reset(handle: *mut c_void) -> i32;
}

/// The on-device "Рикардо" detector. Use only from the audio processing thread.
pub struct RicardoWake {
    raw: NonNull<c_void>,
}

impl RicardoWake {
    /// Loads the model and opens the firmware's `libtflite.so`.
    pub fn new(model: &Path) -> Result<Self, String> {
        let path = CString::new(model.as_os_str().as_bytes())
            .map_err(|_| format!("{}: NUL in path", model.display()))?;
        // SAFETY: the C++ function copies/reads the NUL-terminated path and
        // returns an opaque handle owned by this wrapper.
        let raw = unsafe { mww_create(path.as_ptr()) };
        let raw = NonNull::new(raw).ok_or_else(|| format!("cannot load {}", model.display()))?;
        Ok(Self { raw })
    }

    /// Feeds one VQE chunk. Ok((true, score)) on a detection, Ok((false, _))
    /// otherwise; Err after the detector disabled itself.
    pub fn feed(&mut self, pcm: &[i16]) -> Result<(bool, f32), String> {
        let mut score = -1.0f32;
        // SAFETY: `raw` is a live handle from mww_create; `pcm` is valid for
        // the call and `score` points to a local.
        let rc = unsafe { mww_feed(self.raw.as_ptr(), pcm.as_ptr(), pcm.len(), &mut score) };
        match rc {
            0 => Ok((false, score)),
            1 => Ok((true, score)),
            _ => Err("microWakeWord disabled itself".into()),
        }
    }

    /// Resets the streaming state (normally the detector does this after a hit).
    #[allow(dead_code, reason = "kept for diagnostics and future reconfiguration")]
    pub fn reset(&mut self) -> Result<(), String> {
        // SAFETY: live handle, no aliasing.
        if unsafe { mww_reset(self.raw.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err("microWakeWord reset failed".into())
        }
    }
}

// The detector is constructed on the main thread and moved once to the audio
// processing thread before it is fed; after that only that thread touches it.
// The C++ object has no thread-local state of its own.
//
// SAFETY: the handle is not shared: ownership moves to exactly one processing
// thread before any mww_feed call, and mww_destroy runs after that thread stops.
unsafe impl Send for RicardoWake {}

impl Drop for RicardoWake {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this wrapper and destroyed once.
        unsafe { mww_destroy(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_is_an_error() {
        // No libtflite.so or model on the build host: the C++ bridge must
        // report failure instead of returning a dangling handle.
        assert!(RicardoWake::new(Path::new("/nonexistent/ricardo.tflite")).is_err());
    }
}
