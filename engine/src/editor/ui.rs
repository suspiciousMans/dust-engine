//! egui front-end: hierarchy tree and inspector.
//!
//! All behaviour that can be tested without a pointer lives in plain
//! functions at the top ([`visible_rows`], [`click_select`], [`perform_drop`],
//! [`set_field_on_selection`]); the `draw_*` functions are thin glue over them.

use egui::{Align, Color32, Key, Layout, RichText, Sense, Ui};
use hecs::{Entity, World};

use super::command::*;
use super::hierarchy::{self, display_name, is_descendant_of, is_locked, is_visible, parent_of, sibling_index, Locked, PrimitiveShape, Visible};
use super::inspector::{ComponentDesc, FieldKind, Value};
use super::state::EditorState;

/// Drag payload an asset browser can use to drop a path onto an `AssetPath` field.
#[derive(Clone, Debug)]
pub struct AssetPathPayload(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    Before,
    Inside,
    After,
}

impl DropZone {
    /// Top quarter of the row = before, bottom quarter = after, rest = inside.
    pub fn from_fraction(f: f32) -> Self {
        if f < 0.25 {
            Self::Before
        } else if f > 0.75 {
            Self::After
        } else {
            Self::Inside
        }
    }
}

pub fn name_matches(world: &World, e: Entity, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || display_name(world, e).to_lowercase().contains(&q)
}

/// Rows to display: the tree honouring `collapsed`, or a flat match list
/// while searching. `(entity, depth)`.
pub fn visible_rows(world: &World, state: &EditorState) -> Vec<(Entity, usize)> {
    let mut out = Vec::new();
    if !state.search.trim().is_empty() {
        for r in hierarchy::roots(world) {
            for e in hierarchy::subtree(world, r) {
                if name_matches(world, e, &state.search) {
                    out.push((e, 0));
                }
            }
        }
        return out;
    }
    fn walk(world: &World, state: &EditorState, e: Entity, depth: usize, out: &mut Vec<(Entity, usize)>) {
        out.push((e, depth));
        if !state.collapsed.contains(&e) {
            for c in hierarchy::children(world, e) {
                walk(world, state, c, depth + 1, out);
            }
        }
    }
    for r in hierarchy::roots(world) {
        walk(world, state, r, 0, &mut out);
    }
    out
}

/// Click semantics: plain = select only, Ctrl/Cmd = toggle, Shift = range
/// over `visible_order`.
pub fn click_select(state: &mut EditorState, visible_order: &[Entity], e: Entity, mods: egui::Modifiers) {
    if mods.shift {
        state.selection.select_range(visible_order, e);
    } else if mods.ctrl || mods.command {
        state.selection.toggle(e);
    } else {
        state.selection.select_only(e);
    }
}

/// Drops `dragged` (or the whole selection if `dragged` is part of it) on
/// `target`. One undo step. Returns how many entities were moved.
pub fn perform_drop(
    state: &mut EditorState,
    world: &mut World,
    dragged: Entity,
    target: Entity,
    zone: DropZone,
) -> Result<usize, String> {
    let mut list = if state.selection.contains(dragged) {
        state.selection.top_level(world)
    } else {
        vec![dragged]
    };
    // Can't drop something on itself or into its own subtree.
    list.retain(|d| *d != target && !is_descendant_of(world, target, *d) && !is_locked(world, *d));
    if list.is_empty() || !world.contains(target) {
        return Err("nothing to move".into());
    }
    let label = if list.len() == 1 { "Reparent".to_string() } else { format!("Move {} objects", list.len()) };
    let moved = state.transaction(&label, |s| {
        let mut placed = 0usize;
        for d in &list {
            let (parent, index) = match zone {
                DropZone::Inside => (Some(target), None),
                DropZone::Before | DropZone::After => {
                    let ti = sibling_index(world, target).unwrap_or(0);
                    let same = parent_of(world, *d) == parent_of(world, target);
                    let ti = match sibling_index(world, *d) {
                        Some(di) if same && di < ti => ti - 1,
                        _ => ti,
                    };
                    let idx = if zone == DropZone::After { ti + 1 + placed } else { ti };
                    (parent_of(world, target), Some(idx))
                }
            };
            if s.exec(world, ReparentCommand::new(*d, parent, index)).is_ok() {
                placed += 1;
            }
        }
        placed
    });
    if zone == DropZone::Inside {
        state.collapsed.remove(&target);
    }
    if moved == 0 {
        Err(state.last_error.clone().unwrap_or_else(|| "move failed".into()))
    } else {
        Ok(moved)
    }
}

/// Entities an inspector edit applies to: the whole selection that still exists.
pub fn edit_targets(state: &EditorState, world: &World) -> Vec<Entity> {
    state.selection.iter().filter(|e| world.contains(*e)).collect()
}

/// What the inspector does when a field widget changes: one (mergeable)
/// command over every selected entity that has the component.
pub fn set_field_on_selection(
    state: &mut EditorState,
    world: &mut World,
    comp: &'static str,
    field: &'static str,
    value: Value,
) -> Result<(), String> {
    let desc = state.registry.find(comp).ok_or_else(|| format!("unknown component {comp}"))?.clone();
    let targets: Vec<Entity> = edit_targets(state, world)
        .into_iter()
        .filter(|e| desc.has(world, *e) && !is_locked(world, *e))
        .collect();
    state.exec(world, SetFieldCommand::new(targets, comp, field, value))
}

/// Does `field` hold different values across the selected entities?
pub fn field_is_mixed(state: &EditorState, world: &World, comp: &ComponentDesc, field: &str) -> bool {
    let Some(f) = comp.field(field) else { return false };
    let mut first: Option<Value> = None;
    for e in state.selection.iter() {
        if let Some(v) = f.get(world, e) {
            match &first {
                None => first = Some(v),
                Some(p) if *p != v => return true,
                _ => {}
            }
        }
    }
    false
}

// --------------------------------------------------------------- shortcuts

/// Feeds this frame's key presses to the keymap. Skipped while a text field
/// has keyboard focus.
pub fn handle_shortcuts(ctx: &egui::Context, state: &mut EditorState, world: &mut World) {
    if ctx.wants_keyboard_input() {
        return;
    }
    let keys: Vec<(Key, egui::Modifiers)> = ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => Some((*key, *modifiers)),
                _ => None,
            })
            .collect()
    });
    for (k, m) in keys {
        state.handle_key(world, k, m);
    }
}

// ------------------------------------------------------------------ helpers

fn release_merge_if_pointer_up(ui: &Ui, state: &mut EditorState) {
    if state.merge_pending && !ui.input(|i| i.pointer.any_down()) {
        state.undo.seal();
        state.merge_pending = false;
    }
}

/// Call after executing a mergeable edit from a widget.
fn after_edit(ui: &Ui, state: &mut EditorState) {
    if ui.input(|i| i.pointer.any_down()) {
        state.merge_pending = true; // dragging: keep merging until release
    } else if !ui.ctx().wants_keyboard_input() {
        state.undo.seal(); // discrete change (checkbox, combo, ...)
    }
}

fn end_of_interaction(state: &mut EditorState, r: &egui::Response) {
    if r.lost_focus() || r.drag_stopped() {
        state.undo.seal();
    }
}

fn create_menu(ui: &mut Ui, state: &mut EditorState, world: &mut World, parent: Option<Entity>) {
    if ui.button("Create Empty").clicked() {
        state.create_empty(world, parent);
        ui.close_menu();
    }
    ui.menu_button("Create Primitive", |ui| {
        for shape in PrimitiveShape::ALL {
            if ui.button(shape.label()).clicked() {
                state.create_primitive(world, shape, parent);
                ui.close_menu();
            }
        }
    });
}

// ---------------------------------------------------------------- hierarchy

pub fn draw_hierarchy(ui: &mut Ui, state: &mut EditorState, world: &mut World) {
    state.selection.prune(world);
    release_merge_if_pointer_up(ui, state);

    ui.horizontal(|ui| {
        let w = (ui.available_width() - 34.0).max(40.0);
        ui.add(egui::TextEdit::singleline(&mut state.search).hint_text("Search…").desired_width(w));
        if !state.search.is_empty() && ui.small_button("✖").on_hover_text("Clear search").clicked() {
            state.search.clear();
        }
        ui.menu_button("＋", |ui| create_menu(ui, state, world, None));
    });
    ui.separator();

    let rows = visible_rows(world, state);
    let order: Vec<Entity> = rows.iter().map(|r| r.0).collect();
    let searching = !state.search.trim().is_empty();
    let dragging: Option<Entity> = egui::DragAndDrop::payload::<Entity>(ui.ctx()).map(|p| *p);
    let released = ui.input(|i| i.pointer.any_released());
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let mut pending_drop: Option<(Entity, Entity, DropZone)> = None;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (e, depth) in &rows {
            if let Some(d) = draw_row(ui, state, world, *e, *depth, searching, &order, dragging, pointer) {
                if released {
                    if let Some(dr) = dragging {
                        pending_drop = Some((dr, *e, d));
                    }
                }
            }
        }

        // Empty space: deselect on click, create menu on right click,
        // dropping here moves the dragged entity to the end of the root list.
        let rest = ui.available_size().max(egui::vec2(10.0, 40.0));
        let (rect, resp) = ui.allocate_exact_size(rest, Sense::click());
        if resp.clicked() {
            state.selection.clear();
        }
        resp.context_menu(|ui| create_menu(ui, state, world, None));
        if let (Some(dr), true) = (dragging, released) {
            if pointer.map(|p| rect.contains(p)).unwrap_or(false) {
                let _ = state.exec(world, ReparentCommand::new(dr, None, None));
            }
        }
    });

    if let Some((dragged, target, zone)) = pending_drop {
        if let Err(e) = perform_drop(state, world, dragged, target, zone) {
            state.last_error = Some(e);
        }
    }
    if rows.is_empty() {
        ui.weak(if searching { "No matches" } else { "Scene is empty — use ＋ to create an object" });
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ui: &mut Ui,
    state: &mut EditorState,
    world: &mut World,
    e: Entity,
    depth: usize,
    searching: bool,
    order: &[Entity],
    dragging: Option<Entity>,
    pointer: Option<egui::Pos2>,
) -> Option<DropZone> {
    let selected = state.selection.contains(e);
    let hidden = !is_visible(world, e);
    let locked = is_locked(world, e);
    let has_kids = !searching && !hierarchy::children(world, e).is_empty();
    let own_visible = world.get::<&Visible>(e).map(|v| v.0).unwrap_or(true);
    let own_locked = world.get::<&Locked>(e).map(|v| v.0).unwrap_or(false);
    let mut drop_zone = None;

    let row = ui.horizontal(|ui| {
        let flag_targets = |state: &EditorState| if selected { state.selection.to_vec() } else { vec![e] };
        let eye = if own_visible { "👁" } else { "–" };
        if ui.small_button(eye).on_hover_text("Show / hide").clicked() {
            let t = flag_targets(state);
            let _ = state.exec(world, SetFlagCommand::new(t, Flag::Visible, !own_visible));
        }
        let lock = if own_locked { "🔒" } else { "🔓" };
        if ui.small_button(lock).on_hover_text("Lock / unlock (locked objects can't be edited or deleted)").clicked() {
            let t = flag_targets(state);
            let _ = state.exec(world, SetFlagCommand::new(t, Flag::Locked, !own_locked));
        }

        ui.add_space(depth as f32 * 14.0);
        if has_kids {
            let open = !state.collapsed.contains(&e);
            if ui.small_button(if open { "▼" } else { "▶" }).clicked() {
                if open {
                    state.collapsed.insert(e);
                } else {
                    state.collapsed.remove(&e);
                }
            }
        } else {
            ui.add_space(ui.spacing().interact_size.y);
        }

        if state.rename.as_ref().map(|r| r.entity) == Some(e) {
            let mut commit: Option<(String, bool)> = None;
            if let Some(r) = state.rename.as_mut() {
                let te = ui.add(egui::TextEdit::singleline(&mut r.buffer).desired_width(f32::INFINITY));
                if !r.focus_requested {
                    te.request_focus();
                    r.focus_requested = true;
                }
                let esc = ui.input(|i| i.key_pressed(Key::Escape));
                if esc || te.lost_focus() {
                    commit = Some((r.buffer.clone(), esc));
                }
            }
            if let Some((text, cancelled)) = commit {
                state.rename = None;
                let text = text.trim().to_string();
                if !cancelled && !text.is_empty() && text != display_name(world, e) {
                    let _ = state.exec(world, RenameCommand::new(e, text));
                    state.undo.seal();
                }
            }
            return;
        }

        let mut text = RichText::new(display_name(world, e));
        if hidden {
            text = text.color(Color32::GRAY).italics();
        }
        if locked {
            text = text.weak();
        }
        let resp = ui
            .with_layout(Layout::left_to_right(Align::Center).with_main_justify(true), |ui| {
                ui.selectable_label(selected, text).interact(Sense::click_and_drag())
            })
            .inner;

        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            click_select(state, order, e, mods);
        }
        if resp.double_clicked() {
            state.begin_rename(world, e);
        }
        if resp.secondary_clicked() && !selected {
            state.selection.select_only(e);
        }
        if resp.drag_started() && !locked {
            if !selected {
                state.selection.select_only(e);
            }
            egui::DragAndDrop::set_payload(ui.ctx(), e);
        }
        resp.context_menu(|ui| {
            if ui.button("Rename   F2").clicked() {
                state.begin_rename(world, e);
                ui.close_menu();
            }
            if ui.button("Duplicate").clicked() {
                state.duplicate_selection(world);
                ui.close_menu();
            }
            if ui.button("Delete").clicked() {
                state.delete_selection(world);
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Create Empty Child").clicked() {
                state.create_empty(world, Some(e));
                ui.close_menu();
            }
            ui.menu_button("Create Primitive Child", |ui| {
                for shape in PrimitiveShape::ALL {
                    if ui.button(shape.label()).clicked() {
                        state.create_primitive(world, shape, Some(e));
                        ui.close_menu();
                    }
                }
            });
        });
    });

    super::asset_drop::note_hierarchy_row(ui, row.response.rect, e, pointer);
    // Drop indicator / target detection over the whole row.
    if let (Some(dr), Some(p)) = (dragging, pointer) {
        let rect = row.response.rect;
        if rect.contains(p) && dr != e {
            let zone = DropZone::from_fraction((p.y - rect.top()) / rect.height().max(1.0));
            let stroke = egui::Stroke::new(2.0_f32, ui.visuals().selection.stroke.color);
            match zone {
                DropZone::Before => {
                    ui.painter().hline(rect.x_range(), rect.top(), stroke);
                }
                DropZone::After => {
                    ui.painter().hline(rect.x_range(), rect.bottom(), stroke);
                }
                DropZone::Inside => {
                    ui.painter().rect_stroke(rect, 2.0, stroke);
                }
            }
            drop_zone = Some(zone);
        }
    }
    drop_zone
}

// ---------------------------------------------------------------- inspector

pub fn draw_inspector(ui: &mut Ui, state: &mut EditorState, world: &mut World) {
    state.selection.prune(world);
    release_merge_if_pointer_up(ui, state);

    let targets = edit_targets(state, world);
    let Some(primary) = state.selection.primary().filter(|p| world.contains(*p)) else {
        ui.weak("Nothing selected");
        return;
    };
    let multi = targets.len() > 1;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // --- header: name + flags
        ui.horizontal(|ui| {
            if multi {
                ui.strong(format!("{} objects selected", targets.len()));
                ui.weak("— edits apply to all");
            } else {
                let mut name = display_name(world, primary);
                let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY));
                if r.changed() && !is_locked(world, primary) {
                    let _ = state.exec(world, RenameCommand::new(primary, name));
                    after_edit(ui, state);
                }
                end_of_interaction(state, &r);
            }
        });
        ui.horizontal(|ui| {
            let mut vis = world.get::<&Visible>(primary).map(|v| v.0).unwrap_or(true);
            if ui.checkbox(&mut vis, "Visible").changed() {
                let _ = state.exec(world, SetFlagCommand::new(targets.clone(), Flag::Visible, vis));
            }
            let mut lock = world.get::<&Locked>(primary).map(|v| v.0).unwrap_or(false);
            if ui.checkbox(&mut lock, "Locked").changed() {
                let _ = state.exec(world, SetFlagCommand::new(targets.clone(), Flag::Locked, lock));
            }
        });
        if is_locked(world, primary) {
            ui.colored_label(Color32::YELLOW, "Locked — unlock to edit components");
        }
        ui.separator();

        // --- components
        let comps: Vec<ComponentDesc> = state.registry.present(world, primary).into_iter().cloned().collect();
        for comp in comps {
            let mut remove = false;
            let header = egui::CollapsingHeader::new(RichText::new(comp.name).strong())
                .default_open(true)
                .id_salt(("inspector_comp", comp.name))
                .show(ui, |ui| {
                    ui.add_enabled_ui(!is_locked(world, primary), |ui| {
                        egui::Grid::new(("inspector_grid", comp.name)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                            for f in &comp.fields {
                                draw_field(ui, state, world, &comp, f.name, primary);
                                ui.end_row();
                            }
                        });
                        if comp.removable && ui.small_button("Remove component").clicked() {
                            remove = true;
                        }
                    });
                });
            header.header_response.context_menu(|ui| {
                if comp.removable && ui.button("Remove component").clicked() {
                    remove = true;
                    ui.close_menu();
                }
            });
            if remove {
                let _ = state.exec(world, RemoveComponentCommand::new(targets.clone(), comp.name));
                state.undo.seal();
            }
        }

        ui.separator();
        ui.menu_button("Add Component", |ui| {
            ui.add(egui::TextEdit::singleline(&mut state.add_component_filter).hint_text("Search…"));
            let names: Vec<&'static str> = state
                .registry
                .addable(world, primary, &state.add_component_filter)
                .iter()
                .map(|c| c.name)
                .collect();
            if names.is_empty() {
                ui.weak("Nothing to add");
            }
            for n in names {
                if ui.button(n).clicked() {
                    let _ = state.exec(world, AddComponentCommand::new(targets.clone(), n));
                    state.undo.seal();
                    state.add_component_filter.clear();
                    ui.close_menu();
                }
            }
        });
    });
}

fn draw_field(
    ui: &mut Ui,
    state: &mut EditorState,
    world: &mut World,
    comp: &ComponentDesc,
    field_name: &'static str,
    primary: Entity,
) {
    let Some(field) = comp.field(field_name) else { return };
    let Some(mut value) = field.get(world, primary) else { return };
    let mixed = field_is_mixed(state, world, comp, field_name);

    let label = if mixed { RichText::new(format!("{} ≠", field.name)).italics() } else { RichText::new(field.name) };
    let l = ui.label(label);
    if !field.tooltip.is_empty() {
        l.on_hover_text(field.tooltip);
    } else if mixed {
        l.on_hover_text("Values differ across the selection; editing sets all of them");
    }

    let resp = edit_value(ui, (comp.name, field_name), &field.kind, &mut value);
    let resp = if field.tooltip.is_empty() { resp } else { resp.on_hover_text(field.tooltip) };
    if resp.changed() {
        let _ = set_field_on_selection(state, world, comp.name, field.name, value);
        after_edit(ui, state);
    }
    end_of_interaction(state, &resp);
}

/// Draws the widget for one value; the returned response's `changed()` says
/// whether `value` was edited.
fn edit_value(ui: &mut Ui, id: (&'static str, &'static str), kind: &FieldKind, value: &mut Value) -> egui::Response {
    match (kind, value) {
        (FieldKind::F32 { range, speed }, Value::F32(x)) => match range {
            Some((a, b)) => ui.add(egui::Slider::new(x, *a..=*b)),
            None => ui.add(egui::DragValue::new(x).speed(*speed)),
        },
        (FieldKind::Vec3, Value::Vec3(v)) => {
            ui.horizontal(|ui| {
                let x = ui.add(egui::DragValue::new(&mut v.x).speed(0.05).prefix("x "));
                let y = ui.add(egui::DragValue::new(&mut v.y).speed(0.05).prefix("y "));
                let z = ui.add(egui::DragValue::new(&mut v.z).speed(0.05).prefix("z "));
                x | y | z
            })
            .inner
        }
        (FieldKind::Bool, Value::Bool(b)) => ui.checkbox(b, ""),
        (FieldKind::String, Value::Str(s)) => ui.add(egui::TextEdit::singleline(s).desired_width(f32::INFINITY)),
        (FieldKind::AssetPath, Value::AssetPath(s)) => {
            let mut r = ui.add(egui::TextEdit::singleline(s).hint_text("assets/…").desired_width(f32::INFINITY));
            if r.hovered() {
                if let Some(p) = egui::DragAndDrop::payload::<AssetPathPayload>(ui.ctx()) {
                    if ui.input(|i| i.pointer.any_released()) {
                        *s = p.0.clone();
                        r.mark_changed();
                    }
                }
            }
            r
        }
        (FieldKind::Color, Value::Color(c)) => ui.color_edit_button_rgb(c),
        (FieldKind::Enum(variants), Value::Enum(i)) => {
            let mut changed = false;
            let current = variants.get(*i).copied().unwrap_or("?");
            let mut r = egui::ComboBox::from_id_salt(id).selected_text(current).show_ui(ui, |ui| {
                for (n, v) in variants.iter().enumerate() {
                    if ui.selectable_value(i, n, *v).clicked() {
                        changed = true;
                    }
                }
            });
            if changed {
                r.response.mark_changed();
            }
            r.response
        }
        _ => ui.weak("(unsupported)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::{Camera, Transform};
    use crate::editor::hierarchy::*;
    use glam::Vec3;

    fn setup() -> (World, EditorState, Entity, Entity, Entity, Entity) {
        // a { b { c } }, d
        let mut w = World::new();
        let mut s = EditorState::new();
        let a = s.create_empty(&mut w, None).unwrap();
        let b = s.create_empty(&mut w, Some(a)).unwrap();
        let c = s.create_empty(&mut w, Some(b)).unwrap();
        let d = s.create_empty(&mut w, None).unwrap();
        for (e, n) in [(a, "a"), (b, "b"), (c, "c"), (d, "d")] {
            w.insert_one(e, Name(n.into())).unwrap();
        }
        s.selection.clear();
        s.undo.clear();
        (w, s, a, b, c, d)
    }

    fn names(w: &World, rows: &[(Entity, usize)]) -> Vec<(String, usize)> {
        rows.iter().map(|(e, d)| (display_name(w, *e), *d)).collect()
    }

    #[test]
    fn rows_follow_tree_and_collapse() {
        let (w, mut s, a, ..) = setup();
        assert_eq!(
            names(&w, &visible_rows(&w, &s)),
            [("a".into(), 0), ("b".into(), 1), ("c".into(), 2), ("d".into(), 0)]
        );
        s.collapsed.insert(a);
        assert_eq!(names(&w, &visible_rows(&w, &s)), [("a".into(), 0), ("d".into(), 0)]);
    }

    #[test]
    fn search_is_flat_case_insensitive_and_ignores_collapse() {
        let (mut w, mut s, a, b, ..) = setup();
        w.insert_one(b, Name("Bobby".into())).unwrap();
        s.collapsed.insert(a);
        s.search = "BOB".into();
        assert_eq!(names(&w, &visible_rows(&w, &s)), [("Bobby".into(), 0)]);
        s.search = "zzz".into();
        assert!(visible_rows(&w, &s).is_empty());
    }

    #[test]
    fn click_select_modes() {
        let (w, mut s, a, b, c, d) = setup();
        let order: Vec<_> = visible_rows(&w, &s).into_iter().map(|r| r.0).collect();
        click_select(&mut s, &order, a, egui::Modifiers::NONE);
        click_select(&mut s, &order, c, egui::Modifiers::CTRL);
        assert_eq!(s.selection.to_vec(), vec![a, c]);
        click_select(&mut s, &order, d, egui::Modifiers::SHIFT);
        // range from primary (c) to d
        assert!(s.selection.contains(c) && s.selection.contains(d));
        click_select(&mut s, &order, b, egui::Modifiers::NONE);
        assert_eq!(s.selection.to_vec(), vec![b]);
    }

    #[test]
    fn drop_inside_reparents_and_undoes() {
        let (mut w, mut s, a, b, c, d) = setup();
        assert_eq!(perform_drop(&mut s, &mut w, d, a, DropZone::Inside), Ok(1));
        assert_eq!(children(&w, a), vec![b, d]);
        s.undo(&mut w);
        assert_eq!(children(&w, a), vec![b]);
        assert_eq!(roots(&w), vec![a, d]);
        let _ = c;
    }

    #[test]
    fn drop_before_and_after_reorder_siblings() {
        let (mut w, mut s, a, b, _c, d) = setup();
        perform_drop(&mut s, &mut w, d, a, DropZone::Before).unwrap();
        assert_eq!(roots(&w), vec![d, a]);
        perform_drop(&mut s, &mut w, d, a, DropZone::After).unwrap();
        assert_eq!(roots(&w), vec![a, d]);
        // into another parent between siblings
        let x = s.create_empty(&mut w, Some(a)).unwrap();
        assert_eq!(children(&w, a), vec![b, x]);
        perform_drop(&mut s, &mut w, d, b, DropZone::After).unwrap();
        assert_eq!(children(&w, a), vec![b, d, x]);
        perform_drop(&mut s, &mut w, x, b, DropZone::Before).unwrap();
        assert_eq!(children(&w, a), vec![x, b, d]);
    }

    #[test]
    fn drop_same_parent_moving_down_accounts_for_removal() {
        let (mut w, mut s, a, b, ..) = setup();
        let x = s.create_empty(&mut w, Some(a)).unwrap();
        let y = s.create_empty(&mut w, Some(a)).unwrap();
        assert_eq!(children(&w, a), vec![b, x, y]);
        perform_drop(&mut s, &mut w, b, y, DropZone::After).unwrap();
        assert_eq!(children(&w, a), vec![x, y, b]);
        perform_drop(&mut s, &mut w, b, x, DropZone::Before).unwrap();
        assert_eq!(children(&w, a), vec![b, x, y]);
    }

    #[test]
    fn drop_into_own_subtree_is_rejected() {
        let (mut w, mut s, a, _b, c, _d) = setup();
        assert!(perform_drop(&mut s, &mut w, a, c, DropZone::Inside).is_err());
        assert!(perform_drop(&mut s, &mut w, a, a, DropZone::Inside).is_err());
        assert_eq!(s.undo.undo_len(), 0);
        assert_eq!(parent_of(&w, a), None);
    }

    #[test]
    fn dragging_a_selected_entity_moves_whole_selection_in_order_as_one_step() {
        let (mut w, mut s, a, _b, _c, d) = setup();
        let e1 = s.create_empty(&mut w, None).unwrap();
        let e2 = s.create_empty(&mut w, None).unwrap();
        s.selection.clear();
        s.selection.add(e1);
        s.selection.add(e2);
        let n = s.undo.undo_len();
        assert_eq!(perform_drop(&mut s, &mut w, e1, a, DropZone::After), Ok(2));
        assert_eq!(s.undo.undo_len(), n + 1);
        assert_eq!(roots(&w), vec![a, e1, e2, d]);
        s.undo(&mut w);
        assert_eq!(roots(&w), vec![a, d, e1, e2]);
    }

    #[test]
    fn drop_zone_thresholds() {
        assert_eq!(DropZone::from_fraction(0.1), DropZone::Before);
        assert_eq!(DropZone::from_fraction(0.5), DropZone::Inside);
        assert_eq!(DropZone::from_fraction(0.9), DropZone::After);
    }

    #[test]
    fn inspector_edit_applies_to_all_selected_with_component_and_is_one_step() {
        let (mut w, mut s, a, b, c, _d) = setup();
        for e in [a, b] {
            w.insert_one(e, Transform::default()).unwrap();
        }
        s.selection.add(a);
        s.selection.add(b);
        s.selection.add(c); // no Transform: skipped, not an error
        for x in 1..=5 {
            set_field_on_selection(&mut s, &mut w, "Transform", "position", Value::Vec3(Vec3::splat(x as f32))).unwrap();
        }
        assert_eq!(s.undo.undo_len(), 1, "slider drag merged");
        assert_eq!(w.get::<&Transform>(a).unwrap().position, Vec3::splat(5.0));
        assert_eq!(w.get::<&Transform>(b).unwrap().position, Vec3::splat(5.0));
        s.undo(&mut w);
        assert_eq!(w.get::<&Transform>(a).unwrap().position, Vec3::ZERO);
    }

    #[test]
    fn inspector_edit_skips_locked_entities() {
        let (mut w, mut s, a, b, ..) = setup();
        for e in [a, b] {
            w.insert_one(e, Transform::default()).unwrap();
        }
        w.insert_one(b, Locked(true)).unwrap();
        s.selection.add(a);
        s.selection.add(b);
        set_field_on_selection(&mut s, &mut w, "Transform", "scale", Value::Vec3(Vec3::splat(2.0))).unwrap();
        assert_eq!(w.get::<&Transform>(a).unwrap().scale, Vec3::splat(2.0));
        assert_eq!(w.get::<&Transform>(b).unwrap().scale, Vec3::ONE);
    }

    #[test]
    fn mixed_detection() {
        let (mut w, mut s, a, b, ..) = setup();
        w.insert_one(a, Camera::default()).unwrap();
        w.insert_one(b, Camera::default()).unwrap();
        s.selection.add(a);
        s.selection.add(b);
        let comp = s.registry.find("Camera").unwrap().clone();
        assert!(!field_is_mixed(&s, &w, &comp, "near"));
        w.get::<&mut Camera>(b).unwrap().near = 3.0;
        assert!(field_is_mixed(&s, &w, &comp, "near"));
        assert!(!field_is_mixed(&s, &w, &comp, "far"));
    }

    // ---- headless egui smoke tests (no GL / SDL)

    fn run_frame(f: impl FnMut(&mut Ui)) {
        run_frame_with(egui::RawInput::default(), f)
    }
    fn run_frame_with(input: egui::RawInput, mut f: impl FnMut(&mut Ui)) {
        let ctx = egui::Context::default();
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui));
        });
    }

    #[test]
    fn draw_hierarchy_smoke_with_rename_search_and_empty_scene() {
        let (mut w, mut s, a, ..) = setup();
        s.selection.add(a);
        run_frame(|ui| draw_hierarchy(ui, &mut s, &mut w));
        s.begin_rename(&w, a);
        run_frame(|ui| draw_hierarchy(ui, &mut s, &mut w));
        run_frame(|ui| draw_hierarchy(ui, &mut s, &mut w));
        s.search = "c".into();
        run_frame(|ui| draw_hierarchy(ui, &mut s, &mut w));
        let mut empty = World::new();
        let mut s2 = EditorState::new();
        run_frame(|ui| draw_hierarchy(ui, &mut s2, &mut empty));
    }

    #[test]
    fn draw_inspector_smoke_single_multi_and_none() {
        let (mut w, mut s, a, b, ..) = setup();
        run_frame(|ui| draw_inspector(ui, &mut s, &mut w)); // nothing selected
        w.insert_one(a, Transform::default()).unwrap();
        w.insert_one(a, Camera::default()).unwrap();
        w.insert_one(b, Transform::default()).unwrap();
        s.selection.add(a);
        run_frame(|ui| draw_inspector(ui, &mut s, &mut w));
        s.selection.add(b);
        run_frame(|ui| draw_inspector(ui, &mut s, &mut w));
        w.insert_one(a, Locked(true)).unwrap();
        run_frame(|ui| draw_inspector(ui, &mut s, &mut w));
        assert_eq!(s.undo.undo_len(), 0, "drawing alone must not edit anything");
    }

    #[test]
    fn handle_shortcuts_reads_key_events() {
        let (mut w, mut s, a, ..) = setup();
        s.selection.add(a);
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: Key::Delete,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run(input, |ctx| handle_shortcuts(ctx, &mut s, &mut w));
        assert!(!w.contains(a));
    }
}
