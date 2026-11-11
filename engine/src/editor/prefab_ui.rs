//! Prefab UX: library state, hierarchy markers and menu entries, the
//! inspector's instance header and per-field override markers.
//!
//! The egui functions are thin glue; the actions they trigger are plain
//! functions ([`create_prefab`], [`instantiate_prefab`], [`apply`],
//! [`revert_all`], [`revert_field`], [`unpack`]) that run an undoable command
//! through the [`EditorState`], so they are testable without a UI.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use egui::{Color32, RichText, Ui};
use hecs::{Entity, World};

use super::hierarchy::display_name;
use super::prefab::{self, InstantiatePrefab, Override, PrefabLibrary, SharedLibrary, PREFAB_EXT};
use super::prefab_cmd::{PrefabOp, PrefabOpCommand};
use super::scene::EntityId;
use super::state::EditorState;

pub const NO_PROJECT_HINT: &str = "Set a project directory to use prefabs";

const ACCENT: Color32 = Color32::from_rgb(90, 160, 255);

/// Pending "Create Prefab from Selection" name prompt.
#[derive(Clone, Debug)]
pub struct NameDialog {
    pub root: Entity,
    pub buffer: String,
    pub error: Option<String>,
}

/// What the inspector knows about the selected entity's prefab instance this frame.
#[derive(Clone, Debug)]
struct InstanceView {
    root: Entity,
    /// The entity's id inside the prefab file.
    target: EntityId,
    overrides: Vec<Override>,
}

#[derive(Default)]
pub struct PrefabUiState {
    lib: Option<SharedLibrary>,
    dir: Option<PathBuf>,
    pub naming: Option<NameDialog>,
    /// Filter text of the Instantiate Prefab picker.
    pub filter: String,
    view: Option<InstanceView>,
}

impl PrefabUiState {
    /// Points the library at `dir` (the prefabs folder). `None` disables the
    /// prefab entries. No-op if unchanged, so it is cheap to call every frame.
    pub fn set_root(&mut self, dir: Option<PathBuf>) {
        if self.dir == dir {
            return;
        }
        self.lib = dir.as_ref().map(|d| Rc::new(RefCell::new(PrefabLibrary::new(d))));
        self.dir = dir;
        self.naming = None;
        self.view = None;
    }

    /// Forget the inspected entity (multi-selection, nothing selected).
    pub fn clear_view(&mut self) {
        self.view = None;
    }

    pub fn root(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn library(&self) -> Option<SharedLibrary> {
        self.lib.clone()
    }

    /// Overrides of the entity shown in the inspector (empty if it is not
    /// part of an instance).
    pub fn overrides_of_selected(&self) -> &[Override] {
        self.view.as_ref().map(|v| v.overrides.as_slice()).unwrap_or(&[])
    }

    /// `(instance root, prefab-local id)` if `comp.field` of the inspected
    /// entity differs from its prefab.
    pub fn field_override(&self, comp: &str, field: &str) -> Option<(Entity, EntityId)> {
        let v = self.view.as_ref()?;
        v.overrides.iter().any(|o| o.target == v.target && o.comp == comp && o.field == field).then_some((v.root, v.target))
    }
}

// ------------------------------------------------------------------ actions

/// `"props/Lamp"` -> `"props/Lamp.prefab.ron"`; rejects absolute / `..` paths.
pub fn normalize_source(typed: &str) -> Result<String, String> {
    let t = typed.trim().replace('\\', "/");
    let t = t.trim_start_matches('/');
    if t.is_empty() {
        return Err("enter a prefab name".into());
    }
    if t.split('/').any(|c| c == ".." || c.is_empty()) {
        return Err("prefab path must stay inside the prefabs folder".into());
    }
    Ok(if t.ends_with(PREFAB_EXT) { t.to_string() } else { format!("{t}{PREFAB_EXT}") })
}

/// Suggested file name for turning `root` into a prefab.
pub fn default_prefab_name(world: &World, root: Entity) -> String {
    let clean: String = display_name(world, root)
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{}{PREFAB_EXT}", if clean.is_empty() { "Prefab".into() } else { clean })
}

fn run_op(state: &mut EditorState, world: &mut World, op: PrefabOp) -> Result<Option<Entity>, String> {
    let lib = state.prefab_ui.library().ok_or_else(|| NO_PROJECT_HINT.to_string())?;
    let (cmd, handle) = PrefabOpCommand::new(lib, op, world);
    state.exec(world, cmd)?;
    state.undo.seal();
    Ok(handle.get())
}

/// Root of the instance `e` belongs to.
fn instance_root(world: &World, e: Entity) -> Result<Entity, String> {
    prefab::containing_instance(world, e).ok_or_else(|| "not part of a prefab instance".to_string())
}

/// Makes `root`'s subtree a prefab file `typed_name` and an instance of it.
/// Selects and returns the instance root.
pub fn create_prefab(state: &mut EditorState, world: &mut World, root: Entity, typed_name: &str) -> Result<Entity, String> {
    let source = normalize_source(typed_name)?;
    let e = run_op(state, world, PrefabOp::Create { root, source })?.unwrap_or(root);
    state.selection.select_only(e);
    Ok(e)
}

/// Drops the prefab `source` into the scene under `parent` (undoable) and selects it.
pub fn instantiate_prefab(state: &mut EditorState, world: &mut World, source: &str, parent: Option<Entity>) -> Result<Entity, String> {
    let lib = state.prefab_ui.library().ok_or_else(|| NO_PROJECT_HINT.to_string())?;
    let (cmd, handle) = InstantiatePrefab::new(lib, source, parent, None);
    state.exec(world, cmd)?;
    state.undo.seal();
    let e = handle.get().ok_or_else(|| "prefab produced no entity".to_string())?;
    state.selection.select_only(e);
    Ok(e)
}

/// "Apply All": writes the instance back into its prefab, refreshing other instances.
pub fn apply(state: &mut EditorState, world: &mut World, e: Entity) -> Result<(), String> {
    let root = instance_root(world, e)?;
    run_op(state, world, PrefabOp::Apply { root }).map(|_| ())
}

/// "Revert All": drops every override except the instance's placement.
pub fn revert_all(state: &mut EditorState, world: &mut World, e: Entity) -> Result<(), String> {
    let root = instance_root(world, e)?;
    run_op(state, world, PrefabOp::RevertAll { root }).map(|_| ())
}

pub fn revert_field(
    state: &mut EditorState,
    world: &mut World,
    root: Entity,
    target: EntityId,
    comp: &str,
    field: &str,
) -> Result<(), String> {
    run_op(state, world, PrefabOp::RevertField { root, target, comp: comp.into(), field: field.into() }).map(|_| ())
}

/// Detaches the instance from its prefab (nested instances stay instances
/// unless `completely`).
pub fn unpack(state: &mut EditorState, world: &mut World, e: Entity, completely: bool) -> Result<(), String> {
    let root = instance_root(world, e)?;
    run_op(state, world, PrefabOp::Unpack { root, completely }).map(|_| ())
}

fn report(state: &mut EditorState, r: Result<(), String>) {
    if let Err(e) = r {
        state.last_error = Some(e);
    }
}

// ----------------------------------------------------------------- hierarchy

/// Marker in front of the row name: ◆ on an instance root, ◇ on its members.
pub fn row_marker(ui: &mut Ui, world: &World, e: Entity) {
    if let Some(inst) = prefab::instance_of(world, e) {
        let n_over = inst.overrides.len();
        let tip = if n_over == 0 {
            format!("Prefab instance of {}", inst.source)
        } else {
            format!("Prefab instance of {} ({n_over} overrides)", inst.source)
        };
        ui.label(RichText::new("◆").color(ACCENT)).on_hover_text(tip);
    } else if world.get::<&prefab::PrefabMember>(e).is_ok() {
        ui.label(RichText::new("◇").color(ACCENT.gamma_multiply(0.6))).on_hover_text("Part of a prefab instance");
    }
}

fn prefab_display(source: &str) -> &str {
    let n = source.rsplit('/').next().unwrap_or(source);
    n.strip_suffix(PREFAB_EXT).unwrap_or(n)
}

/// "Instantiate Prefab" picker; used by every Create menu in the hierarchy.
pub fn create_menu_items(ui: &mut Ui, state: &mut EditorState, world: &mut World, parent: Option<Entity>) {
    ui.separator();
    let Some(lib) = state.prefab_ui.library() else {
        ui.add_enabled(false, egui::Button::new("Instantiate Prefab")).on_disabled_hover_text(NO_PROJECT_HINT);
        return;
    };
    ui.menu_button("Instantiate Prefab", |ui| {
        ui.add(egui::TextEdit::singleline(&mut state.prefab_ui.filter).hint_text("Search…"));
        let f = state.prefab_ui.filter.to_lowercase();
        let list: Vec<String> = lib.borrow().list().into_iter().filter(|s| s.to_lowercase().contains(&f)).collect();
        if list.is_empty() {
            ui.weak(if f.is_empty() { "No prefabs in the project yet" } else { "No matches" });
        }
        egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
            for source in list {
                if ui.button(prefab_display(&source)).on_hover_text(&source).clicked() {
                    let r = instantiate_prefab(state, world, &source, parent).map(|_| ());
                    report(state, r);
                    ui.close_menu();
                }
            }
        });
    });
}

/// Entries for the hierarchy row context menu of `e`.
pub fn row_menu_items(ui: &mut Ui, state: &mut EditorState, world: &mut World, e: Entity) {
    ui.separator();
    let has_lib = state.prefab_ui.library().is_some();
    let single = state.selection.top_level(world).len() <= 1;
    let root = state.selection.primary().filter(|p| world.contains(*p)).unwrap_or(e);
    let create = ui.add_enabled(has_lib && single, egui::Button::new("Create Prefab from Selection"));
    let create = if !has_lib {
        create.on_disabled_hover_text(NO_PROJECT_HINT)
    } else if !single {
        create.on_disabled_hover_text("Select a single object (with its children)")
    } else {
        create
    };
    if create.clicked() {
        state.prefab_ui.naming = Some(NameDialog { root, buffer: default_prefab_name(world, root), error: None });
        ui.close_menu();
    }
    if prefab::containing_instance(world, e).is_some() {
        ui.menu_button("Prefab", |ui| {
            if ui.button("Apply to Prefab").clicked() {
                let r = apply(state, world, e);
                report(state, r);
                ui.close_menu();
            }
            if ui.button("Revert All").clicked() {
                let r = revert_all(state, world, e);
                report(state, r);
                ui.close_menu();
            }
            if ui.button("Unpack").clicked() {
                let r = unpack(state, world, e, false);
                report(state, r);
                ui.close_menu();
            }
            if ui.button("Unpack Completely").clicked() {
                let r = unpack(state, world, e, true);
                report(state, r);
                ui.close_menu();
            }
        });
    }
}

/// Modal-ish windows owned by the prefab UX (currently the name prompt).
pub fn draw_dialogs(ctx: &egui::Context, state: &mut EditorState, world: &mut World) {
    let Some(mut dlg) = state.prefab_ui.naming.take() else { return };
    if !world.contains(dlg.root) {
        return;
    }
    let mut keep = true;
    let mut submit = false;
    egui::Window::new("Create Prefab").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).show(
        ctx,
        |ui| {
            ui.label(format!("Prefab from '{}' (path relative to the prefabs folder):", display_name(world, dlg.root)));
            let r = ui.add(egui::TextEdit::singleline(&mut dlg.buffer).desired_width(320.0));
            if let Some(e) = &dlg.error {
                ui.colored_label(Color32::from_rgb(235, 90, 80), e);
            }
            ui.horizontal(|ui| {
                submit = ui.button("Create").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
        },
    );
    if submit {
        match create_prefab(state, world, dlg.root, &dlg.buffer) {
            Ok(_) => return,
            Err(e) => dlg.error = Some(e),
        }
    }
    if keep {
        state.prefab_ui.naming = Some(dlg);
    }
}

// ----------------------------------------------------------------- inspector

/// Inspector header for the primary selection. Always call it (it also
/// resets the per-frame override cache used by [`field_label`]).
pub fn inspector_header(ui: &mut Ui, state: &mut EditorState, world: &mut World, primary: Entity) {
    state.prefab_ui.view = None;
    let Some(root) = prefab::containing_instance(world, primary) else { return };
    let Some(inst) = prefab::instance_of(world, root) else { return };

    // Overrides are derived data: refresh them from the live values.
    if let Some(lib) = state.prefab_ui.library() {
        prefab::update_overrides_of(world, &state.registry, &mut lib.borrow_mut(), root);
    }
    let inst = prefab::instance_of(world, root).unwrap_or(inst);
    let target = world
        .get::<&prefab::PrefabMember>(primary)
        .ok()
        .and_then(|m| m.0.iter().find(|r| r.source == inst.source).map(|r| r.id));
    if let Some(target) = target {
        state.prefab_ui.view = Some(InstanceView { root, target, overrides: inst.overrides.clone() });
    }

    let is_root = root == primary;
    let n = inst.overrides.len();
    let mut action: Option<&'static str> = None;
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("◆ Prefab").color(ACCENT).strong());
            ui.label(prefab_display(&inst.source)).on_hover_text(&inst.source);
            ui.weak(match n {
                0 => "no overrides".to_string(),
                1 => "1 override".to_string(),
                n => format!("{n} overrides"),
            });
        });
        if is_root {
            let has_lib = state.prefab_ui.library().is_some();
            ui.horizontal(|ui| {
                if ui.add_enabled(has_lib, egui::Button::new("Apply")).on_hover_text("Write this instance back into the prefab").clicked() {
                    action = Some("apply");
                }
                if ui.add_enabled(has_lib && n > 0, egui::Button::new("Revert All")).on_hover_text("Drop all overrides").clicked() {
                    action = Some("revert");
                }
                if ui.button("Unpack").on_hover_text("Detach from the prefab").clicked() {
                    action = Some("unpack");
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.weak(format!("Part of '{}'", display_name(world, root)));
                if ui.small_button("Select root").clicked() {
                    action = Some("select_root");
                }
            });
        }
    });
    let r = match action {
        Some("apply") => apply(state, world, root),
        Some("revert") => revert_all(state, world, root),
        Some("unpack") => unpack(state, world, root, false),
        Some("select_root") => {
            state.selection.select_only(root);
            Ok(())
        }
        _ => Ok(()),
    };
    report(state, r);
}

/// The label cell of an inspector field. Overridden fields get a ● marker
/// and a "Revert field" context menu / button.
pub fn field_label(ui: &mut Ui, state: &mut EditorState, world: &mut World, comp: &str, field: &str, text: RichText) -> egui::Response {
    let Some((root, target)) = state.prefab_ui.field_override(comp, field) else { return ui.label(text) };
    let mut revert = false;
    let l = ui
        .horizontal(|ui| {
            ui.label(RichText::new("●").color(ACCENT)).on_hover_text("Overrides the prefab value");
            let l = ui.label(text.strong());
            if ui.small_button("↺").on_hover_text("Revert field").clicked() {
                revert = true;
            }
            l
        })
        .inner;
    l.context_menu(|ui| {
        if ui.button("Revert field").clicked() {
            revert = true;
            ui.close_menu();
        }
    });
    if revert {
        let r = revert_field(state, world, root, target, comp, field);
        report(state, r);
    }
    l
}
