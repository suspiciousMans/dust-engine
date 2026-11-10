//! Viewport interaction: ties picking, the gizmo, snapping and the modal
//! G/R/S operators to the editor state. One entry point,
//! [`ViewportInteraction::update`], and one for drawing.
//!
//! Every transform gesture (gizmo drag or modal) previews live by writing the
//! `Transform`s directly, then commits **one** [`TransformCommand`] on the
//! undo stack when it ends. Cancelling restores the originals and records
//! nothing. Only `Selection::top_level` entities are moved (descendants follow
//! through the hierarchy) and locked entities are never touched.

use std::any::Any;

use egui::{Color32, Key, Modifiers, Painter, Pos2, Rect, Stroke};
use glam::{Quat, Vec3};
use hecs::{Entity, World};

use super::cam_matrices::CameraMatrices;
use super::command::{Command, Ctx};
use super::gizmo::{apply_delta, pivot_point, Delta, Gizmo, GizmoMode, Handle, Pivot, Space};
use super::hierarchy::is_locked;
use super::modal::{ModalAction, ModalKind, ModalSession};
use super::picking::{apply_box_selection, box_select, pick_all, BoxMode, BoxOp, ClickSelector};
use super::snapping::{collect_snap_targets, snap_angle, snap_scale_factor, snap_to_targets_at_cursor, snap_value, snap_vec3, SnapSettings};
use super::state::{EditorMode, EditorState, Tool};
use super::world_xform::{set_world_transform, world_rotation, world_transform};
use crate::ecs::Transform;

// ------------------------------------------------------------------ input

/// Per-frame pointer/keyboard snapshot for the viewport. Build it from egui
/// with [`ViewportInput::from_egui`] or fill it by hand (tests do).
#[derive(Clone, Debug)]
pub struct ViewportInput {
    pub cursor: Option<Pos2>,
    /// Pointer is over the viewport (not over a panel or popup).
    pub hovered: bool,
    pub primary_pressed: bool,
    pub primary_down: bool,
    pub primary_released: bool,
    pub secondary_pressed: bool,
    pub modifiers: Modifiers,
    pub keys_pressed: Vec<Key>,
    /// Text typed this frame (for numeric entry).
    pub text: String,
    /// Allow bare G/R/S to start modal transforms. Turn off if a keymap
    /// preset already uses those keys (e.g. Unity-like R = scale tool).
    pub modal_hotkeys: bool,
}

impl Default for ViewportInput {
    fn default() -> Self {
        Self {
            cursor: None,
            hovered: false,
            primary_pressed: false,
            primary_down: false,
            primary_released: false,
            secondary_pressed: false,
            modifiers: Modifiers::NONE,
            keys_pressed: Vec::new(),
            text: String::new(),
            modal_hotkeys: true,
        }
    }
}

impl ViewportInput {
    pub fn from_egui(input: &egui::InputState, hovered: bool) -> Self {
        use egui::PointerButton::{Primary, Secondary};
        let mut v = Self {
            cursor: input.pointer.hover_pos().or(input.pointer.latest_pos()),
            hovered,
            primary_pressed: input.pointer.button_pressed(Primary),
            primary_down: input.pointer.button_down(Primary),
            primary_released: input.pointer.button_released(Primary),
            secondary_pressed: input.pointer.button_pressed(Secondary),
            modifiers: input.modifiers,
            ..Default::default()
        };
        for ev in &input.events {
            match ev {
                egui::Event::Key { key, pressed: true, repeat: false, .. } => v.keys_pressed.push(*key),
                egui::Event::Text(t) => v.text.push_str(t),
                _ => {}
            }
        }
        v
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewportEvent {
    SelectionChanged,
    ModalStarted(ModalKind),
    ModalEnded { committed: bool },
    /// A gizmo drag started.
    GizmoStarted,
    TransformCommitted { label: String },
    TransformCancelled,
    Error(String),
}

// ---------------------------------------------------------------- command

/// Undo step for a whole move/rotate/scale gesture over any number of
/// entities. Stores local transforms before and after.
pub struct TransformCommand {
    label: String,
    items: Vec<(Entity, Transform, Transform)>,
}

impl TransformCommand {
    pub fn new(label: impl Into<String>, items: Vec<(Entity, Transform, Transform)>) -> Self {
        Self { label: label.into(), items }
    }
    fn set(world: &mut World, e: Entity, t: Transform) -> bool {
        match world.get::<&mut Transform>(e) {
            Ok(mut cur) => {
                *cur = t;
                true
            }
            Err(_) => false,
        }
    }
}

impl Command for TransformCommand {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let n = self.items.iter().filter(|(e, _, after)| Self::set(ctx.world, *e, *after)).count();
        if n == 0 {
            Err("no transformable entities".into())
        } else {
            Ok(())
        }
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        for (e, before, _) in &self.items {
            Self::set(ctx.world, *e, *before);
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---------------------------------------------------------------- gesture

struct Gesture {
    entities: Vec<Entity>,
    initial_world: Vec<Transform>,
    initial_local: Vec<Transform>,
    pivot: Vec3,
    frame: Quat,
    space: Space,
    pivot_mode: Pivot,
    snap_targets: Vec<Vec3>,
    primary_rotation: Quat,
}

/// Unlocked top-level selected entities that have a `Transform`.
fn movable(world: &World, state: &EditorState) -> Vec<Entity> {
    state
        .selection
        .top_level(world)
        .into_iter()
        .filter(|e| world.satisfies::<&Transform>(*e).unwrap_or(false) && !is_locked(world, *e))
        .collect()
}

impl Gesture {
    fn begin(world: &World, state: &EditorState, gizmo: &Gizmo) -> Option<Self> {
        let entities = movable(world, state);
        if entities.is_empty() {
            return None;
        }
        let initial_world: Vec<Transform> = entities.iter().map(|e| world_transform(world, *e)).collect();
        let initial_local: Vec<Transform> = entities.iter().map(|e| *world.get::<&Transform>(*e).unwrap()).collect();
        let pos: Vec<Vec3> = initial_world.iter().map(|t| t.position).collect();
        let primary_idx = state.selection.primary().and_then(|p| entities.iter().position(|e| *e == p)).unwrap_or(0);
        let pivot = pivot_point(&pos, primary_idx, gizmo.pivot);
        let primary_rotation = initial_world[primary_idx].rotation;
        let frame = gizmo.frame(primary_rotation);
        let snap_targets = collect_snap_targets(world, &entities);
        Some(Self { entities, initial_world, initial_local, pivot, frame, space: gizmo.space, pivot_mode: gizmo.pivot, snap_targets, primary_rotation })
    }

    fn apply(&self, world: &mut World, delta: &Delta, frame: Quat, space: Space) {
        let out = apply_delta(&self.initial_world, self.pivot, frame, self.pivot_mode, space, delta);
        for (e, t) in self.entities.iter().zip(out) {
            set_world_transform(world, *e, &t);
        }
    }

    fn restore(&self, world: &mut World) {
        for (e, t) in self.entities.iter().zip(&self.initial_local) {
            TransformCommand::set(world, *e, *t);
        }
    }

    /// Records one undo step if anything changed. Returns the label.
    fn commit(&self, world: &mut World, state: &mut EditorState, verb: &str) -> Result<Option<String>, String> {
        let items: Vec<(Entity, Transform, Transform)> = self
            .entities
            .iter()
            .zip(&self.initial_local)
            .filter_map(|(e, before)| world.get::<&Transform>(*e).ok().map(|after| (*e, *before, *after)))
            .filter(|(_, b, a)| (b.position, b.rotation, b.scale) != (a.position, a.rotation, a.scale))
            .collect();
        if items.is_empty() {
            return Ok(None);
        }
        let label = if items.len() == 1 { verb.to_string() } else { format!("{verb} {} objects", items.len()) };
        let mut ctx = Ctx { world, registry: &state.registry };
        state.undo.execute(Box::new(TransformCommand::new(label.clone(), items)), &mut ctx)?;
        state.undo.seal();
        Ok(Some(label))
    }
}

fn verb(mode: GizmoMode) -> &'static str {
    match mode {
        GizmoMode::Translate => "Move",
        GizmoMode::Rotate => "Rotate",
        GizmoMode::Scale => "Scale",
    }
}

// ------------------------------------------------------------ interaction

struct Press {
    pos: Pos2,
    marquee: bool,
}

struct ActiveModal {
    session: ModalSession,
    gesture: Gesture,
    hud: String,
}

struct ActiveDrag {
    gesture: Gesture,
}

#[derive(Default)]
pub struct ViewportInteraction {
    pub gizmo: Gizmo,
    pub snap: SnapSettings,
    pub box_mode: BoxMode,
    click: ClickSelector,
    press: Option<Press>,
    drag: Option<ActiveDrag>,
    modal: Option<ActiveModal>,
    last_cursor: Option<Pos2>,
    /// Cached gizmo placement (origin, frame) for `draw`.
    anchor: Option<(Vec3, Quat)>,
}

impl ViewportInteraction {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_modal_active(&self) -> bool {
        self.modal.is_some()
    }
    pub fn is_dragging_gizmo(&self) -> bool {
        self.drag.is_some()
    }
    /// True while any gesture owns the pointer (suppress camera navigation).
    pub fn is_busy(&self) -> bool {
        self.modal.is_some() || self.drag.is_some() || self.press.as_ref().map(|p| p.marquee).unwrap_or(false)
    }
    /// HUD line for the running modal transform.
    pub fn hud(&self) -> Option<&str> {
        self.modal.as_ref().map(|m| m.hud.as_str())
    }

    fn compute_anchor(&self, world: &World, state: &EditorState) -> Option<(Vec3, Quat)> {
        let ents = movable(world, state);
        if ents.is_empty() {
            return None;
        }
        let pos: Vec<Vec3> = ents.iter().map(|e| world_transform(world, *e).position).collect();
        let primary = state.selection.primary().and_then(|p| ents.iter().position(|e| *e == p)).unwrap_or(0);
        let origin = pivot_point(&pos, primary, self.gizmo.pivot);
        Some((origin, self.gizmo.frame(world_rotation(world, ents[primary]))))
    }

    pub fn update(&mut self, input: &ViewportInput, cam: &CameraMatrices, world: &mut World, state: &mut EditorState) -> Vec<ViewportEvent> {
        let mut ev = Vec::new();
        if input.cursor.is_some() {
            self.last_cursor = input.cursor;
        }
        self.gizmo.mode = match state.tool {
            Tool::Move => GizmoMode::Translate,
            Tool::Rotate => GizmoMode::Rotate,
            Tool::Scale => GizmoMode::Scale,
        };
        state.selection.prune(world);

        if state.mode != EditorMode::Edit {
            self.cancel_all(world, &mut ev);
            self.press = None;
            self.anchor = None;
            return ev;
        }

        if self.modal.is_some() {
            self.update_modal(input, cam, world, state, &mut ev);
        } else if self.drag.is_some() {
            self.update_drag(input, cam, world, state, &mut ev);
        } else if !self.try_start_modal(input, world, state, &mut ev) {
            self.update_pointer(input, cam, world, state, &mut ev);
        }

        self.anchor = self.compute_anchor(world, state);
        if self.modal.is_none() && self.drag.is_none() {
            match self.anchor {
                Some((o, f)) if self.press.is_none() => self.gizmo.update_hover(cam, o, f, input.cursor.filter(|_| input.hovered)),
                _ => self.gizmo.hover = None,
            }
        }
        ev
    }

    fn cancel_all(&mut self, world: &mut World, ev: &mut Vec<ViewportEvent>) {
        if let Some(m) = self.modal.take() {
            m.gesture.restore(world);
            ev.push(ViewportEvent::ModalEnded { committed: false });
        }
        if let Some(d) = self.drag.take() {
            d.gesture.restore(world);
            self.gizmo.end_drag();
            ev.push(ViewportEvent::TransformCancelled);
        }
    }

    // ------------------------------------------------------------- modal

    fn try_start_modal(&mut self, input: &ViewportInput, world: &mut World, state: &mut EditorState, ev: &mut Vec<ViewportEvent>) -> bool {
        let m = input.modifiers;
        if !input.modal_hotkeys || !input.hovered || m.ctrl || m.alt || m.command || self.press.is_some() {
            return false;
        }
        let Some(cursor) = input.cursor else { return false };
        let Some(kind) = input.keys_pressed.iter().find_map(|k| match k {
            Key::G => Some(ModalKind::Grab),
            Key::R => Some(ModalKind::Rotate),
            Key::S => Some(ModalKind::Scale),
            _ => None,
        }) else {
            return false;
        };
        let Some(gesture) = Gesture::begin(world, state, &self.gizmo) else { return false };
        let session = ModalSession::new(kind, gesture.pivot, gesture.primary_rotation, cursor);
        let hud = session.hud(&match kind {
            ModalKind::Grab => Delta::Translate(Vec3::ZERO),
            ModalKind::Rotate => Delta::Rotate { axis: Vec3::Z, angle: 0.0 },
            ModalKind::Scale => Delta::Scale(Vec3::ONE),
        });
        self.modal = Some(ActiveModal { session, gesture, hud });
        ev.push(ViewportEvent::ModalStarted(kind));
        true
    }

    fn update_modal(&mut self, input: &ViewportInput, cam: &CameraMatrices, world: &mut World, state: &mut EditorState, ev: &mut Vec<ViewportEvent>) {
        let Some(mut m) = self.modal.take() else { return };
        let mut action = ModalAction::None;
        for c in input.text.chars() {
            m.session.type_char(c);
        }
        for k in &input.keys_pressed {
            match m.session.handle_key(*k, input.modifiers.shift) {
                ModalAction::None => {}
                a => action = a,
            }
        }
        if input.primary_pressed {
            action = ModalAction::Confirm;
        }
        if input.secondary_pressed {
            action = ModalAction::Cancel;
        }
        let cursor = input.cursor.or(self.last_cursor).unwrap_or(cam.rect.center());
        let snap_on = self.snap.active(input.modifiers.ctrl);
        let delta = m.session.delta(cam, cursor, &self.snap, snap_on, &m.gesture.snap_targets);
        let space = if m.session.local { Space::Local } else { Space::Global };
        m.gesture.apply(world, &delta, m.session.frame(), space);
        m.hud = m.session.hud(&delta);
        let kind = m.session.kind;
        match action {
            ModalAction::None => self.modal = Some(m),
            ModalAction::Cancel => {
                m.gesture.restore(world);
                ev.push(ViewportEvent::ModalEnded { committed: false });
            }
            ModalAction::Confirm => {
                let v = match kind {
                    ModalKind::Grab => "Move",
                    ModalKind::Rotate => "Rotate",
                    ModalKind::Scale => "Scale",
                };
                match m.gesture.commit(world, state, v) {
                    Ok(Some(label)) => {
                        ev.push(ViewportEvent::TransformCommitted { label });
                        ev.push(ViewportEvent::ModalEnded { committed: true });
                    }
                    Ok(None) => ev.push(ViewportEvent::ModalEnded { committed: false }),
                    Err(e) => {
                        m.gesture.restore(world);
                        ev.push(ViewportEvent::Error(e));
                        ev.push(ViewportEvent::ModalEnded { committed: false });
                    }
                }
            }
        }
    }

    // ------------------------------------------------------- gizmo drag

    fn update_drag(&mut self, input: &ViewportInput, cam: &CameraMatrices, world: &mut World, state: &mut EditorState, ev: &mut Vec<ViewportEvent>) {
        let Some(d) = self.drag.take() else { return };
        if input.keys_pressed.contains(&Key::Escape) || input.secondary_pressed {
            d.gesture.restore(world);
            self.gizmo.end_drag();
            ev.push(ViewportEvent::TransformCancelled);
            return;
        }
        let cursor = input.cursor.or(self.last_cursor).unwrap_or(cam.rect.center());
        if let Some(delta) = self.gizmo.drag_delta(cam, cursor) {
            let snap_on = self.snap.active(input.modifiers.ctrl);
            let delta = self.snap_delta(delta, snap_on, cursor, cam, &d.gesture);
            d.gesture.apply(world, &delta, d.gesture.frame, d.gesture.space);
        }
        if input.primary_released || !input.primary_down {
            self.gizmo.end_drag();
            match d.gesture.commit(world, state, verb(self.gizmo.mode)) {
                Ok(Some(label)) => ev.push(ViewportEvent::TransformCommitted { label }),
                Ok(None) => ev.push(ViewportEvent::TransformCancelled),
                Err(e) => {
                    d.gesture.restore(world);
                    ev.push(ViewportEvent::Error(e));
                }
            }
        } else {
            self.drag = Some(d);
        }
    }

    fn snap_delta(&self, delta: Delta, on: bool, cursor: Pos2, cam: &CameraMatrices, g: &Gesture) -> Delta {
        if !on {
            return delta;
        }
        let s = &self.snap;
        match delta {
            Delta::Translate(d) => match self.gizmo.active_handle() {
                Some(Handle::Axis(n)) => {
                    let a = g.frame * Vec3::AXES[n];
                    Delta::Translate(a * snap_value(d.dot(a), s.grid))
                }
                Some(Handle::Center) if s.bounds_snap => match snap_to_targets_at_cursor(cursor, &g.snap_targets, cam, s.bounds_radius_px) {
                    Some(t) => Delta::Translate(t - g.pivot),
                    None => Delta::Translate(snap_vec3(d, s.grid)),
                },
                _ => Delta::Translate(snap_vec3(d, s.grid)),
            },
            Delta::Rotate { axis, angle } => Delta::Rotate { axis, angle: snap_angle(angle, s.angle_deg) },
            Delta::Scale(f) => {
                let sn = |v: f32| if (v - 1.0).abs() > 1e-6 { snap_scale_factor(v, s.scale_step) } else { 1.0 };
                Delta::Scale(Vec3::new(sn(f.x), sn(f.y), sn(f.z)))
            }
        }
    }

    // ---------------------------------------------------------- pointer

    fn update_pointer(&mut self, input: &ViewportInput, cam: &CameraMatrices, world: &mut World, state: &mut EditorState, ev: &mut Vec<ViewportEvent>) {
        if input.primary_pressed && input.hovered {
            if let Some(pos) = input.cursor {
                if let Some((o, f)) = self.compute_anchor(world, state) {
                    if let Some(h) = self.gizmo.hit_test(cam, o, f, pos) {
                        if let Some(g) = Gesture::begin(world, state, &self.gizmo) {
                            if self.gizmo.begin_drag(h, cam, o, g.frame, pos) {
                                self.drag = Some(ActiveDrag { gesture: g });
                                ev.push(ViewportEvent::GizmoStarted);
                                return;
                            }
                        }
                    }
                }
                self.press = Some(Press { pos, marquee: false });
            }
        }
        let Some(mut p) = self.press.take() else { return };
        if let Some(c) = input.cursor {
            if !p.marquee && input.primary_down && c.distance(p.pos) > 4.0 {
                p.marquee = true;
            }
        }
        if !input.primary_released {
            if input.primary_down {
                self.press = Some(p);
            }
            return;
        }
        let before = state.selection.to_vec();
        let end = input.cursor.unwrap_or(p.pos);
        if p.marquee {
            let found = box_select(world, cam, Rect::from_two_pos(p.pos, end), self.box_mode);
            apply_box_selection(&mut state.selection, &found, BoxOp::from_modifiers(input.modifiers));
        } else {
            let hits: Vec<Entity> = pick_all(world, &cam.ray(p.pos)).into_iter().map(|h| h.entity).collect();
            self.click.click(&mut state.selection, &hits, p.pos, input.modifiers);
        }
        if state.selection.to_vec() != before {
            ev.push(ViewportEvent::SelectionChanged);
        }
    }

    // ------------------------------------------------------------- draw

    pub fn draw(&self, painter: &Painter, cam: &CameraMatrices) {
        if let Some(m) = &self.modal {
            painter.text(
                cam.rect.left_bottom() + egui::vec2(12.0, -12.0),
                egui::Align2::LEFT_BOTTOM,
                &m.hud,
                egui::FontId::monospace(14.0),
                Color32::from_rgb(255, 220, 80),
            );
            return;
        }
        if let Some((o, f)) = self.anchor {
            self.gizmo.draw(painter, cam, o, f);
        }
        if let (Some(p), Some(c)) = (&self.press, self.last_cursor) {
            if p.marquee {
                let r = Rect::from_two_pos(p.pos, c);
                painter.rect(r, 0.0, Color32::from_rgba_unmultiplied(120, 170, 255, 30), Stroke::new(1.0, Color32::from_rgb(140, 190, 255)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::cam_matrices::test_util::*;
    use crate::editor::hierarchy::{reparent, Locked, Primitive, PrimitiveShape, Visible};

    struct Rig {
        ui: ViewportInteraction,
        cam: CameraMatrices,
        world: World,
        st: EditorState,
    }

    impl Rig {
        fn new() -> Self {
            Self { ui: ViewportInteraction::new(), cam: cam_front(), world: World::new(), st: EditorState::new() }
        }
        fn cube(&mut self, p: Vec3) -> Entity {
            self.world.spawn((Transform::from_position(p), Primitive(PrimitiveShape::Cube)))
        }
        fn scr(&self, p: Vec3) -> Pos2 {
            self.cam.project(p).unwrap()
        }
        fn run(&mut self, i: ViewportInput) -> Vec<ViewportEvent> {
            let cam = self.cam;
            self.ui.update(&i, &cam, &mut self.world, &mut self.st)
        }
        fn input(&self, at: Pos2) -> ViewportInput {
            ViewportInput { cursor: Some(at), hovered: true, ..Default::default() }
        }
        fn click(&mut self, at: Pos2, mods: Modifiers) -> Vec<ViewportEvent> {
            let mut a = self.input(at);
            a.primary_pressed = true;
            a.primary_down = true;
            a.modifiers = mods;
            self.run(a);
            let mut b = self.input(at);
            b.primary_released = true;
            b.modifiers = mods;
            self.run(b)
        }
        fn key(&mut self, at: Pos2, k: Key) -> Vec<ViewportEvent> {
            let mut i = self.input(at);
            i.keys_pressed.push(k);
            self.run(i)
        }
        fn text(&mut self, at: Pos2, t: &str) {
            let mut i = self.input(at);
            i.text = t.into();
            self.run(i);
        }
        fn pos(&self, e: Entity) -> Vec3 {
            self.world.get::<&Transform>(e).unwrap().position
        }
        fn undo(&mut self) -> bool {
            let mut ctx = Ctx { world: &mut self.world, registry: &self.st.registry };
            self.st.undo.undo(&mut ctx)
        }
        fn redo(&mut self) -> bool {
            let mut ctx = Ctx { world: &mut self.world, registry: &self.st.registry };
            self.st.undo.redo(&mut ctx)
        }
    }

    #[test]
    fn click_selects_and_empty_click_clears() {
        let mut r = Rig::new();
        let a = r.cube(Vec3::ZERO);
        let ev = r.click(r.scr(Vec3::ZERO), Modifiers::NONE);
        assert_eq!(ev, vec![ViewportEvent::SelectionChanged]);
        assert_eq!(r.st.selection.primary(), Some(a));
        // click far from the gizmo and the cube
        r.click(Pos2::new(120.0, 70.0), Modifiers::NONE);
        assert!(r.st.selection.is_empty());
    }

    #[test]
    fn click_skips_locked_and_hidden() {
        let mut r = Rig::new();
        r.world.spawn((Transform::from_position(Vec3::new(0.0, 0.0, 3.0)), Primitive(PrimitiveShape::Cube), Locked(true)));
        r.world.spawn((Transform::from_position(Vec3::new(0.0, 0.0, 2.0)), Primitive(PrimitiveShape::Cube), Visible(false)));
        let back = r.cube(Vec3::ZERO);
        r.click(r.scr(Vec3::ZERO), Modifiers::NONE);
        assert_eq!(r.st.selection.primary(), Some(back));
    }

    #[test]
    fn repeat_click_cycles_and_shift_adds() {
        let mut r = Rig::new();
        let a = r.cube(Vec3::new(0.0, 0.0, 2.0));
        let b = r.cube(Vec3::ZERO);
        // click away from the gizmo centre handle: use a point on the cube's face edge
        let at = r.scr(Vec3::new(0.4, -0.4, 0.0)) + egui::vec2(0.0, 0.0);
        r.click(at, Modifiers::NONE);
        assert_eq!(r.st.selection.to_vec(), vec![a]);
        // move cursor off the gizmo for the second press: gizmo handles may swallow it
        r.click(at, Modifiers::NONE);
        assert_eq!(r.st.selection.to_vec(), vec![b]);
        r.click(at, Modifiers::NONE);
        assert_eq!(r.st.selection.to_vec(), vec![a]);
        r.click(at, Modifiers::SHIFT);
        assert_eq!(r.st.selection.len(), 2);
    }

    #[test]
    fn marquee_selects_with_modes() {
        let mut r = Rig::new();
        let a = r.cube(Vec3::new(-3.0, 0.0, 0.0));
        let b = r.cube(Vec3::new(3.0, 0.0, 0.0));
        let (pa, pb) = (r.scr(Vec3::new(-3.0, 0.0, 0.0)), r.scr(Vec3::new(3.0, 0.0, 0.0)));
        let drag = |r: &mut Rig, c: Pos2, m: Modifiers| {
            let (from, to) = (c - egui::vec2(60.0, 60.0), c + egui::vec2(60.0, 60.0));
            let mut i = r.input(from);
            i.primary_pressed = true;
            i.primary_down = true;
            i.modifiers = m;
            r.run(i);
            let mut i = r.input(to);
            i.primary_down = true;
            i.modifiers = m;
            r.run(i);
            let mut i = r.input(to);
            i.primary_released = true;
            i.modifiers = m;
            r.run(i)
        };
        let ev = drag(&mut r, pa, Modifiers::NONE);
        assert_eq!(ev, vec![ViewportEvent::SelectionChanged]);
        assert_eq!(r.st.selection.to_vec(), vec![a]);
        drag(&mut r, pb, Modifiers::SHIFT);
        assert_eq!(r.st.selection.len(), 2);
        drag(&mut r, pa, Modifiers::CTRL);
        assert_eq!(r.st.selection.to_vec(), vec![b]);
    }

    #[test]
    fn gizmo_drag_moves_and_is_one_undo_step() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        r.run(r.input(Pos2::ZERO)); // compute anchor
        let o = r.scr(Vec3::ZERO);
        let mut i = r.input(o + egui::vec2(60.0, 0.0)); // on the X arrow
        i.primary_pressed = true;
        i.primary_down = true;
        assert_eq!(r.run(i), vec![ViewportEvent::GizmoStarted]);
        for dx in [100.0, 130.0, 160.0] {
            let mut i = r.input(o + egui::vec2(dx, 0.0));
            i.primary_down = true;
            r.run(i);
        }
        assert!(r.pos(e).x > 1.0 && r.pos(e).y.abs() < 1e-4);
        let mut i = r.input(o + egui::vec2(160.0, 0.0));
        i.primary_released = true;
        let ev = r.run(i);
        assert_eq!(ev, vec![ViewportEvent::TransformCommitted { label: "Move".into() }]);
        assert_eq!(r.st.undo.undo_len(), 1);
        let moved = r.pos(e);
        assert!(r.undo());
        assert_eq!(r.pos(e), Vec3::ZERO);
        assert!(r.redo());
        assert_eq!(r.pos(e), moved);
    }

    #[test]
    fn gizmo_drag_escape_restores_without_undo_entry() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        r.run(r.input(Pos2::ZERO));
        let o = r.scr(Vec3::ZERO);
        let mut i = r.input(o + egui::vec2(60.0, 0.0));
        i.primary_pressed = true;
        i.primary_down = true;
        r.run(i);
        let mut i = r.input(o + egui::vec2(150.0, 0.0));
        i.primary_down = true;
        r.run(i);
        assert!(r.pos(e).x > 0.5);
        let mut i = r.input(o + egui::vec2(150.0, 0.0));
        i.primary_down = true;
        i.keys_pressed.push(Key::Escape);
        assert_eq!(r.run(i), vec![ViewportEvent::TransformCancelled]);
        assert_eq!(r.pos(e), Vec3::ZERO);
        assert_eq!(r.st.undo.undo_len(), 0);
    }

    #[test]
    fn modal_grab_x_typed_enter_commits_once() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::new(1.0, 0.0, 0.0));
        r.st.selection.select_only(e);
        let at = r.scr(Vec3::ZERO) + egui::vec2(200.0, 100.0);
        assert_eq!(r.key(at, Key::G), vec![ViewportEvent::ModalStarted(ModalKind::Grab)]);
        assert!(r.ui.is_modal_active() && r.ui.is_busy());
        r.key(at, Key::X);
        r.text(at, "2.5");
        assert_eq!(r.ui.hud(), Some("Move X: 2.5_ m"));
        assert!((r.pos(e).x - 3.5).abs() < 1e-4);
        let ev = r.key(at, Key::Enter);
        assert_eq!(ev, vec![ViewportEvent::TransformCommitted { label: "Move".into() }, ViewportEvent::ModalEnded { committed: true }]);
        assert_eq!(r.st.undo.undo_len(), 1);
        assert!(!r.ui.is_modal_active());
        r.undo();
        assert_eq!(r.pos(e).x, 1.0);
    }

    #[test]
    fn modal_escape_and_right_click_restore() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::G);
        let moved = at + egui::vec2(80.0, -40.0);
        r.run(r.input(moved));
        assert!(r.pos(e).length() > 0.1);
        assert_eq!(r.key(moved, Key::Escape), vec![ViewportEvent::ModalEnded { committed: false }]);
        assert_eq!(r.pos(e), Vec3::ZERO);
        r.key(at, Key::G);
        r.run(r.input(moved));
        let mut i = r.input(moved);
        i.secondary_pressed = true;
        r.run(i);
        assert_eq!(r.pos(e), Vec3::ZERO);
        assert_eq!(r.st.undo.undo_len(), 0);
    }

    #[test]
    fn modal_click_confirms() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::G);
        let mut i = r.input(at + egui::vec2(60.0, 0.0));
        i.primary_pressed = true;
        let ev = r.run(i);
        assert!(ev.contains(&ViewportEvent::ModalEnded { committed: true }));
        assert!(r.pos(e).x > 0.3);
    }

    #[test]
    fn modal_rotate_typed_degrees_about_z() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::new(2.0, 0.0, 0.0));
        r.st.selection.select_only(e);
        r.ui.gizmo.pivot = Pivot::Individual;
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::R);
        r.key(at, Key::Z);
        r.text(at, "90");
        r.key(at, Key::Enter);
        let q = r.world.get::<&Transform>(e).unwrap().rotation;
        assert!(q.angle_between(Quat::from_rotation_z(90f32.to_radians())) < 1e-4);
        assert!((r.pos(e) - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-5); // own origin
    }

    #[test]
    fn modal_scale_typed_about_median() {
        let mut r = Rig::new();
        let a = r.cube(Vec3::new(-1.0, 0.0, 0.0));
        let b = r.cube(Vec3::new(1.0, 0.0, 0.0));
        r.st.selection.select_only(a);
        r.st.selection.add(b);
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::S);
        r.text(at, "2");
        let ev = r.key(at, Key::Enter);
        assert!(ev.contains(&ViewportEvent::TransformCommitted { label: "Scale 2 objects".into() }));
        assert!((r.pos(b) - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-4);
        assert_eq!(r.world.get::<&Transform>(a).unwrap().scale, Vec3::splat(2.0));
        assert_eq!(r.st.undo.undo_len(), 1);
    }

    #[test]
    fn selected_parent_and_child_move_once() {
        let mut r = Rig::new();
        let p = r.cube(Vec3::new(1.0, 0.0, 0.0));
        let c = r.world.spawn((Transform::from_position(Vec3::new(0.0, 1.0, 0.0)), Primitive(PrimitiveShape::Cube)));
        reparent(&mut r.world, c, Some(p), None).unwrap();
        r.st.selection.select_only(p);
        r.st.selection.add(c);
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::G);
        r.key(at, Key::X);
        r.text(at, "2");
        r.key(at, Key::Enter);
        assert_eq!(r.pos(p), Vec3::new(3.0, 0.0, 0.0));
        assert_eq!(r.pos(c), Vec3::new(0.0, 1.0, 0.0)); // local unchanged, follows parent
        assert_eq!(r.st.undo.undo_len(), 1);
    }

    #[test]
    fn child_of_rotated_scaled_parent_moves_in_world_space() {
        let mut r = Rig::new();
        let p = r.world.spawn((
            Transform { position: Vec3::new(5.0, 0.0, 0.0), rotation: Quat::from_rotation_y(1.0), scale: Vec3::splat(2.0) },
            Primitive(PrimitiveShape::Cube),
        ));
        let c = r.cube(Vec3::new(0.0, 1.0, 0.0));
        reparent(&mut r.world, c, Some(p), None).unwrap();
        let before = world_transform(&r.world, c).position;
        r.st.selection.select_only(c);
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::G);
        r.key(at, Key::Y);
        r.text(at, "3");
        r.key(at, Key::Enter);
        let after = world_transform(&r.world, c).position;
        assert!((after - (before + Vec3::new(0.0, 3.0, 0.0))).length() < 1e-4, "{before:?} -> {after:?}");
    }

    #[test]
    fn locked_selection_starts_no_modal() {
        let mut r = Rig::new();
        let a = r.world.spawn((Transform::default(), Primitive(PrimitiveShape::Cube), Locked(true)));
        r.st.selection.select_only(a);
        let at = r.scr(Vec3::ZERO);
        assert!(r.key(at, Key::G).is_empty());
        assert!(!r.ui.is_modal_active());
    }

    #[test]
    fn hotkeys_respect_flag_hover_and_play_mode() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        let at = r.scr(Vec3::ZERO);
        let mut i = r.input(at);
        i.keys_pressed.push(Key::G);
        i.modal_hotkeys = false;
        assert!(r.run(i).is_empty());
        let mut i = r.input(at);
        i.keys_pressed.push(Key::G);
        i.hovered = false;
        assert!(r.run(i).is_empty());
        r.st.mode = EditorMode::Play;
        assert!(r.key(at, Key::G).is_empty());
    }

    #[test]
    fn grid_snap_with_ctrl_toggle() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        r.ui.snap = SnapSettings { enabled: false, grid: 1.0, bounds_snap: false, ..Default::default() };
        let at = r.scr(Vec3::ZERO);
        r.key(at, Key::G);
        r.key(at, Key::X);
        let to = r.scr(Vec3::new(1.3, 0.0, 0.0));
        let mut i = r.input(to);
        i.modifiers = Modifiers::CTRL; // ctrl inverts "off" -> on
        r.run(i);
        assert!((r.pos(e).x - 1.0).abs() < 1e-4, "{:?}", r.pos(e));
        r.run(r.input(to));
        assert!((r.pos(e).x - 1.3).abs() < 1e-2);
    }

    #[test]
    fn draw_smoke_with_gizmo_and_modal() {
        let mut r = Rig::new();
        let e = r.cube(Vec3::ZERO);
        r.st.selection.select_only(e);
        let at = r.scr(Vec3::ZERO);
        r.run(r.input(at));
        let ctx = egui::Context::default();
        let cam = r.cam;
        let _ = ctx.run(Default::default(), |ctx| {
            let p = ctx.layer_painter(egui::LayerId::background());
            r.ui.draw(&p, &cam);
        });
        r.key(at, Key::G);
        assert!(r.ui.hud().is_some());
        let _ = ctx.run(Default::default(), |ctx| {
            let p = ctx.layer_painter(egui::LayerId::background());
            r.ui.draw(&p, &cam);
        });
    }
}
