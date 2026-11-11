//! Skeletal (skinned) animation.
//!
//! Layers, bottom up — all but the last two are pure data/math with no GL:
//!
//! * [`pose`]: `JointTransform`, `Pose`, shortest-arc blending, weighted and
//!   additive layering.
//! * [`skeleton`]: joint hierarchy, bind pose, inverse-bind matrices and the
//!   joint-palette builder.
//! * [`clip`]: keyframed `AnimationClip`s with linear/step sampling,
//!   looping and root-motion extraction.
//! * [`animator`]: `AnimatorController` (states, parameters, transitions) and
//!   the `SkeletalAnimator` ECS component plus `skeletal_animation_system`.
//! * [`skin`]: per-vertex joint indices/weights and CPU skinning; the
//!   `SkinnedMesh` GL wrapper streams skinned vertices into a dynamic VBO.
//! * [`import`]: glTF skin/animation import and a RON asset format.
//!
//! This is separate from the older `crate::rig` (rigid segmented parts) and
//! `crate::animation` (procedural orbit/bob), which are unchanged.

pub mod animator;
pub mod clip;
pub mod import;
pub mod pose;
pub mod skeleton;
pub mod skin;

pub use animator::{
    skeletal_animation_system, AnimationSet, AnimatorController, AnimatorError, BlendPoint, Compare, Condition,
    Motion, ParamDef, ParamValue, SkeletalAnimator, StateDef, TransitionDef, SKELETAL_ANIMATION_SYSTEM,
};
pub use clip::{
    AnimationClip, ClipError, Interpolation, JointChannel, RootMotion, RootMotionDelta, Track,
};
pub use pose::{normalize_weights, slerp_shortest, JointTransform, Pose};
pub use skeleton::{Joint, Skeleton, SkeletonError};
pub use skin::{SkinVertex, SkinnedMesh, SkinnedMeshData};
