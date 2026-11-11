//! Turning a released asset drag into a `ShellEvent::AssetDropped`.
//!
//! Pure math + egui helpers, so the shell stays engine-agnostic and the
//! pieces are testable without a window.

use std::path::PathBuf;

use egui::{Pos2, Rect};
use glam::{Mat4, Vec3};
use hecs::{Entity, World};

use super::asset_tab::AssetDrag;
use super::cam_matrices::CameraMatrices;
use super::camera::{ray_plane_y, viewport_ray};
use super::picking::pick_nearest;
use super::shell::ShellEvent;

/// Where a drop at `cursor` on the viewport lands on the ground plane
/// (`y = 0`), through the editor camera. `None` if the ray never reaches it
/// (looking up / parallel).
pub fn ground_point(cursor: Pos2, viewport: Rect, view: Mat4, proj: Mat4) -> Option<Vec3> {
    let ray = viewport_ray(cursor, viewport, (proj * view).inverse());
    ray_plane_y(&ray, 0.0)
}

/// The event for dropping `path` at `cursor` in the viewport: the entity
/// under the cursor (nearest pick) if any, and the ground point.
pub fn viewport_drop_event(path: PathBuf, cursor: Pos2, viewport: Rect, view: Mat4, proj: Mat4, world: &World) -> ShellEvent {
    let cam = CameraMatrices::new(view, proj, viewport);
    let target = pick_nearest(world, &cam.ray(cursor)).map(|h| h.entity);
    ShellEvent::AssetDropped { path, target, world_pos: ground_point(cursor, viewport, view, proj) }
}

/// The event for dropping onto a hierarchy row (or empty space: `None`).
pub fn hierarchy_drop_event(path: PathBuf, target: Option<Entity>) -> ShellEvent {
    ShellEvent::AssetDropped { path, target, world_pos: None }
}

fn row_id() -> egui::Id {
    egui::Id::new("asset_drop_hovered_row")
}

/// Called by each hierarchy row while drawing: remembers the row under the
/// pointer so a release this frame can name its entity, and outlines it.
pub fn note_hierarchy_row(ui: &egui::Ui, row: Rect, entity: Entity, pointer: Option<Pos2>) {
    if !super::asset_tab::is_dragging_asset(ui.ctx()) {
        return;
    }
    if pointer.is_some_and(|p| row.contains(p)) {
        ui.ctx().data_mut(|d| d.insert_temp(row_id(), entity));
        let stroke = egui::Stroke::new(2.0_f32, ui.visuals().selection.stroke.color);
        ui.painter().rect_stroke(row, 2.0, stroke);
    }
}

/// Draws the hierarchy (via `draw`) and reports an asset released over it,
/// with the row it was released on, if any.
pub fn with_hierarchy_drop(ui: &mut egui::Ui, draw: impl FnOnce(&mut egui::Ui)) -> Option<(AssetDrag, Option<Entity>)> {
    ui.ctx().data_mut(|d| d.remove::<Entity>(row_id()));
    let area = ui.max_rect();
    draw(ui);
    let ctx = ui.ctx().clone();
    if !super::asset_tab::is_dragging_asset(&ctx) {
        return None;
    }
    let (released, pointer) = ctx.input(|i| (i.pointer.any_released(), i.pointer.interact_pos()));
    if !released || !pointer.is_some_and(|p| area.contains(p)) {
        return None;
    }
    let drag = egui::DragAndDrop::take_payload::<AssetDrag>(&ctx)?;
    let target = ctx.data(|d| d.get_temp::<Entity>(row_id()));
    Some(((*drag).clone(), target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::camera::EditorCamera;
    use crate::ecs::Transform;

    fn view_proj(cam: &EditorCamera, rect: Rect) -> (Mat4, Mat4) {
        (cam.view_matrix(), cam.projection_matrix(rect.width() / rect.height()))
    }

    #[test]
    fn centre_of_viewport_hits_the_ground_under_the_camera_target() {
        let cam = EditorCamera::default();
        let rect = Rect::from_min_size(Pos2::new(100.0, 50.0), egui::vec2(800.0, 600.0));
        let (v, p) = view_proj(&cam, rect);
        let hit = ground_point(rect.center(), rect, v, p);
        let hit = hit.expect("default camera looks down at the ground");
        assert!(hit.y.abs() < 1e-3, "{hit:?}");
    }

    #[test]
    fn ray_that_misses_the_ground_gives_no_position() {
        let cam = EditorCamera::default();
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let (v, p) = view_proj(&cam, rect);
        // Top edge looks at the sky for a camera pitched near the horizon;
        // either way the helper must not panic and a hit must lie at y = 0.
        if let Some(h) = ground_point(Pos2::new(400.0, 0.0), rect, v, p) {
            assert!(h.y.abs() < 1e-3);
        }
    }

    #[test]
    fn viewport_drop_picks_entity_under_cursor() {
        let mut world = World::new();
        let e = world.spawn((Transform::default(),));
        let cam = EditorCamera::default();
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let (v, p) = view_proj(&cam, rect);
        // Project the entity's origin to a pixel and drop there.
        let clip = (p * v) * glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
        let ndc = clip.truncate() / clip.w;
        let px = Pos2::new(rect.left() + (ndc.x * 0.5 + 0.5) * rect.width(), rect.top() + (0.5 - ndc.y * 0.5) * rect.height());
        let ev = viewport_drop_event("a/b.png".into(), px, rect, v, p, &world);
        match ev {
            ShellEvent::AssetDropped { path, target, world_pos } => {
                assert_eq!(path, PathBuf::from("a/b.png"));
                assert_eq!(target, Some(e));
                assert!(world_pos.is_some());
            }
            other => panic!("{other:?}"),
        }
        // A drop far from anything has no target but still a ground point.
        let ev = viewport_drop_event("a/b.png".into(), Pos2::new(5.0, 590.0), rect, v, p, &world);
        assert!(matches!(ev, ShellEvent::AssetDropped { target: None, world_pos: Some(_), .. }), "{ev:?}");
    }

    #[test]
    fn hierarchy_event_has_no_world_pos() {
        let ev = hierarchy_drop_event("x.glb".into(), None);
        assert_eq!(ev, ShellEvent::AssetDropped { path: "x.glb".into(), target: None, world_pos: None });
    }

    // ---- whole-shell drops from a fake drag ------------------------------

    use crate::editor::shell::{EditorShell, ShellHost};
    use crate::editor::state::EditorState;

    struct Open;
    impl ShellHost for Open {}

    struct Refuse;
    impl ShellHost for Refuse {
        fn accepts_asset_drop(&mut self, _path: &std::path::Path) -> bool {
            false
        }
    }

    fn shell_frame(ctx: &egui::Context, shell: &mut EditorShell, ed: &mut EditorState, world: &mut World, host: &mut dyn ShellHost, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 720.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            shell.ui(ctx, ed, world, host);
        });
    }

    fn release(pos: Pos2) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }
    }

    /// Drags a fake asset to `pos` and releases it there; returns shell events.
    fn fake_drop(shell: &mut EditorShell, world: &mut World, host: &mut dyn ShellHost, pos: Pos2) -> Vec<ShellEvent> {
        let ctx = egui::Context::default();
        let mut ed = EditorState::new();
        shell_frame(&ctx, shell, &mut ed, world, host, vec![]);
        shell_frame(&ctx, shell, &mut ed, world, host, vec![]);
        shell.take_events();
        egui::DragAndDrop::set_payload(&ctx, AssetDrag { path: "assets/crate.glb".into() });
        shell_frame(&ctx, shell, &mut ed, world, host, vec![egui::Event::PointerMoved(pos)]);
        assert!(crate::editor::asset_tab::is_dragging_asset(&ctx), "payload survives until release");
        shell_frame(&ctx, shell, &mut ed, world, host, vec![release(pos)]);
        assert!(!crate::editor::asset_tab::is_dragging_asset(&ctx), "payload consumed or cleared by the release");
        shell.take_events()
    }

    #[test]
    fn dropping_on_the_viewport_emits_asset_dropped_with_ground_position() {
        let mut shell = EditorShell::new(vec![]);
        let mut world = World::new();
        // Lay out once to learn where the viewport is.
        let ctx = egui::Context::default();
        shell_frame(&ctx, &mut shell, &mut EditorState::new(), &mut world, &mut Open, vec![]);
        shell_frame(&ctx, &mut shell, &mut EditorState::new(), &mut world, &mut Open, vec![]);
        let rect = shell.viewport.rect.expect("viewport laid out");
        let events = fake_drop(&mut shell, &mut world, &mut Open, rect.center());
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            ShellEvent::AssetDropped { path, target, world_pos } => {
                assert_eq!(path, &PathBuf::from("assets/crate.glb"));
                assert_eq!(*target, None);
                let p = world_pos.expect("default camera looks at the ground");
                assert!(p.y.abs() < 1e-3);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_host_can_refuse_drops() {
        let mut shell = EditorShell::new(vec![]);
        let mut world = World::new();
        let ctx = egui::Context::default();
        shell_frame(&ctx, &mut shell, &mut EditorState::new(), &mut world, &mut Refuse, vec![]);
        shell_frame(&ctx, &mut shell, &mut EditorState::new(), &mut world, &mut Refuse, vec![]);
        let rect = shell.viewport.rect.unwrap();
        let events = fake_drop(&mut shell, &mut world, &mut Refuse, rect.center());
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn dropping_on_the_hierarchy_names_the_row_entity() {
        let mut shell = EditorShell::new(vec![]);
        let mut world = World::new();
        let e = world.spawn((Transform::default(), crate::editor::hierarchy::Name("Crate".into())));
        let ctx = egui::Context::default();
        let mut ed = EditorState::new();
        shell_frame(&ctx, &mut shell, &mut ed, &mut world, &mut Open, vec![]);
        shell_frame(&ctx, &mut shell, &mut ed, &mut world, &mut Open, vec![]);
        let vp = shell.viewport.rect.unwrap();
        // The hierarchy is left of the viewport: sweep down its first column.
        let mut got = None;
        for y in (40..400).step_by(6) {
            let pos = Pos2::new(vp.left() * 0.5, y as f32);
            for ev in fake_drop(&mut shell, &mut world, &mut Open, pos) {
                if let ShellEvent::AssetDropped { target: Some(t), world_pos, .. } = ev {
                    assert_eq!(world_pos, None);
                    got = Some(t);
                }
            }
            if got.is_some() {
                break;
            }
        }
        assert_eq!(got, Some(e), "some hierarchy row reports the entity");
        // Empty hierarchy space still drops, without a target.
        let ev = fake_drop(&mut shell, &mut world, &mut Open, Pos2::new(vp.left() * 0.5, 600.0));
        assert!(matches!(ev.as_slice(), [ShellEvent::AssetDropped { target: None, world_pos: None, .. }]), "{ev:?}");
    }
}
