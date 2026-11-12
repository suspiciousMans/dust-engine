//! The designer panel: `draw_ui_designer(ui, &mut DesignerState)`.
//!
//! Layout: toolbar on top, outline tree on the left, property inspector on
//! the right, canvas preview in the middle, status bar at the bottom. All
//! document changes go through [`DesignerState`] so they are undoable.

use egui::{Align2, Color32, FontId, Pos2, Sense, Stroke, Vec2};

use super::bindings::{UiValue, UiBindings};
use super::edit::*;
use super::focus::NavInput;
use super::layout::{solve, AnchorAxis, LayoutParams, LayoutResult};
use super::model::*;
use super::render::{paint_ui, EguiTextMeasure, NodeStates};
use super::style::{StyleClass, StyleProps, TextAlign};

const HANDLE_RADIUS: f32 = 5.0;
const PROPERTY_SUGGESTIONS: [&str; 9] = ["text", "value", "min", "max", "checked", "visible", "enabled", "opacity", "class"];

/// Draw the whole designer into `ui` (give it a big area, e.g. a dock tab
/// or `CentralPanel`).
pub fn draw_ui_designer(ui: &mut egui::Ui, state: &mut DesignerState) {
    // Hot reload: one stat call every half second.
    let now = ui.input(|i| i.time);
    if now - state.last_poll > 0.5 {
        state.last_poll = now;
        state.check_reload();
    }
    handle_shortcuts(ui, state);

    egui::TopBottomPanel::top("ui_designer_toolbar").show_inside(ui, |ui| toolbar(ui, state));
    egui::TopBottomPanel::bottom("ui_designer_status").show_inside(ui, |ui| status_bar(ui, state));
    egui::SidePanel::left("ui_designer_outline").default_width(230.0).show_inside(ui, |ui| outline(ui, state));
    let layout = compute_layout(ui.ctx(), state);
    egui::SidePanel::right("ui_designer_inspector").default_width(330.0).show_inside(ui, |ui| inspector(ui, state, &layout));
    egui::CentralPanel::default().show_inside(ui, |ui| canvas(ui, state));
}

fn compute_layout(ctx: &egui::Context, state: &DesignerState) -> LayoutResult {
    let measure = EguiTextMeasure(ctx);
    let params = LayoutParams::new(state.viewport())
        .with_bindings(&state.bindings)
        .with_ui_scale(state.ui_scale)
        .with_safe_area(state.safe_area())
        .with_measure(&measure);
    solve(&state.doc, &params)
}

fn handle_shortcuts(ui: &mut egui::Ui, state: &mut DesignerState) {
    if ui.ctx().wants_keyboard_input() {
        return;
    }
    let (undo, redo, dup, del, save) = ui.input(|i| {
        let cmd = i.modifiers.command;
        (
            cmd && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
            (cmd && i.key_pressed(egui::Key::Y)) || (cmd && i.modifiers.shift && i.key_pressed(egui::Key::Z)),
            cmd && i.key_pressed(egui::Key::D),
            i.key_pressed(egui::Key::Delete),
            cmd && i.key_pressed(egui::Key::S),
        )
    });
    if undo {
        state.undo();
    }
    if redo {
        state.redo();
    }
    if !state.preview_interactive {
        if dup {
            state.duplicate_selected();
        }
        if del {
            state.delete_selected();
        }
    }
    if save {
        let _ = state.save();
    }
}

// ---- toolbar ----------------------------------------------------------------

fn toolbar(ui: &mut egui::Ui, state: &mut DesignerState) {
    ui.horizontal_wrapped(|ui| {
        if ui.button("New").clicked() {
            state.checkpoint();
            state.doc = UiDoc::default();
            state.selected = None;
        }
        ui.add(egui::TextEdit::singleline(&mut state.path_text).desired_width(200.0));
        if ui.button("Load").clicked() {
            let path = std::path::PathBuf::from(state.path_text.trim());
            let _ = state.load(&path);
        }
        if ui.button("Save").clicked() {
            let _ = state.save();
        }
        ui.separator();
        if ui.add_enabled(state.can_undo(), egui::Button::new("Undo")).clicked() {
            state.undo();
        }
        if ui.add_enabled(state.can_redo(), egui::Button::new("Redo")).clicked() {
            state.redo();
        }
        ui.separator();
        let label = match state.resolution {
            Some(i) => RESOLUTIONS[i].0.to_string(),
            None => format!("Custom {}×{}", state.custom_resolution[0], state.custom_resolution[1]),
        };
        egui::ComboBox::from_id_salt("ui_designer_resolution").selected_text(label).show_ui(ui, |ui| {
            for (i, (name, _)) in RESOLUTIONS.iter().enumerate() {
                ui.selectable_value(&mut state.resolution, Some(i), *name);
            }
            ui.selectable_value(&mut state.resolution, None, "Custom");
        });
        if state.resolution.is_none() {
            ui.add(egui::DragValue::new(&mut state.custom_resolution[0]).range(160.0..=8192.0).speed(4.0));
            ui.add(egui::DragValue::new(&mut state.custom_resolution[1]).range(120.0..=8192.0).speed(4.0));
        }
        ui.add(egui::Slider::new(&mut state.zoom, 0.25..=3.0).text("zoom"));
        ui.add(egui::Slider::new(&mut state.ui_scale, 0.5..=3.0).text("UI scale"));
        ui.checkbox(&mut state.preview_safe_area, "Safe area");
    });
    ui.horizontal_wrapped(|ui| {
        ui.menu_button("Add ▾", |ui| {
            for name in NodeKind::ALL_NAMES {
                if ui.button(name).clicked() {
                    state.add_node(name);
                    ui.close_menu();
                }
            }
        });
        let has_sel = state.selected.is_some();
        if ui.add_enabled(has_sel, egui::Button::new("Duplicate")).clicked() {
            state.duplicate_selected();
        }
        if ui.add_enabled(has_sel, egui::Button::new("Delete")).clicked() {
            state.delete_selected();
        }
        if ui.add_enabled(has_sel, egui::Button::new("↑")).on_hover_text("Earlier among siblings").clicked() {
            state.reorder_selected(-1);
        }
        if ui.add_enabled(has_sel, egui::Button::new("↓")).on_hover_text("Later among siblings").clicked() {
            state.reorder_selected(1);
        }
        ui.menu_button("Move into ▾", |ui| {
            let mut targets = Vec::new();
            state.doc.root.walk(&mut |n| {
                if n.kind.is_container() {
                    targets.push((n.id, n.name.clone()));
                }
            });
            for (id, name) in targets {
                if ui.button(format!("{name} (#{id})")).clicked() {
                    state.reparent_selected(id);
                    ui.close_menu();
                }
            }
        });
        ui.separator();
        ui.add(egui::DragValue::new(&mut state.snap).range(0.0..=64.0).speed(0.5).prefix("snap "));
        ui.checkbox(&mut state.preview_interactive, "Interactive preview")
            .on_hover_text("Hover/click buttons, arrow keys / Tab / Enter move focus. Events are logged below.");
    });
}

fn status_bar(ui: &mut egui::Ui, state: &mut DesignerState) {
    ui.horizontal(|ui| {
        ui.label(format!("{}{}", state.doc.name, if state.dirty { " *" } else { "" }));
        ui.separator();
        ui.label(&state.status);
        let problems = state.doc.styles.validate();
        if !problems.is_empty() {
            ui.separator();
            ui.colored_label(Color32::LIGHT_RED, format!("{} style problem(s): {}", problems.len(), problems[0]));
        }
    });
    if !state.event_log.is_empty() {
        let tail: Vec<&str> = state.event_log.iter().rev().take(3).map(|s| s.as_str()).collect();
        ui.label(format!("events: {}", tail.join("  ·  ")));
    }
}

// ---- outline ------------------------------------------------------------------

fn outline(ui: &mut egui::Ui, state: &mut DesignerState) {
    ui.heading("Outline");
    let mut clicked = None;
    let mut toggle_visible = None;
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let selected = state.selected;
        outline_node(ui, &state.doc.root, selected, &mut clicked, &mut toggle_visible);
    });
    if let Some(id) = clicked {
        state.select(Some(id));
    }
    if let Some(id) = toggle_visible {
        state.checkpoint();
        if let Some(n) = state.doc.find_mut(id) {
            n.visible = !n.visible;
        }
    }
}

fn outline_node(ui: &mut egui::Ui, node: &UiNode, selected: Option<NodeId>, clicked: &mut Option<NodeId>, toggle: &mut Option<NodeId>) {
    let mut title = |ui: &mut egui::Ui| {
        ui.horizontal(|ui| {
            if ui.small_button(if node.visible { "●" } else { "○" }).on_hover_text("Toggle visible").clicked() {
                *toggle = Some(node.id);
            }
            let text = format!("{}  [{}]", node.name, node.kind.name());
            if ui.selectable_label(selected == Some(node.id), text).clicked() {
                *clicked = Some(node.id);
            }
        });
    };
    if node.children.is_empty() {
        title(ui);
    } else {
        let id = ui.make_persistent_id(("ui_designer_outline", node.id));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
            .show_header(ui, title)
            .body(|ui| {
                for c in &node.children {
                    outline_node(ui, c, selected, clicked, toggle);
                }
            });
    }
}

// ---- inspector ------------------------------------------------------------------

fn inspector(ui: &mut egui::Ui, state: &mut DesignerState, layout: &LayoutResult) {
    let pointer_down = ui.input(|i| i.pointer.any_down());
    let before = state.doc.clone();
    let mut preset = None;

    ui.heading("Inspector");
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if let Some(id) = state.selected {
            let free = state.is_free_positioned(id);
            if let Some(node) = state.doc.find_mut(id) {
                node_section(ui, node, free, &mut preset);
            }
        } else {
            ui.label("Select a node in the outline or on the canvas.");
        }
        ui.separator();
        document_section(ui, &mut state.doc);
        style_section(ui, &mut state.doc.styles, &mut state.style_class, &mut state.new_class_name);
        bindings_section(ui, &mut state.bindings, &mut state.new_binding_name);
    });

    if state.doc != before {
        state.record_change(before, pointer_down);
    } else {
        state.end_edit_group_if_idle(pointer_down);
    }
    if let Some((h, v)) = preset {
        state.apply_preset(h, v, layout);
    }
}

fn document_section(ui: &mut egui::Ui, doc: &mut UiDoc) {
    egui::CollapsingHeader::new("Document").default_open(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("name");
            ui.text_edit_singleline(&mut doc.name);
        });
        ui.checkbox(&mut doc.respect_safe_area, "Respect safe area");
        let mut scaled = matches!(doc.scale_mode, ScaleMode::ScaleWithScreen { .. });
        if ui.checkbox(&mut scaled, "Scale with screen").changed() {
            doc.scale_mode = if scaled { ScaleMode::ScaleWithScreen { reference: [1920.0, 1080.0], match_height: 0.5 } } else { ScaleMode::Constant };
        }
        if let ScaleMode::ScaleWithScreen { reference, match_height } = &mut doc.scale_mode {
            vec2_row(ui, "reference", reference, 1.0);
            ui.add(egui::Slider::new(match_height, 0.0..=1.0).text("match width ↔ height"));
        }
    });
}

fn node_section(ui: &mut egui::Ui, node: &mut UiNode, free: bool, preset: &mut Option<(AnchorAxis, AnchorAxis)>) {
    ui.label(format!("{} #{}", node.kind.name(), node.id));
    ui.horizontal(|ui| {
        ui.label("name");
        ui.text_edit_singleline(&mut node.name);
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut node.visible, "visible");
        ui.checkbox(&mut node.enabled, "enabled");
    });
    ui.horizontal(|ui| {
        ui.label("opacity");
        ui.add(egui::DragValue::new(&mut node.opacity).range(0.0..=1.0).speed(0.01));
        ui.label("z");
        ui.add(egui::DragValue::new(&mut node.z));
    });
    let mut classes = node.classes.join(" ");
    ui.horizontal(|ui| {
        ui.label("classes");
        if ui.text_edit_singleline(&mut classes).changed() {
            node.classes = classes.split_whitespace().map(String::from).collect();
        }
    });

    egui::CollapsingHeader::new("Properties").default_open(true).show(ui, |ui| kind_editor(ui, &mut node.kind));

    egui::CollapsingHeader::new("Layout").default_open(true).show(ui, |ui| {
        if free {
            ui.horizontal(|ui| {
                if let Some(p) = anchor_preset_grid(ui, node) {
                    *preset = Some(p);
                }
                ui.vertical(|ui| {
                    ui.label("Anchor presets keep the node where it is on screen.");
                });
            });
            vec2_row(ui, "anchor min", &mut node.anchor_min, 0.01);
            vec2_row(ui, "anchor max", &mut node.anchor_max, 0.01);
            vec2_row(ui, "pivot", &mut node.pivot, 0.01);
            vec2_row(ui, "offset min", &mut node.offset_min, 1.0);
            vec2_row(ui, "offset max", &mut node.offset_max, 1.0);
        } else {
            ui.weak("Positioned by its stack/grid parent (anchors unused).");
            ui.horizontal(|ui| {
                ui.label("fill weight");
                ui.add(egui::DragValue::new(&mut node.fill_weight).range(0.0..=100.0).speed(0.1));
            });
        }
        size_mode_row(ui, "width", &mut node.width);
        size_mode_row(ui, "height", &mut node.height);
        insets_row(ui, "margin", &mut node.margin);
        insets_row(ui, "padding", &mut node.padding);
    });

    egui::CollapsingHeader::new("Bindings").default_open(!node.bindings.is_empty()).show(ui, |ui| {
        let mut remove = None;
        for (prop, value) in node.bindings.iter_mut() {
            ui.horizontal(|ui| {
                ui.label(prop);
                ui.text_edit_singleline(value);
                if ui.small_button("✕").clicked() {
                    remove = Some(prop.clone());
                }
            });
        }
        if let Some(p) = remove {
            node.bindings.remove(&p);
        }
        ui.menu_button("+ binding", |ui| {
            for prop in PROPERTY_SUGGESTIONS {
                if ui.button(prop).clicked() {
                    node.bindings.entry(prop.to_string()).or_default();
                    ui.close_menu();
                }
            }
        });
        ui.weak("text: \"{player.health}/{player.max}\"   visible: \"name\" or \"!name\"");
    });
}

fn kind_editor(ui: &mut egui::Ui, kind: &mut NodeKind) {
    match kind {
        NodeKind::Panel | NodeKind::Spacer => {
            ui.weak("No extra properties.");
        }
        NodeKind::Label { text } => string_row(ui, "text", text),
        NodeKind::Button { text, on_click } => {
            string_row(ui, "text", text);
            opt_string_row(ui, "on_click", on_click);
        }
        NodeKind::Image { path, nine_slice, native_size } => {
            string_row(ui, "path", path);
            vec2_row(ui, "native size", native_size, 1.0);
            let mut sliced = nine_slice.is_some();
            if ui.checkbox(&mut sliced, "9-slice").changed() {
                *nine_slice = if sliced { Some(Insets::uniform(8.0)) } else { None };
            }
            if let Some(borders) = nine_slice {
                insets_row(ui, "borders (px)", borders);
            }
        }
        NodeKind::ProgressBar { value, min, max } => range_rows(ui, value, min, max),
        NodeKind::Slider { value, min, max, on_change } => {
            range_rows(ui, value, min, max);
            opt_string_row(ui, "on_change", on_change);
        }
        NodeKind::Toggle { text, checked, on_toggle } => {
            string_row(ui, "text", text);
            ui.checkbox(checked, "checked");
            opt_string_row(ui, "on_toggle", on_toggle);
        }
        NodeKind::HStack { spacing, main_align, cross_align } | NodeKind::VStack { spacing, main_align, cross_align } => {
            ui.horizontal(|ui| {
                ui.label("spacing");
                ui.add(egui::DragValue::new(spacing).speed(0.5));
            });
            align_row(ui, "main align", main_align);
            align_row(ui, "cross align", cross_align);
        }
        NodeKind::Grid { columns, spacing } => {
            ui.horizontal(|ui| {
                ui.label("columns");
                ui.add(egui::DragValue::new(columns).range(1..=32));
            });
            vec2_row(ui, "spacing", spacing, 0.5);
        }
    }
}

fn range_rows(ui: &mut egui::Ui, value: &mut f32, min: &mut f32, max: &mut f32) {
    ui.horizontal(|ui| {
        ui.label("value");
        ui.add(egui::DragValue::new(value).speed(0.01));
        ui.label("min");
        ui.add(egui::DragValue::new(min).speed(0.01));
        ui.label("max");
        ui.add(egui::DragValue::new(max).speed(0.01));
    });
}

/// Unity-style 4×4 anchor preset picker. Returns the clicked preset.
fn anchor_preset_grid(ui: &mut egui::Ui, node: &UiNode) -> Option<(AnchorAxis, AnchorAxis)> {
    const AXES: [AnchorAxis; 4] = [AnchorAxis::Start, AnchorAxis::Middle, AnchorAxis::End, AnchorAxis::Stretch];
    let is_current = |axis: usize, preset: AnchorAxis| {
        let (a0, a1) = (node.anchor_min[axis], node.anchor_max[axis]);
        match preset {
            AnchorAxis::Start => a0 == 0.0 && a1 == 0.0,
            AnchorAxis::Middle => a0 == 0.5 && a1 == 0.5,
            AnchorAxis::End => a0 == 1.0 && a1 == 1.0,
            AnchorAxis::Stretch => a0 == 0.0 && a1 == 1.0,
        }
    };
    let mut result = None;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(2.0);
        for v in AXES {
            ui.horizontal(|ui| {
                for h in AXES {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                    let current = is_current(0, h) && is_current(1, v);
                    let bg = if resp.hovered() { Color32::from_gray(70) } else if current { Color32::from_rgb(40, 70, 110) } else { Color32::from_gray(40) };
                    let painter = ui.painter();
                    painter.rect_filled(rect, 3.0, bg);
                    painter.rect_stroke(rect, 3.0, Stroke::new(1.0_f32, Color32::from_gray(110)));
                    // Mini diagram: the anchored region inside the cell.
                    let inner = rect.shrink(4.0);
                    let span = |preset: AnchorAxis, lo: f32, hi: f32| -> (f32, f32) {
                        let size = 4.0;
                        match preset {
                            AnchorAxis::Start => (lo, lo + size),
                            AnchorAxis::Middle => ((lo + hi - size) * 0.5, (lo + hi + size) * 0.5),
                            AnchorAxis::End => (hi - size, hi),
                            AnchorAxis::Stretch => (lo, hi),
                        }
                    };
                    let (x0, x1) = span(h, inner.left(), inner.right());
                    let (y0, y1) = span(v, inner.top(), inner.bottom());
                    painter.rect_filled(egui::Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)), 1.0, Color32::from_rgb(240, 170, 60));
                    if resp.on_hover_text(format!("{h:?} × {v:?}")).clicked() {
                        result = Some((h, v));
                    }
                }
            });
        }
    });
    result
}

fn string_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value);
    });
}

fn opt_string_row(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut text = value.clone().unwrap_or_default();
        if ui.text_edit_singleline(&mut text).changed() {
            *value = if text.is_empty() { None } else { Some(text) };
        }
    });
}

fn vec2_row(ui: &mut egui::Ui, label: &str, v: &mut [f32; 2], speed: f64) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(&mut v[0]).speed(speed));
        ui.add(egui::DragValue::new(&mut v[1]).speed(speed));
    });
}

fn insets_row(ui: &mut egui::Ui, label: &str, i: &mut Insets) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(&mut i.left).speed(0.5).prefix("L "));
        ui.add(egui::DragValue::new(&mut i.top).speed(0.5).prefix("T "));
        ui.add(egui::DragValue::new(&mut i.right).speed(0.5).prefix("R "));
        ui.add(egui::DragValue::new(&mut i.bottom).speed(0.5).prefix("B "));
    });
}

fn size_mode_row(ui: &mut egui::Ui, label: &str, mode: &mut SizeMode) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut which = match mode {
            SizeMode::Fixed(_) => 0,
            SizeMode::Fill => 1,
            SizeMode::Fit => 2,
        };
        let old = which;
        egui::ComboBox::from_id_salt(("size_mode", label)).selected_text(["Fixed", "Fill", "Fit"][which]).show_ui(ui, |ui| {
            ui.selectable_value(&mut which, 0, "Fixed");
            ui.selectable_value(&mut which, 1, "Fill");
            ui.selectable_value(&mut which, 2, "Fit");
        });
        if which != old {
            *mode = match which {
                0 => SizeMode::Fixed(100.0),
                1 => SizeMode::Fill,
                _ => SizeMode::Fit,
            };
        }
        if let SizeMode::Fixed(v) = mode {
            ui.add(egui::DragValue::new(v).speed(1.0).range(0.0..=100000.0));
        }
    });
}

fn align_row(ui: &mut egui::Ui, label: &str, align: &mut Align) {
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(("align", label)).selected_text(format!("{align:?}")).show_ui(ui, |ui| {
            for a in [Align::Start, Align::Center, Align::End, Align::Stretch] {
                ui.selectable_value(align, a, format!("{a:?}"));
            }
        });
    });
}

// ---- style editor ------------------------------------------------------------------

fn style_section(ui: &mut egui::Ui, styles: &mut super::style::StyleSheet, selected: &mut Option<String>, new_name: &mut String) {
    egui::CollapsingHeader::new("Styles").default_open(false).show(ui, |ui| {
        let names: Vec<String> = styles.classes.keys().cloned().collect();
        egui::ComboBox::from_id_salt("style_class").selected_text(selected.clone().unwrap_or_else(|| "(pick a class)".into())).show_ui(ui, |ui| {
            for n in &names {
                ui.selectable_value(selected, Some(n.clone()), n);
            }
        });
        ui.horizontal(|ui| {
            ui.text_edit_singleline(new_name);
            if ui.button("Add class").clicked() && !new_name.trim().is_empty() {
                let name = new_name.trim().to_string();
                styles.classes.entry(name.clone()).or_default();
                *selected = Some(name);
                new_name.clear();
            }
        });
        let Some(name) = selected.clone() else { return };
        if ui.button("Delete class").clicked() {
            styles.classes.remove(&name);
            *selected = None;
            return;
        }
        let parent_options: Vec<String> = names.iter().filter(|n| **n != name).cloned().collect();
        let Some(class) = styles.classes.get_mut(&name) else { return };
        style_class_editor(ui, class, &parent_options);
    });
}

fn style_class_editor(ui: &mut egui::Ui, class: &mut StyleClass, parents: &[String]) {
    ui.horizontal(|ui| {
        ui.label("inherits");
        egui::ComboBox::from_id_salt("style_parent").selected_text(class.parent.clone().unwrap_or_else(|| "(none)".into())).show_ui(ui, |ui| {
            ui.selectable_value(&mut class.parent, None, "(none)");
            for p in parents {
                ui.selectable_value(&mut class.parent, Some(p.clone()), p);
            }
        });
    });
    for (title, props) in [
        ("base", &mut class.base),
        ("hover", &mut class.hover),
        ("pressed", &mut class.pressed),
        ("disabled", &mut class.disabled),
        ("focused", &mut class.focused),
    ] {
        egui::CollapsingHeader::new(title).default_open(title == "base").show(ui, |ui| props_editor(ui, props));
    }
}

fn props_editor(ui: &mut egui::Ui, p: &mut StyleProps) {
    opt_color_row(ui, "background", &mut p.bg_color);
    opt_color_row(ui, "text color", &mut p.text_color);
    opt_color_row(ui, "border color", &mut p.border_color);
    opt_color_row(ui, "accent", &mut p.accent_color);
    opt_f32_row(ui, "font size", &mut p.font_size, 14.0);
    opt_f32_row(ui, "border width", &mut p.border_width, 1.0);
    opt_f32_row(ui, "corner radius", &mut p.corner_radius, 4.0);
    ui.horizontal(|ui| {
        let mut on = p.text_align.is_some();
        if ui.checkbox(&mut on, "text align").changed() {
            p.text_align = if on { Some(TextAlign::Left) } else { None };
        }
        if let Some(a) = &mut p.text_align {
            for option in [TextAlign::Left, TextAlign::Center, TextAlign::Right] {
                ui.selectable_value(a, option, format!("{option:?}"));
            }
        }
    });
}

fn opt_color_row(ui: &mut egui::Ui, label: &str, v: &mut Option<[f32; 4]>) {
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, label).changed() {
            *v = if on { Some([1.0, 1.0, 1.0, 1.0]) } else { None };
        }
        if let Some(c) = v {
            ui.color_edit_button_rgba_unmultiplied(c);
        }
    });
}

fn opt_f32_row(ui: &mut egui::Ui, label: &str, v: &mut Option<f32>, default: f32) {
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, label).changed() {
            *v = if on { Some(default) } else { None };
        }
        if let Some(x) = v {
            ui.add(egui::DragValue::new(x).speed(0.25));
        }
    });
}

// ---- preview bindings ----------------------------------------------------------------

fn bindings_section(ui: &mut egui::Ui, bindings: &mut UiBindings, new_name: &mut String) {
    egui::CollapsingHeader::new("Preview bindings").default_open(false).show(ui, |ui| {
        ui.weak("Sample values used by the canvas (not saved).");
        let mut names: Vec<String> = bindings.names().cloned().collect();
        names.sort();
        let mut remove = None;
        for name in names {
            let Some(mut value) = bindings.get(&name).cloned() else { continue };
            ui.horizontal(|ui| {
                ui.label(&name);
                match &mut value {
                    UiValue::Float(f) => {
                        ui.add(egui::DragValue::new(f).speed(0.05));
                    }
                    UiValue::Bool(b) => {
                        ui.checkbox(b, "");
                    }
                    UiValue::Text(t) => {
                        ui.text_edit_singleline(t);
                    }
                }
                if ui.small_button("✕").clicked() {
                    remove = Some(name.clone());
                }
            });
            bindings.set(&name, value);
        }
        if let Some(n) = remove {
            bindings.remove(&n);
        }
        ui.horizontal(|ui| {
            ui.text_edit_singleline(new_name);
            let name = new_name.trim().to_string();
            if ui.button("+ number").clicked() && !name.is_empty() {
                bindings.set_f32(&name, 1.0);
            }
            if ui.button("+ bool").clicked() && !name.is_empty() {
                bindings.set_bool(&name, true);
            }
            if ui.button("+ text").clicked() && !name.is_empty() {
                bindings.set_text(&name, "text");
            }
        });
    });
}

// ---- canvas ------------------------------------------------------------------------------

/// Where the viewport sits inside the canvas: `(screen origin, screen pixels per viewport point)`.
pub fn canvas_geometry(canvas: egui::Rect, viewport: [f32; 2], zoom_multiplier: f32) -> (Pos2, f32) {
    let fit = ((canvas.width() - 24.0) / viewport[0]).min((canvas.height() - 24.0) / viewport[1]).max(0.01);
    let zoom = fit * zoom_multiplier;
    let size = Vec2::new(viewport[0] * zoom, viewport[1] * zoom);
    (canvas.center() - size * 0.5, zoom)
}

fn canvas(ui: &mut egui::Ui, state: &mut DesignerState) {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
    painter.rect_filled(resp.rect, 0.0, Color32::from_gray(26));

    let viewport = state.viewport();
    let (origin, zoom) = canvas_geometry(resp.rect, viewport, state.zoom);
    state.canvas_view = Some(([origin.x, origin.y], zoom));
    let vp_rect = egui::Rect::from_min_size(origin, Vec2::new(viewport[0] * zoom, viewport[1] * zoom));
    painter.rect_filled(vp_rect, 0.0, Color32::from_rgb(38, 42, 52));
    painter.rect_stroke(vp_rect, 0.0, Stroke::new(1.0_f32, Color32::from_gray(90)));

    let layout = compute_layout(ui.ctx(), state);
    let to_viewport = |p: Pos2| -> [f32; 2] { [(p.x - origin.x) / zoom, (p.y - origin.y) / zoom] };
    let to_screen = |r: super::layout::Rect| egui::Rect::from_min_size(origin + Vec2::new(r.x, r.y) * zoom, Vec2::new(r.w, r.h) * zoom);
    let hover_pos = resp.hover_pos().filter(|p| vp_rect.contains(*p));

    // Interactive preview: hover / pressed styles, focus ring, event log.
    let mut states = NodeStates::default();
    if state.preview_interactive {
        let enabled_focusable = |id: NodeId| {
            state.doc.find(id).map_or(false, |n| n.kind.is_focusable() && super::bindings::resolve_enabled(n, Some(&state.bindings)))
        };
        states.hovered = hover_pos.and_then(|p| layout.hit_test_where(to_viewport(p), &enabled_focusable));
        if resp.is_pointer_button_down_on() {
            states.pressed = states.hovered;
        }
        states.focused = state.preview_focus.focused;
        preview_input(ui, &resp, state, &layout, states.hovered);
    }

    let vp_painter = painter.with_clip_rect(vp_rect.intersect(resp.rect));
    paint_ui(&vp_painter, origin, zoom, &state.doc, &layout, Some(&state.bindings), &states, None);

    if state.preview_safe_area {
        let s = state.safe_area().scaled(zoom);
        let red = Color32::from_rgba_unmultiplied(220, 60, 60, 50);
        painter.rect_filled(egui::Rect::from_min_max(vp_rect.min, Pos2::new(vp_rect.max.x, vp_rect.min.y + s.top)), 0.0, red);
        painter.rect_filled(egui::Rect::from_min_max(Pos2::new(vp_rect.min.x, vp_rect.max.y - s.bottom), vp_rect.max), 0.0, red);
        painter.rect_filled(egui::Rect::from_min_max(vp_rect.min, Pos2::new(vp_rect.min.x + s.left, vp_rect.max.y)), 0.0, red);
        painter.rect_filled(egui::Rect::from_min_max(Pos2::new(vp_rect.max.x - s.right, vp_rect.min.y), vp_rect.max), 0.0, red);
    }
    painter.text(
        resp.rect.left_top() + Vec2::new(8.0, 6.0),
        Align2::LEFT_TOP,
        format!("{}×{}  ·  UI scale {:.2}  ·  view {:.0}%", viewport[0], viewport[1], layout.scale, zoom * 100.0),
        FontId::proportional(12.0),
        Color32::from_gray(160),
    );

    if state.preview_interactive {
        return;
    }
    draw_selection_overlay(&painter, state, &layout, &to_screen);
    canvas_pointer(&resp, state, &layout, origin, zoom, &to_viewport, &to_screen);
}

fn draw_selection_overlay(painter: &egui::Painter, state: &DesignerState, layout: &LayoutResult, to_screen: &dyn Fn(super::layout::Rect) -> egui::Rect) {
    let Some(entry) = state.selected.and_then(|id| layout.entry(id)) else { return };
    let rect = to_screen(entry.rect);
    painter.rect_stroke(rect, 0.0, Stroke::new(1.5_f32, Color32::from_rgb(80, 160, 255)));

    // Anchor markers on the parent's content rect.
    if let (Some(node), Some(parent)) = (state.doc.find(entry.id), entry.parent.and_then(|p| layout.entry(p))) {
        if state.is_free_positioned(entry.id) {
            let pc = to_screen(parent.content);
            let point = |ax: f32, ay: f32| Pos2::new(pc.left() + pc.width() * ax, pc.top() + pc.height() * ay);
            let orange = Color32::from_rgb(240, 170, 60);
            let (a, b) = (node.anchor_min, node.anchor_max);
            for p in [point(a[0], a[1]), point(b[0], a[1]), point(a[0], b[1]), point(b[0], b[1])] {
                let r = 4.5;
                let diamond = vec![p + Vec2::new(0.0, -r), p + Vec2::new(r, 0.0), p + Vec2::new(0.0, r), p + Vec2::new(-r, 0.0)];
                painter.add(egui::Shape::convex_polygon(diamond, orange, Stroke::NONE));
            }
            painter.rect_stroke(egui::Rect::from_two_pos(point(a[0], a[1]), point(b[0], b[1])), 0.0, Stroke::new(1.0_f32, orange.gamma_multiply(0.5)));
        }
        if state.is_free_positioned(entry.id) {
            for h in Handle::ALL {
                let f = h.fraction();
                let c = Pos2::new(rect.left() + rect.width() * f[0], rect.top() + rect.height() * f[1]);
                painter.rect_filled(egui::Rect::from_center_size(c, Vec2::splat(HANDLE_RADIUS * 2.0)), 1.0, Color32::WHITE);
                painter.rect_stroke(egui::Rect::from_center_size(c, Vec2::splat(HANDLE_RADIUS * 2.0)), 1.0, Stroke::new(1.0_f32, Color32::from_rgb(80, 160, 255)));
            }
        }
    }
}

fn handle_at(rect: egui::Rect, pos: Pos2) -> Option<Handle> {
    Handle::ALL.into_iter().find(|h| {
        let f = h.fraction();
        let c = Pos2::new(rect.left() + rect.width() * f[0], rect.top() + rect.height() * f[1]);
        (c.x - pos.x).abs() <= HANDLE_RADIUS + 2.0 && (c.y - pos.y).abs() <= HANDLE_RADIUS + 2.0
    })
}

fn canvas_pointer(
    resp: &egui::Response,
    state: &mut DesignerState,
    layout: &LayoutResult,
    origin: Pos2,
    zoom: f32,
    to_viewport: &dyn Fn(Pos2) -> [f32; 2],
    to_screen: &dyn Fn(super::layout::Rect) -> egui::Rect,
) {
    if resp.drag_started() {
        // egui reports the drag a few pixels after the press; hit-test where the press happened.
        if let Some(pos) = resp.ctx.input(|i| i.pointer.press_origin()) {
            let selected_rect = state.selected.and_then(|id| layout.rect(id)).map(to_screen);
            let handle = selected_rect.and_then(|r| handle_at(r, pos));
            let started = if let Some(h) = handle {
                state.begin_drag(DragMode::Resize(h), layout)
            } else if let Some(id) = layout.hit_test(to_viewport(pos)) {
                state.select(Some(id));
                state.begin_drag(DragMode::Move, layout)
            } else {
                false
            };
            state.canvas_drag_start = started.then_some([pos.x, pos.y]);
        }
    }
    if resp.dragged() && state.is_dragging() {
        if let (Some(start), Some(now)) = (state.canvas_drag_start, resp.interact_pointer_pos()) {
            state.update_drag([(now.x - start[0]) / zoom, (now.y - start[1]) / zoom]);
        }
    }
    if resp.drag_stopped() {
        state.end_drag();
        state.canvas_drag_start = None;
    } else if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let _ = origin;
            state.select(layout.hit_test(to_viewport(pos)));
        }
    }
}

/// Interactive preview: log events for clicks, and drive focus with the keyboard.
fn preview_input(ui: &mut egui::Ui, resp: &egui::Response, state: &mut DesignerState, layout: &LayoutResult, hovered: Option<NodeId>) {
    let mut events = Vec::new();
    if resp.clicked() {
        if let Some(node) = hovered.and_then(|id| state.doc.find(id)) {
            state.preview_focus.focused = Some(node.id);
            let name = match &node.kind {
                NodeKind::Button { on_click, .. } => on_click.clone(),
                NodeKind::Toggle { on_toggle, .. } => on_toggle.clone(),
                _ => None,
            };
            if let Some(name) = name {
                events.push(format!("{name} (click on {})", node.name));
            }
        }
    }
    if !ui.ctx().wants_keyboard_input() {
        let keys = [
            (egui::Key::ArrowUp, NavInput::Up),
            (egui::Key::ArrowDown, NavInput::Down),
            (egui::Key::ArrowLeft, NavInput::Left),
            (egui::Key::ArrowRight, NavInput::Right),
            (egui::Key::Enter, NavInput::Accept),
            (egui::Key::Space, NavInput::Accept),
        ];
        let mut inputs: Vec<NavInput> = ui.input(|i| keys.iter().filter(|(k, _)| i.key_pressed(*k)).map(|(_, n)| *n).collect());
        if ui.input(|i| i.key_pressed(egui::Key::Tab)) {
            inputs.push(if ui.input(|i| i.modifiers.shift) { NavInput::Prev } else { NavInput::Next });
        }
        for input in inputs {
            if let Some(e) = state.preview_focus.handle(input, &state.doc, layout, Some(&state.bindings)) {
                events.push(format!("{} (focus {:?})", e.name, e.value));
            }
        }
    }
    state.event_log.extend(events);
    let excess = state.event_log.len().saturating_sub(50);
    state.event_log.drain(..excess);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Vec2 = Vec2::new(1500.0, 900.0);

    fn frame(ctx: &egui::Context, state: &mut DesignerState, events: Vec<egui::Event>) {
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, SCREEN)), events, ..Default::default() };
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| draw_ui_designer(ui, state));
        });
    }

    fn button_state() -> (DesignerState, NodeId) {
        let mut s = DesignerState::default();
        let id = s.add_node("Button").unwrap();
        s.doc.find_mut(id).unwrap().width = SizeMode::Fixed(200.0);
        s.doc.find_mut(id).unwrap().height = SizeMode::Fixed(60.0);
        s.doc.find_mut(id).unwrap().offset_min = [100.0, 100.0];
        s.selected = None;
        s.dirty = false;
        (s, id)
    }

    fn press(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }
    }

    #[test]
    fn designer_draws_every_node_kind_with_inspector() {
        let ctx = egui::Context::default();
        let mut s = DesignerState::default();
        for kind in NodeKind::ALL_NAMES {
            // Containers become the selection and so the parent of later kinds: fine, tests nesting too.
            s.add_node(kind).unwrap();
            frame(&ctx, &mut s, vec![]);
            frame(&ctx, &mut s, vec![]);
        }
        assert_eq!(s.doc.root.subtree_size(), 1 + NodeKind::ALL_NAMES.len());
        // Open all collapsing sections by exercising other panels too.
        s.bindings.set_f32("player.health", 3.0);
        s.preview_interactive = true;
        s.preview_safe_area = true;
        s.resolution = Some(4);
        frame(&ctx, &mut s, vec![]);
    }

    #[test]
    fn inspector_runs_without_changing_the_document() {
        let ctx = egui::Context::default();
        let (mut s, id) = button_state();
        s.selected = Some(id);
        let before = s.snapshot();
        let depth = s.undo_depth();
        for _ in 0..3 {
            frame(&ctx, &mut s, vec![]);
        }
        assert_eq!(s.doc, before, "merely drawing must not mutate the doc");
        assert_eq!(s.undo_depth(), depth);
    }

    #[test]
    fn clicking_canvas_selects_the_node_under_the_pointer() {
        let ctx = egui::Context::default();
        let (mut s, id) = button_state();
        frame(&ctx, &mut s, vec![]);
        let ([ox, oy], zoom) = s.canvas_view.expect("canvas drew");
        let at = |x: f32, y: f32| Pos2::new(ox + x * zoom, oy + y * zoom);
        let on_button = at(150.0, 120.0);
        for events in [vec![egui::Event::PointerMoved(on_button)], vec![egui::Event::PointerMoved(on_button)], vec![press(on_button, true)], vec![press(on_button, false)], vec![]] {
            frame(&ctx, &mut s, events);
        }
        assert_eq!(s.selected, Some(id));
        // Clicking empty canvas (bottom-right of the viewport, away from the button) deselects → root.
        let empty = at(1500.0, 900.0);
        for events in [vec![egui::Event::PointerMoved(empty)], vec![egui::Event::PointerMoved(empty)], vec![press(empty, true)], vec![press(empty, false)], vec![]] {
            frame(&ctx, &mut s, events);
        }
        assert_eq!(s.selected, Some(s.doc.root.id), "root panel covers the rest of the screen");
    }

    #[test]
    fn dragging_on_canvas_moves_node_as_one_undo_step() {
        let ctx = egui::Context::default();
        let (mut s, id) = button_state();
        frame(&ctx, &mut s, vec![]);
        let ([ox, oy], zoom) = s.canvas_view.unwrap();
        let start = Pos2::new(ox + 150.0 * zoom, oy + 120.0 * zoom);
        let end = start + Vec2::new(50.0 * zoom, 20.0 * zoom);
        let depth = s.undo_depth();
        let script = [
            vec![egui::Event::PointerMoved(start)],
            vec![egui::Event::PointerMoved(start)],
            vec![press(start, true)],
            vec![egui::Event::PointerMoved(start + Vec2::new(10.0, 0.0))],
            vec![egui::Event::PointerMoved(end)],
            vec![egui::Event::PointerMoved(end)],
            vec![press(end, false)],
            vec![],
        ];
        for events in script {
            frame(&ctx, &mut s, events);
        }
        let n = s.doc.find(id).unwrap();
        assert!((n.offset_min[0] - 150.0).abs() < 0.5 && (n.offset_min[1] - 120.0).abs() < 0.5, "moved by (50,20): {:?}", n.offset_min);
        assert_eq!(s.undo_depth(), depth + 1);
        assert_eq!(s.selected, Some(id));
        assert!(!s.is_dragging());
    }

    #[test]
    fn dragging_a_handle_resizes() {
        let ctx = egui::Context::default();
        let (mut s, id) = button_state();
        s.selected = Some(id);
        frame(&ctx, &mut s, vec![]);
        let ([ox, oy], zoom) = s.canvas_view.unwrap();
        // SE handle = bottom-right corner of the button: (300, 160)
        let corner = Pos2::new(ox + 300.0 * zoom, oy + 160.0 * zoom);
        let to = corner + Vec2::new(40.0 * zoom, 30.0 * zoom);
        let script = [
            vec![egui::Event::PointerMoved(corner)],
            vec![egui::Event::PointerMoved(corner)],
            vec![press(corner, true)],
            vec![egui::Event::PointerMoved(corner + Vec2::new(8.0, 8.0))],
            vec![egui::Event::PointerMoved(to)],
            vec![egui::Event::PointerMoved(to)],
            vec![press(to, false)],
            vec![],
        ];
        for events in script {
            frame(&ctx, &mut s, events);
        }
        let n = s.doc.find(id).unwrap();
        match (n.width, n.height) {
            (SizeMode::Fixed(w), SizeMode::Fixed(h)) => assert!((w - 240.0).abs() < 0.1 && (h - 90.0).abs() < 0.1, "got {w}x{h}"),
            other => panic!("expected fixed sizes, got {other:?}"),
        }
    }

    #[test]
    fn interactive_preview_logs_button_events() {
        let ctx = egui::Context::default();
        let (mut s, id) = button_state();
        if let NodeKind::Button { on_click, .. } = &mut s.doc.find_mut(id).unwrap().kind {
            *on_click = Some("resume_game".into());
        }
        s.preview_interactive = true;
        frame(&ctx, &mut s, vec![]);
        let ([ox, oy], zoom) = s.canvas_view.unwrap();
        let p = Pos2::new(ox + 150.0 * zoom, oy + 120.0 * zoom);
        for events in [vec![egui::Event::PointerMoved(p)], vec![egui::Event::PointerMoved(p)], vec![press(p, true)], vec![press(p, false)], vec![]] {
            frame(&ctx, &mut s, events);
        }
        assert!(s.event_log.iter().any(|e| e.starts_with("resume_game")), "{:?}", s.event_log);
        assert_eq!(s.preview_focus.focused, Some(id));
    }

    #[test]
    fn canvas_geometry_centres_and_fits() {
        let canvas = egui::Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(824.0, 624.0));
        let (origin, zoom) = canvas_geometry(canvas, [1600.0, 1200.0], 1.0);
        assert!((zoom - 0.5).abs() < 1e-5);
        assert!((origin.x - 112.0).abs() < 1e-3 && (origin.y - 62.0).abs() < 1e-3);
        let (_, z2) = canvas_geometry(canvas, [1600.0, 1200.0], 2.0);
        assert!((z2 - 1.0).abs() < 1e-5);
    }
}
