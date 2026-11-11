//! Effect primitives: short-lived entities and temporary texture swaps.
//!
//! Games used to track `(Entity, born_at)` pairs and `(Entity, Arc<GpuTexture>)`
//! pairs in `Vec`s on their own structs and despawn/restore by hand. These
//! components move that bookkeeping into the ECS:
//!
//! * [`Lifetime`] — age / max age; the entity is despawned when it expires.
//! * [`Fade`] — scales and/or fades an entity over its [`Lifetime`].
//! * [`TextureOverride`] — swaps a [`MeshRenderer`]'s texture and remembers the
//!   original so it can be restored (optionally after a timer).
//!
//! Call [`tick`] once per frame. It advances everything, then despawns expired
//! entities. The individual steps ([`tick_lifetimes`], [`apply_fades`],
//! [`tick_texture_overrides`]) are public for games that need a custom order.
//!
//! Despawning an entity drops its `MeshRenderer` and `TextureOverride` together,
//! so the saved original texture is released with it and nothing is left
//! behind in a side table.

use std::sync::Arc;

use glam::Vec3;
use hecs::{Entity, World};

use crate::ecs::{MeshRenderer, Transform};
use crate::texture::GpuTexture;

// ---------------------------------------------------------------- Lifetime

/// Age and maximum age of a temporary entity, in seconds.
///
/// The entity expires when `age >= max`, so a `max` of `0.0` expires on the
/// first [`tick`] (even with `dt == 0`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lifetime {
    pub age: f32,
    pub max: f32,
}

impl Lifetime {
    pub fn new(max: f32) -> Self {
        Self { age: 0.0, max }
    }

    /// True once `age` has reached `max`.
    pub fn expired(&self) -> bool {
        self.age >= self.max
    }

    /// Progress through the lifetime, clamped to `0.0..=1.0`. A non-positive
    /// `max` counts as fully elapsed.
    pub fn fraction(&self) -> f32 {
        if self.max <= 0.0 {
            1.0
        } else {
            (self.age / self.max).clamp(0.0, 1.0)
        }
    }

    /// Seconds left, never negative.
    pub fn remaining(&self) -> f32 {
        (self.max - self.age).max(0.0)
    }
}

// -------------------------------------------------------------------- Fade

/// Shape of a [`Fade`] curve over normalised life `t` in `0..=1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FadeCurve {
    #[default]
    Linear,
    /// Fast at first, slowing toward the end: `1 - (1 - t)^2`.
    EaseOut,
}

impl FadeCurve {
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear => t,
            FadeCurve::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
        }
    }
}

/// Scale and/or alpha animated over the entity's [`Lifetime`].
///
/// `scale` is a multiplier `(from, to)` applied to the entity's
/// `Transform::scale` as it was on the first [`apply_fades`] (remembered in
/// the component, so repeated ticks don't compound). `alpha` is `(from, to)`
/// and is exposed through [`Fade::alpha`] for the renderer/game to read;
/// `MeshRenderer` has no alpha channel of its own yet.
///
/// A `Fade` on an entity without a `Lifetime` does nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fade {
    pub scale: Option<(f32, f32)>,
    pub alpha: Option<(f32, f32)>,
    pub curve: FadeCurve,
    base_scale: Option<Vec3>,
    current_alpha: f32,
}

impl Fade {
    /// A fade that does nothing until `with_scale` / `with_alpha` is added.
    pub fn new() -> Self {
        Self {
            scale: None,
            alpha: None,
            curve: FadeCurve::Linear,
            base_scale: None,
            current_alpha: 1.0,
        }
    }

    /// Shrink from full size to nothing.
    pub fn shrink() -> Self {
        Self::new().with_scale(1.0, 0.0)
    }

    /// Alpha 1 → 0.
    pub fn fade_out() -> Self {
        Self::new().with_alpha(1.0, 0.0)
    }

    pub fn with_scale(mut self, from: f32, to: f32) -> Self {
        self.scale = Some((from, to));
        self
    }

    pub fn with_alpha(mut self, from: f32, to: f32) -> Self {
        self.alpha = Some((from, to));
        self.current_alpha = from;
        self
    }

    pub fn with_curve(mut self, curve: FadeCurve) -> Self {
        self.curve = curve;
        self
    }

    /// Scale multiplier at normalised life `t` (1.0 if no scale curve).
    pub fn scale_at(&self, t: f32) -> f32 {
        match self.scale {
            Some((a, b)) => a + (b - a) * self.curve.apply(t),
            None => 1.0,
        }
    }

    /// Alpha at normalised life `t` (1.0 if no alpha curve).
    pub fn alpha_at(&self, t: f32) -> f32 {
        match self.alpha {
            Some((a, b)) => a + (b - a) * self.curve.apply(t),
            None => 1.0,
        }
    }

    /// Alpha as of the last [`apply_fades`] / [`tick`].
    pub fn alpha(&self) -> f32 {
        self.current_alpha
    }
}

impl Default for Fade {
    fn default() -> Self {
        Self::new()
    }
}

// --------------------------------------------------------- TextureOverride

/// Marks a [`MeshRenderer`] whose texture has been swapped, and remembers the
/// texture to put back. Create it with [`apply_texture_override`]; remove it
/// with [`restore_texture`] (removing the bare component with
/// `World::remove_one` would leave the swapped texture in place).
#[derive(Clone)]
pub struct TextureOverride {
    /// The texture the renderer had before the first override (`None` if it
    /// had no texture).
    pub original: Option<Arc<GpuTexture>>,
    /// Seconds until [`tick`] restores the original automatically; `None`
    /// keeps the override until [`restore_texture`] is called.
    pub remaining: Option<f32>,
}

/// Slot-level swap shared by the world functions (and testable without a GL
/// mesh). Returns the override to insert when `existing` is `None`.
fn apply_to_slot(
    slot: &mut Option<Arc<GpuTexture>>,
    existing: Option<&mut TextureOverride>,
    texture: Option<Arc<GpuTexture>>,
    duration: Option<f32>,
) -> Option<TextureOverride> {
    match existing {
        // Already overridden: the saved original stays the ORIGINAL; only the
        // visible texture and the timer change.
        Some(ov) => {
            *slot = texture;
            ov.remaining = duration;
            None
        }
        None => {
            let original = std::mem::replace(slot, texture);
            Some(TextureOverride { original, remaining: duration })
        }
    }
}

/// Puts the saved original back into `slot`.
fn restore_slot(slot: &mut Option<Arc<GpuTexture>>, ov: TextureOverride) {
    *slot = ov.original;
}

/// Advances the timer; returns true when it has run out and the original
/// should be restored.
fn tick_override(ov: &mut TextureOverride, dt: f32) -> bool {
    match &mut ov.remaining {
        Some(r) => {
            *r -= dt.max(0.0);
            *r <= 0.0
        }
        None => false,
    }
}

/// Swaps `entity`'s `MeshRenderer` texture to `texture` and records the
/// original in a [`TextureOverride`]. If the entity is already overridden the
/// texture (and timer) are replaced but the first original is kept, so
/// applying twice and restoring once returns the true original.
///
/// `duration` is the number of seconds until [`tick`] restores it (`None` =
/// until [`restore_texture`]). Returns false if the entity doesn't exist or
/// has no `MeshRenderer`.
pub fn apply_texture_override(
    world: &mut World,
    entity: Entity,
    texture: Option<Arc<GpuTexture>>,
    duration: Option<f32>,
) -> bool {
    let new_override = {
        let Ok(mut mr) = world.get::<&mut MeshRenderer>(entity) else {
            return false;
        };
        match world.get::<&mut TextureOverride>(entity) {
            Ok(mut ov) => apply_to_slot(&mut mr.texture, Some(&mut ov), texture, duration),
            Err(_) => apply_to_slot(&mut mr.texture, None, texture, duration),
        }
    };
    if let Some(ov) = new_override {
        let _ = world.insert_one(entity, ov);
    }
    true
}

/// Restores the original texture and removes the [`TextureOverride`].
/// Returns false if the entity wasn't overridden.
pub fn restore_texture(world: &mut World, entity: Entity) -> bool {
    let Ok(ov) = world.remove_one::<TextureOverride>(entity) else {
        return false;
    };
    if let Ok(mut mr) = world.get::<&mut MeshRenderer>(entity) {
        restore_slot(&mut mr.texture, ov);
    }
    true
}

/// Restores every overridden entity. Returns how many were restored.
pub fn restore_all_textures(world: &mut World) -> usize {
    let entities: Vec<Entity> = world
        .query::<&TextureOverride>()
        .iter()
        .map(|(e, _)| e)
        .collect();
    entities
        .into_iter()
        .filter(|&e| restore_texture(world, e))
        .count()
}

// ------------------------------------------------------------------ Ticks

/// Advances every [`Lifetime`] by `dt` (negative `dt` is treated as 0) and
/// returns the entities that have expired. Does not despawn them.
pub fn tick_lifetimes(world: &mut World, dt: f32) -> Vec<Entity> {
    let dt = dt.max(0.0);
    let mut expired = Vec::new();
    for (entity, life) in world.query::<&mut Lifetime>().iter() {
        life.age += dt;
        if life.expired() {
            expired.push(entity);
        }
    }
    expired
}

/// Applies each [`Fade`] at its entity's current [`Lifetime`] fraction.
pub fn apply_fades(world: &mut World) {
    for (_entity, (life, fade, transform)) in
        world.query::<(&Lifetime, &mut Fade, Option<&mut Transform>)>().iter()
    {
        let t = life.fraction();
        fade.current_alpha = fade.alpha_at(t);
        if fade.scale.is_some() {
            if let Some(transform) = transform {
                let base = *fade.base_scale.get_or_insert(transform.scale);
                transform.scale = base * fade.scale_at(t);
            }
        }
    }
}

/// Counts down timed [`TextureOverride`]s and restores those that ran out.
/// Returns the entities restored.
pub fn tick_texture_overrides(world: &mut World, dt: f32) -> Vec<Entity> {
    let mut due = Vec::new();
    for (entity, ov) in world.query::<&mut TextureOverride>().iter() {
        if tick_override(ov, dt) {
            due.push(entity);
        }
    }
    due.retain(|&e| restore_texture(world, e));
    due
}

/// One frame of effects: advance lifetimes, apply fades (so the final frame
/// shows the end value), tick texture overrides, then despawn expired
/// entities. Returns the despawned entities.
pub fn tick(world: &mut World, dt: f32) -> Vec<Entity> {
    let expired = tick_lifetimes(world, dt);
    apply_fades(world);
    tick_texture_overrides(world, dt);
    for &entity in &expired {
        let _ = world.despawn(entity);
    }
    expired
}

// ------------------------------------------------------------------ Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    /// A `GpuTexture` with a fake handle. `GpuTexture` has no `Drop`, so this
    /// never touches GL.
    fn fake_tex(id: u32) -> Arc<GpuTexture> {
        Arc::new(GpuTexture {
            handle: glow::NativeTexture(NonZeroU32::new(id).unwrap()),
            width: 1,
            height: 1,
        })
    }

    fn id(t: &Option<Arc<GpuTexture>>) -> Option<u32> {
        t.as_ref().map(|t| t.handle.0.get())
    }

    fn spawn_fx(world: &mut World, max: f32) -> Entity {
        world.spawn((Transform::default(), Lifetime::new(max)))
    }

    // ---- Lifetime

    #[test]
    fn lifetime_despawns_exactly_at_expiry() {
        let mut w = World::new();
        let e = spawn_fx(&mut w, 1.0);
        // Powers of two so the f32 sums are exact.
        assert!(tick(&mut w, 0.5).is_empty());
        assert!(w.contains(e));
        assert!(tick(&mut w, 0.375).is_empty()); // age 0.875 < 1.0
        assert!(w.contains(e));
        assert_eq!(tick(&mut w, 0.125), vec![e]); // age 1.0 == max
        assert!(!w.contains(e));
    }

    #[test]
    fn lifetime_age_equal_to_max_expires() {
        let mut w = World::new();
        let e = spawn_fx(&mut w, 0.5);
        assert!(tick(&mut w, 0.25).is_empty());
        assert_eq!(tick(&mut w, 0.25), vec![e]); // 0.5 exactly in f32
        assert!(!w.contains(e));
    }

    #[test]
    fn lifetime_fraction_and_remaining() {
        let mut l = Lifetime::new(2.0);
        assert_eq!(l.fraction(), 0.0);
        l.age = 0.5;
        assert_eq!(l.fraction(), 0.25);
        assert_eq!(l.remaining(), 1.5);
        l.age = 9.0;
        assert_eq!(l.fraction(), 1.0);
        assert_eq!(l.remaining(), 0.0);
        assert_eq!(Lifetime::new(0.0).fraction(), 1.0);
    }

    #[test]
    fn multiple_entities_only_expired_despawn() {
        let mut w = World::new();
        let short = spawn_fx(&mut w, 1.0);
        let long = spawn_fx(&mut w, 3.0);
        let immortal = w.spawn((Transform::default(),));
        let mid = spawn_fx(&mut w, 2.0);

        assert_eq!(tick(&mut w, 1.0), vec![short]);
        assert!(!w.contains(short));
        assert!(w.contains(long) && w.contains(mid) && w.contains(immortal));

        assert_eq!(tick(&mut w, 1.0), vec![mid]);
        assert!(w.contains(long) && w.contains(immortal));

        assert_eq!(tick(&mut w, 1.0), vec![long]);
        assert!(w.contains(immortal));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn dt_zero_changes_nothing() {
        let mut w = World::new();
        let e = w.spawn((
            Transform::default(),
            Lifetime::new(1.0),
            Fade::shrink().with_alpha(1.0, 0.0),
        ));
        for _ in 0..5 {
            assert!(tick(&mut w, 0.0).is_empty());
        }
        assert!(w.contains(e));
        assert_eq!(w.get::<&Lifetime>(e).unwrap().age, 0.0);
        assert_eq!(w.get::<&Transform>(e).unwrap().scale, Vec3::ONE);
        assert_eq!(w.get::<&Fade>(e).unwrap().alpha(), 1.0);
    }

    #[test]
    fn dt_zero_expires_only_zero_max() {
        let mut w = World::new();
        let zero = spawn_fx(&mut w, 0.0);
        let one = spawn_fx(&mut w, 1.0);
        assert_eq!(tick(&mut w, 0.0), vec![zero]);
        assert!(w.contains(one));
    }

    #[test]
    fn negative_dt_is_ignored() {
        let mut w = World::new();
        let e = spawn_fx(&mut w, 1.0);
        tick(&mut w, 0.5);
        tick(&mut w, -10.0);
        assert_eq!(w.get::<&Lifetime>(e).unwrap().age, 0.5);
    }

    // ---- Fade

    #[test]
    fn fade_scale_endpoints_linear() {
        let mut w = World::new();
        let e = w.spawn((
            Transform { scale: Vec3::splat(2.0), ..Default::default() },
            Lifetime::new(4.0),
            Fade::shrink(),
        ));
        let scale = |w: &World| w.get::<&Transform>(e).unwrap().scale;

        tick(&mut w, 0.0); // 0% of life
        assert_eq!(scale(&w), Vec3::splat(2.0));
        tick(&mut w, 2.0); // 50%
        assert_eq!(scale(&w), Vec3::splat(1.0));
        tick(&mut w, 1.0); // 75%: repeated ticks must not compound
        assert_eq!(scale(&w), Vec3::splat(0.5));
    }

    #[test]
    fn fade_reaches_end_value_on_final_tick() {
        let mut w = World::new();
        let e = w.spawn((
            Transform::default(),
            Lifetime::new(1.0),
            Fade::shrink().with_alpha(1.0, 0.0),
        ));
        // Use the step functions so the entity survives to be inspected.
        tick_lifetimes(&mut w, 1.0);
        apply_fades(&mut w);
        assert_eq!(w.get::<&Transform>(e).unwrap().scale, Vec3::ZERO);
        assert_eq!(w.get::<&Fade>(e).unwrap().alpha(), 0.0);
    }

    #[test]
    fn fade_alpha_endpoints_and_midpoint() {
        let mut w = World::new();
        let e = w.spawn((Lifetime::new(2.0), Fade::fade_out()));
        let alpha = |w: &World| w.get::<&Fade>(e).unwrap().alpha();
        assert_eq!(alpha(&w), 1.0);
        tick(&mut w, 1.0);
        assert_eq!(alpha(&w), 0.5);
        tick_lifetimes(&mut w, 1.0); // keep alive for inspection
        apply_fades(&mut w);
        assert_eq!(alpha(&w), 0.0);
    }

    #[test]
    fn fade_alpha_can_rise_and_scale_can_grow() {
        let f = Fade::new().with_scale(0.5, 2.0).with_alpha(0.0, 1.0);
        assert_eq!(f.scale_at(0.0), 0.5);
        assert_eq!(f.scale_at(1.0), 2.0);
        assert_eq!(f.alpha_at(0.0), 0.0);
        assert_eq!(f.alpha_at(1.0), 1.0);
        // Out-of-range t clamps.
        assert_eq!(f.alpha_at(-3.0), 0.0);
        assert_eq!(f.alpha_at(7.0), 1.0);
    }

    #[test]
    fn fade_ease_out_curve() {
        let f = Fade::shrink().with_curve(FadeCurve::EaseOut);
        assert_eq!(f.scale_at(0.0), 1.0);
        assert_eq!(f.scale_at(1.0), 0.0);
        // Ease-out at t=0.5 is 0.75 of the way: scale 1 - 0.75.
        assert!((f.scale_at(0.5) - 0.25).abs() < 1e-6);
        // ...which is ahead of linear.
        assert!(f.scale_at(0.5) < Fade::shrink().scale_at(0.5));
    }

    #[test]
    fn fade_without_lifetime_is_inert() {
        let mut w = World::new();
        let e = w.spawn((Transform::default(), Fade::shrink()));
        tick(&mut w, 10.0);
        assert!(w.contains(e));
        assert_eq!(w.get::<&Transform>(e).unwrap().scale, Vec3::ONE);
    }

    #[test]
    fn fade_alpha_only_leaves_scale_alone() {
        let mut w = World::new();
        let e = w.spawn((
            Transform { scale: Vec3::splat(3.0), ..Default::default() },
            Lifetime::new(1.0),
            Fade::fade_out(),
        ));
        tick(&mut w, 0.5);
        assert_eq!(w.get::<&Transform>(e).unwrap().scale, Vec3::splat(3.0));
    }

    #[test]
    fn fade_applies_to_many_entities_independently() {
        let mut w = World::new();
        let a = w.spawn((Transform::default(), Lifetime::new(1.0), Fade::shrink()));
        let b = w.spawn((Transform::default(), Lifetime::new(4.0), Fade::shrink()));
        tick(&mut w, 0.5);
        assert_eq!(w.get::<&Transform>(a).unwrap().scale, Vec3::splat(0.5));
        assert_eq!(w.get::<&Transform>(b).unwrap().scale, Vec3::splat(0.875));
    }

    // ---- TextureOverride (slot level: building a GpuMesh needs GL)

    #[test]
    fn override_apply_then_restore() {
        let mut slot = Some(fake_tex(1));
        let ov = apply_to_slot(&mut slot, None, Some(fake_tex(2)), None).unwrap();
        assert_eq!(id(&slot), Some(2));
        assert_eq!(id(&ov.original), Some(1));
        restore_slot(&mut slot, ov);
        assert_eq!(id(&slot), Some(1));
    }

    #[test]
    fn override_double_apply_keeps_original() {
        let mut slot = Some(fake_tex(1));
        let mut ov = apply_to_slot(&mut slot, None, Some(fake_tex(2)), None).unwrap();
        // Second apply with an existing override: no new component, original kept.
        assert!(apply_to_slot(&mut slot, Some(&mut ov), Some(fake_tex(3)), None).is_none());
        assert_eq!(id(&slot), Some(3));
        assert_eq!(id(&ov.original), Some(1), "original must not become texture 2");
        restore_slot(&mut slot, ov);
        assert_eq!(id(&slot), Some(1));
    }

    #[test]
    fn override_from_no_texture_restores_to_none() {
        let mut slot = None;
        let ov = apply_to_slot(&mut slot, None, Some(fake_tex(5)), None).unwrap();
        assert_eq!(id(&slot), Some(5));
        assert!(ov.original.is_none());
        restore_slot(&mut slot, ov);
        assert!(slot.is_none());
    }

    #[test]
    fn override_to_none_clears_texture_and_restores() {
        let mut slot = Some(fake_tex(1));
        let ov = apply_to_slot(&mut slot, None, None, None).unwrap();
        assert!(slot.is_none());
        restore_slot(&mut slot, ov);
        assert_eq!(id(&slot), Some(1));
    }

    #[test]
    fn override_timer_expires_and_dt_zero_is_noop() {
        let mut slot = Some(fake_tex(1));
        let mut ov = apply_to_slot(&mut slot, None, Some(fake_tex(2)), Some(1.0)).unwrap();
        assert!(!tick_override(&mut ov, 0.0));
        assert_eq!(ov.remaining, Some(1.0));
        assert!(!tick_override(&mut ov, 0.75));
        assert!(tick_override(&mut ov, 0.25));
        // No timer: never expires.
        let mut forever = TextureOverride { original: None, remaining: None };
        assert!(!tick_override(&mut forever, 1e9));
    }

    #[test]
    fn override_reapply_resets_timer_keeps_original() {
        let mut slot = Some(fake_tex(1));
        let mut ov = apply_to_slot(&mut slot, None, Some(fake_tex(2)), Some(1.0)).unwrap();
        tick_override(&mut ov, 0.9);
        apply_to_slot(&mut slot, Some(&mut ov), Some(fake_tex(2)), Some(1.0));
        assert_eq!(ov.remaining, Some(1.0));
        assert_eq!(id(&ov.original), Some(1));
    }

    #[test]
    fn override_original_released_on_drop() {
        // No leak: dropping the override (as happens on despawn) releases the
        // saved Arc.
        let original = fake_tex(1);
        let mut slot = Some(original.clone());
        let ov = apply_to_slot(&mut slot, None, Some(fake_tex(2)), None).unwrap();
        assert_eq!(Arc::strong_count(&original), 2);
        drop(ov);
        assert_eq!(Arc::strong_count(&original), 1);
    }

    #[test]
    fn world_functions_ignore_entities_without_mesh_renderer() {
        let mut w = World::new();
        let e = w.spawn((Transform::default(),));
        assert!(!apply_texture_override(&mut w, e, Some(fake_tex(2)), None));
        assert!(w.get::<&TextureOverride>(e).is_err());
        assert!(!restore_texture(&mut w, e));
        assert_eq!(restore_all_textures(&mut w), 0);
        assert!(tick_texture_overrides(&mut w, 1.0).is_empty());
    }
}
