//! ECS glue for spatial audio: an [`AudioEmitter`] component, an
//! [`AudioListener`] marker, and [`audio_spatial_system`], a
//! [`Schedule`](crate::schedule::Schedule) system that starts emitters, moves
//! them with their [`Transform`], and moves the listener with a camera.
//!
//! ```ignore
//! // resources: the engine's audio over any backend (AudioContext natively)
//! resources.insert(AudioContext::new()?);
//! schedule.add_system(AUDIO_SPATIAL_SYSTEM, audio_spatial_system::<PlatformBackend>);
//!
//! world.spawn((Transform::from_position(Vec3::new(4.0, 0.0, 0.0)),
//!              AudioEmitter::looped((samples, 44_100)).on_bus("sfx")));
//! world.spawn((Transform::default(), Camera::default())); // listener
//! // each frame: schedule.run(..); resources.get_mut::<AudioContext>().unwrap().update();
//! ```
//!
//! The system does not call `update()`/`advance()` on the engine (that stays
//! the host's once-a-frame call, so fades keep their time source). It is a
//! silent no-op when no `AudioEngine<B>` resource exists (no audio device).

use glam::Vec3;

use super::backend::{AudioBackend, LoopBuffer};
use super::engine::AudioEngine;
use super::spatial::{Emitter, Listener};
use super::voices::SpatialHandle;
use crate::ecs::{Camera, Entity, Transform};
use crate::schedule::SystemCtx;

/// Name [`audio_spatial_system`] should be registered under.
pub const AUDIO_SPATIAL_SYSTEM: &str = "audio_spatial";

/// Marks the entity (with a [`Transform`]) whose position and facing the
/// listener follows. If none exists the first entity with a [`Camera`] and a
/// `Transform` is used; if there is neither, the listener is left alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioListener;

/// A sound attached to an entity. Needs a [`Transform`] on the same entity.
///
/// `emitter.position` is ignored: the system overwrites it with the entity's
/// position every frame. Every other `emitter` field (distances, rolloff,
/// cone, doppler factor, ...) is yours.
#[derive(Clone, Debug)]
pub struct AudioEmitter {
    pub source: LoopBuffer,
    /// Loop on the shared loop clock (`true`) or play once (`false`).
    pub looped: bool,
    /// Distance model and the rest of the emitter parameters.
    pub emitter: Emitter,
    /// Mixer bus to route the voice through (`None`: `sfx`, where spatial voices start).
    pub bus: Option<String>,
    /// Set false to silence a looping emitter; set true to start it. A
    /// one-shot sets it false itself when it finishes.
    pub playing: bool,
    /// Feed the entity's measured velocity to the emitter for doppler.
    pub track_velocity: bool,
    /// The running voice, once the system has started it.
    pub handle: Option<SpatialHandle>,
    last_position: Option<Vec3>,
}

impl AudioEmitter {
    pub fn new(source: impl Into<LoopBuffer>, looped: bool) -> Self {
        Self {
            source: source.into(),
            looped,
            emitter: Emitter::default(),
            bus: None,
            playing: true,
            track_velocity: true,
            handle: None,
            last_position: None,
        }
    }

    /// A looping emitter, playing from the first frame.
    pub fn looped(source: impl Into<LoopBuffer>) -> Self {
        Self::new(source, true)
    }

    /// A one-shot emitter, playing from the first frame.
    pub fn once(source: impl Into<LoopBuffer>) -> Self {
        Self::new(source, false)
    }

    pub fn on_bus(mut self, bus: &str) -> Self {
        self.bus = Some(bus.to_string());
        self
    }

    pub fn with_emitter(mut self, emitter: Emitter) -> Self {
        self.emitter = emitter;
        self
    }

    /// Whether a voice is currently running for this component.
    pub fn is_playing(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_alive())
    }
}

/// Remembers the previous listener position to derive its velocity.
#[derive(Default)]
struct ListenerTracker {
    last: Option<Vec3>,
}

/// Velocity from a position change over `dt`; zero for a first sample, a
/// non-positive `dt`, or anything non-finite.
fn velocity(last: Option<Vec3>, now: Vec3, dt: f32) -> Vec3 {
    match last {
        Some(prev) if dt > 0.0 && dt.is_finite() => {
            let v = (now - prev) / dt;
            if v.is_finite() {
                v
            } else {
                Vec3::ZERO
            }
        }
        _ => Vec3::ZERO,
    }
}

/// Schedule system for backend `B`: register as
/// `schedule.add_system(AUDIO_SPATIAL_SYSTEM, audio_spatial_system::<B>)`.
///
/// Per run: (1) every [`AudioEmitter`] with a [`Transform`] is started if it
/// should be playing, then its voice follows the entity (position and, when
/// `track_velocity`, measured velocity); a finished or stolen voice clears
/// `playing`; (2) the engine listener is set from the [`AudioListener`] (or
/// camera) entity's `Transform`, with measured velocity. Looping voices are
/// stopped when their entity (and so its handle) is despawned.
pub fn audio_spatial_system<B: AudioBackend + 'static>(ctx: &mut SystemCtx<'_>) {
    let dt = ctx.dt;
    let SystemCtx { world, resources, .. } = ctx;
    let Some(audio) = resources.get_mut::<AudioEngine<B>>() else { return };

    for (entity, (transform, em)) in world.query_mut::<(&Transform, &mut AudioEmitter)>() {
        let pos = transform.position;
        if em.handle.as_ref().is_some_and(|h| !h.is_alive()) {
            // Finished, stopped or stolen: do not restart (a one-shot would
            // loop forever; a stolen voice would thrash a full pool).
            em.handle = None;
            em.playing = false;
        }
        if !em.playing {
            if let Some(h) = em.handle.take() {
                h.stop();
            }
            em.last_position = Some(pos);
            continue;
        }
        let vel = em.track_velocity.then(|| velocity(em.last_position, pos, dt));
        em.last_position = Some(pos);
        match &em.handle {
            Some(h) => {
                h.set_position(pos);
                h.set_velocity(vel);
            }
            None => {
                let params = Emitter { velocity: vel, ..em.emitter };
                let handle = if em.looped {
                    let h = audio.add_spatial_track(&track_name(entity), em.source.clone(), Emitter { position: pos, ..params });
                    h.stop_when_dropped(true);
                    h
                } else {
                    audio.play_at(em.source.clone(), pos, params)
                };
                if let Some(bus) = &em.bus {
                    audio.assign_track_bus(handle.name(), bus);
                }
                em.handle = Some(handle);
            }
        }
    }

    let found = world
        .query::<(&Transform, &AudioListener)>()
        .iter()
        .next()
        .map(|(_, (t, _))| *t)
        .or_else(|| world.query::<(&Transform, &Camera)>().iter().next().map(|(_, (t, _))| *t));
    if let Some(t) = found {
        let tracker = resources.get_or_insert_with(ListenerTracker::default);
        let v = velocity(tracker.last, t.position, dt);
        tracker.last = Some(t.position);
        let listener = Listener { velocity: v, ..Listener::from_rotation(t.position, t.rotation) };
        if let Some(audio) = resources.get_mut::<AudioEngine<B>>() {
            audio.set_listener(listener);
        }
    }
}

fn track_name(entity: Entity) -> String {
    format!("@emitter:{}", entity.to_bits())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::backend::NullBackend;
    use crate::audio::spatial::Rolloff;
    use crate::ecs::World;
    use crate::resources::Resources;
    use crate::schedule::Schedule;

    type Engine = AudioEngine<NullBackend>;

    fn clip(frames: usize) -> (Vec<f32>, u32) {
        (vec![0.1; frames], 1000)
    }

    fn linear() -> Emitter {
        Emitter::default().with_distances(1.0, 11.0).with_rolloff(Rolloff::Linear, 1.0)
    }

    struct Rig {
        schedule: Schedule,
        world: World,
        res: Resources,
    }

    impl Rig {
        fn new() -> Self {
            let mut schedule = Schedule::new();
            schedule.add_system(AUDIO_SPATIAL_SYSTEM, audio_spatial_system::<NullBackend>);
            schedule.build().unwrap();
            let mut res = Resources::new();
            res.insert(Engine::with_manual_time(NullBackend::new()));
            Self { schedule, world: World::new(), res }
        }

        fn step(&mut self, dt: f32) {
            self.schedule.run(&mut self.world, &mut self.res, dt).unwrap();
            self.audio().advance(dt as f64);
        }

        fn audio(&mut self) -> &mut Engine {
            self.res.get_mut::<Engine>().unwrap()
        }
    }

    #[test]
    fn emitter_follows_transform_and_listener_follows_camera() {
        let mut r = Rig::new();
        let e = r.world.spawn((Transform::from_position(Vec3::new(6.0, 0.0, 0.0)), AudioEmitter::looped(clip(1000)).with_emitter(linear())));
        r.world.spawn((Transform::default(), Camera::default()));
        r.step(0.1);

        let h = r.world.get::<&AudioEmitter>(e).unwrap().handle.clone().expect("voice started");
        assert!(h.is_alive());
        let id = h.track_id();
        // distance 6 on a 1..11 linear model: gain 0.5; listener faces -Z so +X is hard right
        assert!((r.audio().backend().volume(id).unwrap() - 0.5).abs() < 1e-4);
        let (l, rr) = r.audio().backend().pan(id).unwrap();
        assert!(rr > l, "source on the right pans right: {l} {rr}");

        // move the emitter closer: gain rises
        r.world.get::<&mut Transform>(e).unwrap().position = Vec3::new(2.0, 0.0, 0.0);
        r.step(0.1);
        assert!((r.audio().backend().volume(id).unwrap() - 0.9).abs() < 1e-4);

        // move the camera past it and turn around: now it is on the left
        let cam = r.world.query::<&Camera>().iter().next().map(|(e, _)| e).unwrap();
        {
            let mut t = r.world.get::<&mut Transform>(cam).unwrap();
            t.rotation = glam::Quat::from_rotation_y(std::f32::consts::PI);
        }
        r.step(0.1);
        let (l, rr) = r.audio().backend().pan(id).unwrap();
        assert!(l > rr, "after turning around the source is on the left: {l} {rr}");
    }

    #[test]
    fn audio_listener_marker_wins_over_a_camera() {
        let mut r = Rig::new();
        r.world.spawn((Transform::from_position(Vec3::new(100.0, 0.0, 0.0)), Camera::default()));
        r.world.spawn((Transform::from_position(Vec3::new(5.0, 0.0, 0.0)), AudioListener));
        r.step(0.1);
        assert_eq!(r.audio().listener().position, Vec3::new(5.0, 0.0, 0.0));
    }

    #[test]
    fn listener_velocity_is_measured_and_no_listener_leaves_it_alone() {
        let mut r = Rig::new();
        r.step(0.1); // no listener entity, no emitters: harmless
        assert_eq!(*r.audio().listener(), Listener::default());
        let cam = r.world.spawn((Transform::default(), Camera::default()));
        r.step(0.1);
        r.world.get::<&mut Transform>(cam).unwrap().position = Vec3::new(1.0, 0.0, 0.0);
        r.step(0.1);
        assert!((r.audio().listener().velocity.x - 10.0).abs() < 1e-3);
    }

    #[test]
    fn bus_assignment_and_effective_gain() {
        let mut r = Rig::new();
        r.audio().set_bus_gain("sfx", 0.5);
        let e = r.world.spawn((Transform::from_position(Vec3::new(6.0, 0.0, 0.0)), AudioEmitter::looped(clip(1000)).with_emitter(linear()).on_bus("sfx")));
        r.step(0.1);
        let h = r.world.get::<&AudioEmitter>(e).unwrap().handle.clone().unwrap();
        assert_eq!(r.audio().track_bus(h.name()), Some("sfx"));
        // distance gain 0.5 x bus 0.5
        assert!((r.audio().backend().volume(h.track_id()).unwrap() - 0.25).abs() < 1e-4);
    }

    #[test]
    fn one_shot_plays_once_and_is_not_restarted() {
        let mut r = Rig::new();
        let e = r.world.spawn((Transform::default(), AudioEmitter::once(clip(300))));
        r.step(0.1);
        assert!(r.world.get::<&AudioEmitter>(e).unwrap().is_playing());
        for _ in 0..5 {
            r.step(0.1);
        }
        let em = r.world.get::<&AudioEmitter>(e).unwrap();
        assert!(!em.playing && em.handle.is_none(), "finished one-shot clears itself");
        drop(em);
        let starts_before = r.audio().backend().events.iter().filter(|e| matches!(e, crate::audio::NullEvent::Start { .. })).count();
        r.step(0.1);
        r.step(0.1);
        let starts_after = r.audio().backend().events.iter().filter(|e| matches!(e, crate::audio::NullEvent::Start { .. })).count();
        assert_eq!(starts_before, starts_after);
        assert_eq!(starts_after, 1);
    }

    #[test]
    fn playing_flag_stops_and_restarts_a_loop() {
        let mut r = Rig::new();
        let e = r.world.spawn((Transform::default(), AudioEmitter::looped(clip(1000))));
        r.step(0.1);
        let id = r.world.get::<&AudioEmitter>(e).unwrap().handle.as_ref().unwrap().track_id();
        assert!(r.audio().backend().volume(id).is_some());
        r.world.get::<&mut AudioEmitter>(e).unwrap().playing = false;
        r.step(0.1);
        assert!(r.audio().backend().volume(id).is_none(), "backend track stopped");
        r.world.get::<&mut AudioEmitter>(e).unwrap().playing = true;
        r.step(0.1);
        assert!(r.world.get::<&AudioEmitter>(e).unwrap().is_playing());
    }

    #[test]
    fn despawning_the_entity_stops_its_loop() {
        let mut r = Rig::new();
        let e = r.world.spawn((Transform::default(), AudioEmitter::looped(clip(1000))));
        r.step(0.1);
        let id = r.world.get::<&AudioEmitter>(e).unwrap().handle.as_ref().unwrap().track_id();
        r.world.despawn(e).unwrap();
        r.step(0.1);
        r.step(0.1);
        assert!(r.audio().backend().volume(id).is_none());
        assert_eq!(r.audio().spatial_voice_count(), 0);
    }

    #[test]
    fn missing_audio_resource_is_a_silent_noop() {
        let mut schedule = Schedule::new();
        schedule.add_system(AUDIO_SPATIAL_SYSTEM, audio_spatial_system::<NullBackend>);
        let mut world = World::new();
        world.spawn((Transform::default(), AudioEmitter::looped(clip(100))));
        world.spawn((Transform::default(), Camera::default()));
        let mut res = Resources::new();
        schedule.run(&mut world, &mut res, 0.1).unwrap();
    }

    #[test]
    fn works_through_headless() {
        let mut h = crate::headless::Headless::new();
        h.resources.insert(Engine::with_manual_time(NullBackend::new()));
        h.add_system(AUDIO_SPATIAL_SYSTEM, audio_spatial_system::<NullBackend>);
        let e = h.world.spawn((Transform::from_position(Vec3::new(3.0, 0.0, 0.0)), AudioEmitter::looped(clip(1000))));
        h.world.spawn((Transform::default(), Camera::default()));
        h.step(1.0 / 60.0);
        assert!(h.world.get::<&AudioEmitter>(e).unwrap().is_playing());
    }
}
