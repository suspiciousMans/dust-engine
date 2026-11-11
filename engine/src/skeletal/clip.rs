//! Keyframed animation clips: per-joint translation / rotation / scale
//! tracks with linear / step interpolation (rotations slerp along the
//! shortest arc), looping, and optional root-motion extraction.

use glam::{Quat, Vec3};

use super::pose::{slerp_shortest, JointTransform, Pose};
use super::skeleton::Skeleton;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Interpolation {
    /// Lerp for vectors, slerp (shortest arc) for rotations.
    #[default]
    Linear,
    /// Hold the earlier key's value until the next key.
    Step,
}

/// Values that can be interpolated by a [`Track`].
pub trait Interpolate: Copy {
    fn interpolate(a: Self, b: Self, t: f32) -> Self;
}

impl Interpolate for Vec3 {
    fn interpolate(a: Self, b: Self, t: f32) -> Self {
        a.lerp(b, t)
    }
}

impl Interpolate for Quat {
    fn interpolate(a: Self, b: Self, t: f32) -> Self {
        slerp_shortest(a, b, t)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClipError {
    #[error("track has {times} times but {values} values")]
    LengthMismatch { times: usize, values: usize },
    #[error("track times must be strictly increasing (index {0})")]
    Unsorted(usize),
}

/// Keyframes for one channel. `times` is strictly increasing seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Track<T> {
    pub times: Vec<f32>,
    pub values: Vec<T>,
    pub interpolation: Interpolation,
}

impl<T: Interpolate> Track<T> {
    pub fn new(times: Vec<f32>, values: Vec<T>, interpolation: Interpolation) -> Result<Self, ClipError> {
        if times.len() != values.len() {
            return Err(ClipError::LengthMismatch { times: times.len(), values: values.len() });
        }
        if let Some(i) = times.windows(2).position(|w| w[1] <= w[0] || w[1].is_nan() || w[0].is_nan()) {
            return Err(ClipError::Unsorted(i + 1));
        }
        Ok(Self { times, values, interpolation })
    }

    /// Value at `t`, clamped to the first/last key outside the range.
    /// `None` for an empty track.
    pub fn sample(&self, t: f32) -> Option<T> {
        let n = self.times.len().min(self.values.len());
        if n == 0 {
            return None;
        }
        if t <= self.times[0] || n == 1 {
            return Some(self.values[0]);
        }
        if t >= self.times[n - 1] {
            return Some(self.values[n - 1]);
        }
        // first key strictly after t; `hi >= 1` because t > times[0].
        let hi = self.times[..n].partition_point(|&k| k <= t);
        let lo = hi - 1;
        Some(match self.interpolation {
            Interpolation::Step => self.values[lo],
            Interpolation::Linear => {
                let f = (t - self.times[lo]) / (self.times[hi] - self.times[lo]);
                T::interpolate(self.values[lo], self.values[hi], f)
            }
        })
    }

    pub fn end_time(&self) -> f32 {
        self.times.last().copied().unwrap_or(0.0)
    }
}

/// The tracks driving one joint. Absent tracks leave the bind value.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct JointChannel {
    pub joint: usize,
    pub translation: Option<Track<Vec3>>,
    pub rotation: Option<Track<Quat>>,
    pub scale: Option<Track<Vec3>>,
}

impl JointChannel {
    pub fn new(joint: usize) -> Self {
        Self { joint, ..Default::default() }
    }
}

/// Which part of one joint's motion is "root motion": taken out of the
/// sampled pose and reported as a per-update delta instead, so the game
/// can move the entity rather than sliding the mesh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RootMotion {
    pub joint: usize,
    /// Axes of the joint's translation to extract (`true` = extract).
    pub translation_axes: [bool; 3],
    /// Extract the joint's rotation too.
    pub rotation: bool,
}

impl RootMotion {
    /// Extract horizontal (X/Z) translation only.
    pub fn horizontal(joint: usize) -> Self {
        Self { joint, translation_axes: [true, false, true], rotation: false }
    }
}

/// Motion extracted from a clip over one time step, in the root joint's
/// parent space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RootMotionDelta {
    pub translation: Vec3,
    pub rotation: Quat,
}

impl RootMotionDelta {
    pub const ZERO: Self = Self { translation: Vec3::ZERO, rotation: Quat::IDENTITY };

    pub fn blend(&self, other: &Self, w: f32) -> Self {
        Self {
            translation: self.translation.lerp(other.translation, w),
            rotation: slerp_shortest(self.rotation, other.rotation, w),
        }
    }

    fn inverse(&self) -> Self {
        Self { translation: -self.translation, rotation: self.rotation.inverse() }
    }
}

impl Default for RootMotionDelta {
    fn default() -> Self {
        Self::ZERO
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnimationClip {
    pub name: String,
    /// Seconds. Playback time is wrapped (looping) or clamped to this.
    pub duration: f32,
    pub looping: bool,
    pub channels: Vec<JointChannel>,
    pub root_motion: Option<RootMotion>,
}

impl AnimationClip {
    pub fn new(name: impl Into<String>, duration: f32, looping: bool) -> Self {
        Self { name: name.into(), duration, looping, channels: Vec::new(), root_motion: None }
    }

    /// Duration implied by the last key of every track.
    pub fn duration_from_tracks(channels: &[JointChannel]) -> f32 {
        channels
            .iter()
            .flat_map(|c| {
                [
                    c.translation.as_ref().map(|t| t.end_time()),
                    c.rotation.as_ref().map(|t| t.end_time()),
                    c.scale.as_ref().map(|t| t.end_time()),
                ]
            })
            .flatten()
            .fold(0.0, f32::max)
    }

    /// Maps unbounded playback time to a time inside the clip: wrapped when
    /// looping, clamped to `[0, duration]` otherwise.
    pub fn local_time(&self, time: f32) -> f32 {
        if self.duration <= 0.0 {
            return 0.0;
        }
        if self.looping {
            time.rem_euclid(self.duration)
        } else {
            time.clamp(0.0, self.duration)
        }
    }

    /// Full pose at `time`: bind pose overridden by every animated channel,
    /// root motion left in.
    pub fn sample(&self, skeleton: &Skeleton, time: f32) -> Pose {
        let t = self.local_time(time);
        let mut pose = skeleton.bind_pose();
        for ch in &self.channels {
            let Some(j) = pose.joints.get_mut(ch.joint) else { continue };
            if let Some(v) = ch.translation.as_ref().and_then(|tr| tr.sample(t)) {
                j.translation = v;
            }
            if let Some(v) = ch.rotation.as_ref().and_then(|tr| tr.sample(t)) {
                j.rotation = v;
            }
            if let Some(v) = ch.scale.as_ref().and_then(|tr| tr.sample(t)) {
                j.scale = v;
            }
        }
        pose
    }

    /// Like [`AnimationClip::sample`] but with the configured root motion
    /// removed: the extracted axes / rotation of the root joint are pinned
    /// to their value at clip start. Identical to `sample` when the clip
    /// has no [`RootMotion`].
    pub fn sample_without_root_motion(&self, skeleton: &Skeleton, time: f32) -> Pose {
        let mut pose = self.sample(skeleton, time);
        if let Some(rm) = self.root_motion {
            let start = self.joint_at(skeleton, rm.joint, 0.0);
            if let Some(j) = pose.joints.get_mut(rm.joint) {
                for axis in 0..3 {
                    if rm.translation_axes[axis] {
                        j.translation[axis] = start.translation[axis];
                    }
                }
                if rm.rotation {
                    j.rotation = start.rotation;
                }
            }
        }
        pose
    }

    fn joint_at(&self, skeleton: &Skeleton, joint: usize, local_t: f32) -> JointTransform {
        // `local_t` is already inside the clip; sample ignoring looping wrap.
        let mut j = skeleton.joints().get(joint).map(|j| j.bind).unwrap_or_default();
        for ch in self.channels.iter().filter(|c| c.joint == joint) {
            if let Some(v) = ch.translation.as_ref().and_then(|tr| tr.sample(local_t)) {
                j.translation = v;
            }
            if let Some(v) = ch.rotation.as_ref().and_then(|tr| tr.sample(local_t)) {
                j.rotation = v;
            }
        }
        j
    }

    /// Root motion accumulated between playback times `from` and `to`
    /// (unwrapped: a looping clip at `to = 1.25 * duration` has crossed one
    /// loop boundary). Handles loop wraparound by summing the tail of the
    /// loop and the head of the next. Zero when the clip has no
    /// [`RootMotion`]. Playing backwards (`to < from`) returns the inverse.
    pub fn root_motion_delta(&self, skeleton: &Skeleton, from: f32, to: f32) -> RootMotionDelta {
        let Some(rm) = self.root_motion else { return RootMotionDelta::ZERO };
        if to < from {
            return self.root_motion_delta(skeleton, to, from).inverse();
        }
        let d = self.duration;
        let at = |t: f32| self.joint_at(skeleton, rm.joint, t);
        let mask = Vec3::new(
            rm.translation_axes[0] as u8 as f32,
            rm.translation_axes[1] as u8 as f32,
            rm.translation_axes[2] as u8 as f32,
        );
        let delta = |a: JointTransform, b: JointTransform| RootMotionDelta {
            translation: (b.translation - a.translation) * mask,
            rotation: if rm.rotation { b.rotation * a.rotation.inverse() } else { Quat::IDENTITY },
        };

        if !self.looping || d <= 0.0 {
            return delta(at(self.local_time(from)), at(self.local_time(to)));
        }
        let loops = (to / d).floor() - (from / d).floor();
        let (w0, w1) = (self.local_time(from), self.local_time(to));
        if loops < 1.0 {
            return delta(at(w0), at(w1));
        }
        let first = delta(at(w0), at(d));
        let full = delta(at(0.0), at(d));
        let last = delta(at(0.0), at(w1));
        let mut translation = first.translation + last.translation;
        let mut rotation = first.rotation;
        for _ in 1..(loops as u32) {
            translation += full.translation;
            rotation = full.rotation * rotation;
        }
        rotation = last.rotation * rotation;
        RootMotionDelta { translation, rotation }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeletal::skeleton::tests::two_joint_chain;
    use std::f32::consts::FRAC_PI_2;

    fn vec_track(times: &[f32], xs: &[f32], interp: Interpolation) -> Track<Vec3> {
        Track::new(times.to_vec(), xs.iter().map(|x| Vec3::new(*x, 0.0, 0.0)).collect(), interp).unwrap()
    }

    #[test]
    fn track_validation() {
        assert_eq!(
            Track::<Vec3>::new(vec![0.0, 1.0], vec![Vec3::ZERO], Interpolation::Linear).unwrap_err(),
            ClipError::LengthMismatch { times: 2, values: 1 }
        );
        assert_eq!(
            Track::<Vec3>::new(vec![0.0, 0.0], vec![Vec3::ZERO; 2], Interpolation::Linear).unwrap_err(),
            ClipError::Unsorted(1)
        );
    }

    #[test]
    fn linear_endpoints_midpoint_and_clamping() {
        let t = vec_track(&[0.0, 1.0, 3.0], &[0.0, 10.0, 30.0], Interpolation::Linear);
        assert_eq!(t.sample(0.0).unwrap().x, 0.0);
        assert_eq!(t.sample(0.5).unwrap().x, 5.0);
        assert_eq!(t.sample(1.0).unwrap().x, 10.0);
        assert_eq!(t.sample(2.0).unwrap().x, 20.0);
        assert_eq!(t.sample(3.0).unwrap().x, 30.0);
        // clamped outside
        assert_eq!(t.sample(-5.0).unwrap().x, 0.0);
        assert_eq!(t.sample(99.0).unwrap().x, 30.0);
    }

    #[test]
    fn step_holds_until_next_key() {
        let t = vec_track(&[0.0, 1.0, 2.0], &[1.0, 2.0, 3.0], Interpolation::Step);
        assert_eq!(t.sample(0.0).unwrap().x, 1.0);
        assert_eq!(t.sample(0.99).unwrap().x, 1.0);
        assert_eq!(t.sample(1.0).unwrap().x, 2.0);
        assert_eq!(t.sample(1.5).unwrap().x, 2.0);
        assert_eq!(t.sample(2.0).unwrap().x, 3.0);
    }

    #[test]
    fn empty_and_single_key_tracks() {
        let e = Track::<Vec3>::new(vec![], vec![], Interpolation::Linear).unwrap();
        assert!(e.sample(1.0).is_none());
        let s = vec_track(&[0.5], &[7.0], Interpolation::Linear);
        assert_eq!(s.sample(0.0).unwrap().x, 7.0);
        assert_eq!(s.sample(9.0).unwrap().x, 7.0);
    }

    #[test]
    fn rotation_track_slerps_shortest_arc() {
        let a = Quat::from_rotation_z(10f32.to_radians());
        let b = Quat::from_rotation_z(350f32.to_radians()); // == -10deg, other hemisphere
        let t = Track::new(vec![0.0, 1.0], vec![a, b], Interpolation::Linear).unwrap();
        let mid = t.sample(0.5).unwrap();
        assert!(mid.dot(Quat::IDENTITY).abs() > 0.99999, "{mid:?}");
    }

    fn walk_clip(looping: bool) -> AnimationClip {
        // joint 1 ("tip") translates 0 -> 4 along X over 2 s and rotates 0 -> 90deg about Z.
        let mut c = AnimationClip::new("walk", 2.0, looping);
        let mut ch = JointChannel::new(1);
        ch.translation = Some(vec_track(&[0.0, 2.0], &[0.0, 4.0], Interpolation::Linear));
        ch.rotation = Some(
            Track::new(
                vec![0.0, 2.0],
                vec![Quat::IDENTITY, Quat::from_rotation_z(FRAC_PI_2)],
                Interpolation::Linear,
            )
            .unwrap(),
        );
        c.channels.push(ch);
        c
    }

    #[test]
    fn clip_sampling_endpoints_midpoint_and_untouched_joints() {
        let skel = two_joint_chain();
        let clip = walk_clip(false);
        let p0 = clip.sample(&skel, 0.0);
        let p1 = clip.sample(&skel, 1.0);
        let p2 = clip.sample(&skel, 2.0);
        assert_eq!(p0.joints[1].translation, Vec3::ZERO);
        assert_eq!(p1.joints[1].translation, Vec3::new(2.0, 0.0, 0.0));
        assert_eq!(p2.joints[1].translation, Vec3::new(4.0, 0.0, 0.0));
        assert!(p1.joints[1].rotation.dot(Quat::from_rotation_z(FRAC_PI_2 / 2.0)) > 0.99999);
        // root has no channel: stays at bind.
        assert_eq!(p1.joints[0], skel.joints()[0].bind);
        // non-looping clamps past the end and before the start
        assert_eq!(clip.sample(&skel, 50.0), p2);
        assert_eq!(clip.sample(&skel, -3.0), p0);
    }

    #[test]
    fn looping_wraps() {
        let skel = two_joint_chain();
        let clip = walk_clip(true);
        let a = clip.sample(&skel, 0.5);
        let b = clip.sample(&skel, 2.5);
        let c = clip.sample(&skel, -1.5);
        assert!((a.joints[1].translation - b.joints[1].translation).length() < 1e-5);
        assert!((a.joints[1].translation - c.joints[1].translation).length() < 1e-5);
        // exactly at the duration, a looping clip is back at the start
        assert_eq!(clip.sample(&skel, 2.0).joints[1].translation, Vec3::ZERO);
    }

    #[test]
    fn duration_from_tracks_uses_last_key() {
        let clip = walk_clip(false);
        assert_eq!(AnimationClip::duration_from_tracks(&clip.channels), 2.0);
        assert_eq!(AnimationClip::duration_from_tracks(&[]), 0.0);
    }

    #[test]
    fn out_of_range_channel_joint_is_ignored() {
        let skel = two_joint_chain();
        let mut clip = walk_clip(false);
        clip.channels.push(JointChannel { joint: 99, translation: clip.channels[0].translation.clone(), ..Default::default() });
        let _ = clip.sample(&skel, 1.0); // must not panic
    }

    fn root_clip(looping: bool) -> AnimationClip {
        // joint 0 moves (0,0,0) -> (4, 1, 8) over 2 s; mask extracts X and Z only.
        let mut c = AnimationClip::new("run", 2.0, looping);
        let mut ch = JointChannel::new(0);
        ch.translation = Some(
            Track::new(
                vec![0.0, 2.0],
                vec![Vec3::ZERO, Vec3::new(4.0, 1.0, 8.0)],
                Interpolation::Linear,
            )
            .unwrap(),
        );
        c.channels.push(ch);
        c.root_motion = Some(RootMotion::horizontal(0));
        c
    }

    #[test]
    fn root_motion_is_stripped_from_pose_and_reported_as_delta() {
        let skel = two_joint_chain();
        let clip = root_clip(false);
        let raw = clip.sample(&skel, 1.0);
        assert_eq!(raw.joints[0].translation, Vec3::new(2.0, 0.5, 4.0));
        let stripped = clip.sample_without_root_motion(&skel, 1.0);
        // X/Z pinned to clip start, Y (not extracted) still animated.
        assert_eq!(stripped.joints[0].translation, Vec3::new(0.0, 0.5, 0.0));

        let d = clip.root_motion_delta(&skel, 0.5, 1.5);
        assert!((d.translation - Vec3::new(2.0, 0.0, 4.0)).length() < 1e-5);
        assert_eq!(d.rotation, Quat::IDENTITY);
        // deltas add up across a split
        let d1 = clip.root_motion_delta(&skel, 0.0, 0.7);
        let d2 = clip.root_motion_delta(&skel, 0.7, 2.0);
        assert!((d1.translation + d2.translation - Vec3::new(4.0, 0.0, 8.0)).length() < 1e-4);
        // non-looping: no motion past the end
        let past = clip.root_motion_delta(&skel, 2.0, 5.0);
        assert!(past.translation.length() < 1e-6);
        // backwards = inverse
        let back = clip.root_motion_delta(&skel, 1.5, 0.5);
        assert!((back.translation + d.translation).length() < 1e-5);
    }

    #[test]
    fn root_motion_across_loop_boundary() {
        let skel = two_joint_chain();
        let clip = root_clip(true);
        // 1.5 -> 2.5 crosses the loop: (end-1.5) + (0.5-start) = (1,0,2) + (1,0,2)
        let d = clip.root_motion_delta(&skel, 1.5, 2.5);
        assert!((d.translation - Vec3::new(2.0, 0.0, 4.0)).length() < 1e-5, "{:?}", d.translation);
        // 1.5 -> 4.5 crosses two loops: 1 + 4 (one full loop) + 1 => (1,0,2)+(4,0,8)+(2.5*... local 0.5 => 1,0,2)
        let d = clip.root_motion_delta(&skel, 1.5, 4.5);
        assert!((d.translation - Vec3::new(6.0, 0.0, 12.0)).length() < 1e-4, "{:?}", d.translation);
        // no root motion config -> zero
        let mut none = clip.clone();
        none.root_motion = None;
        assert_eq!(none.root_motion_delta(&skel, 0.0, 1.0), RootMotionDelta::ZERO);
    }

    #[test]
    fn root_rotation_delta() {
        let skel = two_joint_chain();
        let mut c = AnimationClip::new("turn", 1.0, false);
        let mut ch = JointChannel::new(0);
        ch.rotation = Some(
            Track::new(vec![0.0, 1.0], vec![Quat::IDENTITY, Quat::from_rotation_y(FRAC_PI_2)], Interpolation::Linear)
                .unwrap(),
        );
        c.channels.push(ch);
        c.root_motion = Some(RootMotion { joint: 0, translation_axes: [false; 3], rotation: true });
        let d = c.root_motion_delta(&skel, 0.0, 1.0);
        assert!(d.rotation.dot(Quat::from_rotation_y(FRAC_PI_2)) > 0.99999);
        let p = c.sample_without_root_motion(&skel, 1.0);
        assert!(p.joints[0].rotation.dot(Quat::IDENTITY) > 0.99999);
    }
}
