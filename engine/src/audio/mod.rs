//! Audio playback with named tracks (buses).
//!
//! * `tracks` – pure logic: track registry, gain fading, the shared loop
//!   clock. No device needed; unit-tested directly.
//! * `backend` – the `AudioBackend` trait, plus `NullBackend` for tests.
//! * `engine` – `AudioEngine<B>`: tracks + clock + master volumes over any
//!   backend.
//! * `native` (rodio) / `web` (SDL2 mixer, Emscripten) – the two backends.
//!
//! `AudioContext` is the engine over the platform backend, so games use the
//! same API on both targets:
//!
//! ```ignore
//! audio.add_track("drums", (samples, 44_100)).set_gain(0.5);
//! audio.fade_track("drums", 1.0, 2.0); // then call audio.update() each frame
//! ```

mod backend;
mod engine;
pub mod mixer;
pub mod spatial;
#[cfg(not(target_os = "emscripten"))]
mod native;
mod tracks;
#[cfg(target_os = "emscripten")]
mod web;

pub use backend::{AudioBackend, LoopBuffer, NullBackend, NullEvent};
pub use engine::{AudioEngine, TrackHandle, LAYER_TRACK, MUSIC_TRACK};
pub use mixer::{Bus, BusSettings, Mixer, MixerError, MixerSettings, MixerSettingsError, MASTER_BUS, MUSIC_BUS, SFX_BUS, UI_BUS};
pub use tracks::{GainFader, LoopClock, Track, TrackBank, TrackId};

#[cfg(not(target_os = "emscripten"))]
pub use native::NativeBackend as PlatformBackend;
#[cfg(target_os = "emscripten")]
pub use web::WebBackend as PlatformBackend;

/// The engine's audio over this platform's backend (rodio natively, the SDL2
/// mixer on Emscripten).
pub type AudioContext = AudioEngine<PlatformBackend>;

impl AudioContext {
    /// Opens the default audio device; `Err` if there is none (callers keep
    /// an `Option<AudioContext>` and carry on silently).
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::with_backend(PlatformBackend::open()?))
    }

    /// The rodio handle, for games that build their own sources.
    #[cfg(not(target_os = "emscripten"))]
    pub fn handle(&self) -> &rodio::OutputStreamHandle {
        self.backend().handle()
    }
}
