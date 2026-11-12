//! egui renderer for [`UiDoc`]: draws the solved layout and reports
//! clicks/hover as [`UiEvent`]s. Also reused by the designer canvas.

use std::collections::HashMap;

use egui::{Align2, Color32, FontId, Pos2, Rounding, Sense, Stroke, Vec2};

use super::bindings::*;
use super::layout::{solve, LayoutParams, LayoutResult, Rect, TextMeasure};
use super::model::*;
use super::style::{Color, ResolvedStyle, TextAlign, WidgetState};

/// Supplies GPU textures for `Image` nodes (the engine's texture cache
/// implements this). Returns the egui texture id and its size in pixels.
pub trait ImageProvider {
    fn texture(&self, path: &str) -> Option<(egui::TextureId, [f32; 2])>;
}

pub struct DrawParams<'a> {
    /// Screen size in points.
    pub viewport: Vec2,
    pub ui_scale: f32,
    pub safe_area: Insets,
    /// Node drawn with the `focused` style (from [`super::UiFocus`]).
    pub focus: Option<NodeId>,
    pub images: Option<&'a dyn ImageProvider>,
    /// Top-left of the UI in screen points. `ZERO` for a full-window UI;
    /// set it (with `viewport` = the region's size) to draw into a sub-rect
    /// such as the docked editor's Viewport tab.
    pub origin: Pos2,
}

impl<'a> DrawParams<'a> {
    pub fn new(viewport: Vec2) -> Self {
        DrawParams { viewport, ui_scale: 1.0, safe_area: Insets::ZERO, focus: None, images: None, origin: Pos2::ZERO }
    }
}

/// What happened this frame.
#[derive(Default)]
pub struct UiFrameResult {
    /// Button clicks, toggle flips, slider changes — push these to the game's queue.
    pub events: Vec<UiEvent>,
    /// Topmost interactive node under the pointer.
    pub hovered: Option<NodeId>,
    /// Node clicked this frame (buttons and toggles).
    pub clicked: Option<NodeId>,
    pub layout: LayoutResult,
}

/// Interaction state used to pick style variants.
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeStates {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub focused: Option<NodeId>,
}

/// Text measurement using egui's real font metrics.
pub struct EguiTextMeasure<'a>(pub &'a egui::Context);

impl TextMeasure for EguiTextMeasure<'_> {
    fn measure(&self, text: &str, font_size: f32) -> [f32; 2] {
        let galley = self.0.fonts(|f| f.layout_no_wrap(text.to_string(), FontId::proportional(font_size), Color32::WHITE));
        [galley.size().x, galley.size().y]
    }
}

/// Draw `doc` over the whole screen and handle input. The simple entry
/// point: `draw_ui_tree(ctx, &doc, &bindings, viewport)`.
pub fn draw_ui_tree(ctx: &egui::Context, doc: &UiDoc, bindings: &UiBindings, viewport: Vec2) -> UiFrameResult {
    draw_ui_tree_with(ctx, doc, bindings, &DrawParams::new(viewport))
}

pub fn draw_ui_tree_with(ctx: &egui::Context, doc: &UiDoc, bindings: &UiBindings, params: &DrawParams) -> UiFrameResult {
    let measure = EguiTextMeasure(ctx);
    let layout_params = LayoutParams::new([params.viewport.x, params.viewport.y])
        .with_bindings(bindings)
        .with_ui_scale(params.ui_scale)
        .with_safe_area(params.safe_area)
        .with_measure(&measure);
    let layout = solve(doc, &layout_params);

    let mut result = UiFrameResult::default();
    let area_id = egui::Id::new(("ui_tree", doc.name.as_str()));
    egui::Area::new(area_id).fixed_pos(params.origin).order(egui::Order::Foreground).show(ctx, |ui| {
        let nodes = index_nodes(doc);
        let mut states = interact_nodes(ui, &nodes, &layout, Some(bindings), params.origin, &mut result);
        states.focused = params.focus;
        paint_ui(&ui.painter().clone(), params.origin, 1.0, doc, &layout, Some(bindings), &states, params.images);
    });
    result.layout = layout;
    result
}

fn index_nodes(doc: &UiDoc) -> HashMap<NodeId, &UiNode> {
    let mut map = HashMap::new();
    doc.root.walk(&mut |n| {
        map.insert(n.id, n);
    });
    map
}

fn to_egui_rect(r: Rect, origin: Pos2, zoom: f32) -> egui::Rect {
    egui::Rect::from_min_size(Pos2::new(origin.x + r.x * zoom, origin.y + r.y * zoom), Vec2::new(r.w * zoom, r.h * zoom))
}

fn color32(c: Color, opacity: f32) -> Color32 {
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(ch(c[0]), ch(c[1]), ch(c[2]), ch(c[3] * opacity.clamp(0.0, 1.0)))
}

/// Register egui interactions for focusable widgets and collect events.
fn interact_nodes(
    ui: &mut egui::Ui,
    nodes: &HashMap<NodeId, &UiNode>,
    layout: &LayoutResult,
    bindings: Option<&UiBindings>,
    origin: Pos2,
    out: &mut UiFrameResult,
) -> NodeStates {
    let mut states = NodeStates::default();
    for e in &layout.entries {
        let node = nodes[&e.id];
        if !node.kind.is_focusable() {
            continue;
        }
        let enabled = resolve_enabled(node, bindings);
        let sense = match (enabled, &node.kind) {
            (false, _) => Sense::hover(),
            (true, NodeKind::Slider { .. }) => Sense::click_and_drag(),
            (true, _) => Sense::click(),
        };
        let rect = to_egui_rect(e.rect, origin, 1.0);
        let resp = ui.interact(rect, ui.id().with(("node", e.id)), sense);
        if resp.hovered() {
            states.hovered = Some(e.id);
        }
        if enabled && resp.is_pointer_button_down_on() {
            states.pressed = Some(e.id);
        }
        if !enabled {
            continue;
        }
        match &node.kind {
            NodeKind::Button { on_click, .. } => {
                if resp.clicked() {
                    out.clicked = Some(e.id);
                    if let Some(name) = on_click {
                        out.events.push(UiEvent { name: name.clone(), node: e.id, value: None });
                    }
                }
            }
            NodeKind::Toggle { on_toggle, .. } => {
                if resp.clicked() {
                    out.clicked = Some(e.id);
                    if let Some(name) = on_toggle {
                        let value = UiValue::Bool(!resolve_checked(node, bindings));
                        out.events.push(UiEvent { name: name.clone(), node: e.id, value: Some(value) });
                    }
                }
            }
            NodeKind::Slider { on_change, .. } => {
                if resp.dragged() || resp.clicked() {
                    if let (Some(name), Some(pos)) = (on_change, resp.interact_pointer_pos()) {
                        let (_, min, max) = resolve_range(node, bindings);
                        let t = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                        out.events.push(UiEvent { name: name.clone(), node: e.id, value: Some(UiValue::Float(min + t * (max - min))) });
                    }
                }
            }
            _ => {}
        }
    }
    out.hovered = states.hovered;
    states
}

/// Pure painting: no input. `origin`/`zoom` place the viewport on screen
/// (the designer canvas uses both; the game uses `ZERO`/`1.0`).
#[allow(clippy::too_many_arguments)]
pub fn paint_ui(
    painter: &egui::Painter,
    origin: Pos2,
    zoom: f32,
    doc: &UiDoc,
    layout: &LayoutResult,
    bindings: Option<&UiBindings>,
    states: &NodeStates,
    images: Option<&dyn ImageProvider>,
) {
    let nodes = index_nodes(doc);
    let mut effective_opacity: HashMap<NodeId, f32> = HashMap::new();
    let unit = layout.scale * zoom; // design units → screen pixels

    for e in &layout.entries {
        let node = nodes[&e.id];
        let parent_opacity = e.parent.and_then(|p| effective_opacity.get(&p).copied()).unwrap_or(1.0);
        let opacity = parent_opacity * resolve_opacity(node, bindings);
        effective_opacity.insert(e.id, opacity);

        let enabled = resolve_enabled(node, bindings);
        let state = WidgetState {
            hovered: states.hovered == Some(e.id) && enabled,
            pressed: states.pressed == Some(e.id),
            disabled: !enabled,
            focused: states.focused == Some(e.id),
        };
        let classes = resolve_classes(node, bindings);
        let style = doc.styles.resolve(node.kind.name(), &classes, state);
        let rect = to_egui_rect(e.rect, origin, zoom);
        let content = to_egui_rect(e.content, origin, zoom);
        let rounding = Rounding::same(style.corner_radius * unit);

        if style.bg_color[3] > 0.0 && !matches!(node.kind, NodeKind::ProgressBar { .. } | NodeKind::Slider { .. }) {
            painter.rect_filled(rect, rounding, color32(style.bg_color, opacity));
        }
        if style.border_width > 0.0 && !matches!(node.kind, NodeKind::Slider { .. }) {
            painter.rect_stroke(rect, rounding, Stroke::new(style.border_width * unit, color32(style.border_color, opacity)));
        }

        match &node.kind {
            NodeKind::Label { .. } | NodeKind::Button { .. } => {
                let text = resolve_text(node, bindings);
                paint_text(painter, content, &text, &style, unit, opacity);
            }
            NodeKind::Toggle { .. } => paint_toggle(painter, node, content, &style, bindings, unit, opacity),
            NodeKind::Image { path, nine_slice, .. } => paint_image(painter, rect, path, *nine_slice, &style, images, unit, opacity),
            NodeKind::ProgressBar { .. } => paint_progress(painter, node, rect, &style, bindings, unit, opacity),
            NodeKind::Slider { .. } => paint_slider(painter, node, rect, &style, bindings, unit, opacity),
            _ => {}
        }

        // Focus ring for widgets whose style has no dedicated focus border.
        if state.focused && doc.styles.resolve(node.kind.name(), &classes, WidgetState::default()).border_width == style.border_width {
            painter.rect_stroke(rect.expand(2.0 * unit), rounding, Stroke::new(2.0 * unit, color32([1.0, 0.85, 0.3, 1.0], opacity)));
        }
    }
}

fn paint_text(painter: &egui::Painter, area: egui::Rect, text: &str, style: &ResolvedStyle, unit: f32, opacity: f32) {
    if text.is_empty() {
        return;
    }
    let font = FontId::proportional((style.font_size * unit).max(1.0));
    let color = color32(style.text_color, opacity);
    let (pos, anchor) = match style.text_align {
        TextAlign::Left => (Pos2::new(area.left(), area.center().y), Align2::LEFT_CENTER),
        TextAlign::Center => (area.center(), Align2::CENTER_CENTER),
        TextAlign::Right => (Pos2::new(area.right(), area.center().y), Align2::RIGHT_CENTER),
    };
    painter.text(pos, anchor, text, font, color);
}

fn paint_toggle(painter: &egui::Painter, node: &UiNode, content: egui::Rect, style: &ResolvedStyle, b: Option<&UiBindings>, unit: f32, opacity: f32) {
    let box_size = 20.0 * unit;
    let box_rect = egui::Rect::from_min_size(Pos2::new(content.left(), content.center().y - box_size * 0.5), Vec2::splat(box_size));
    painter.rect_stroke(box_rect, Rounding::same(3.0 * unit), Stroke::new(1.5 * unit, color32(style.text_color, opacity)));
    if resolve_checked(node, b) {
        painter.rect_filled(box_rect.shrink(4.0 * unit), Rounding::same(2.0 * unit), color32(style.accent_color, opacity));
    }
    let text_area = egui::Rect::from_min_max(Pos2::new(box_rect.right() + 6.0 * unit, content.top()), content.right_bottom());
    let left_style = ResolvedStyle { text_align: TextAlign::Left, ..style.clone() };
    paint_text(painter, text_area, &resolve_text(node, b), &left_style, unit, opacity);
}

fn paint_progress(painter: &egui::Painter, node: &UiNode, rect: egui::Rect, style: &ResolvedStyle, b: Option<&UiBindings>, unit: f32, opacity: f32) {
    let rounding = Rounding::same(style.corner_radius * unit);
    let track = if style.bg_color[3] > 0.0 { style.bg_color } else { [0.15, 0.15, 0.15, 1.0] };
    painter.rect_filled(rect, rounding, color32(track, opacity));
    let fraction = resolve_fraction(node, b);
    if fraction > 0.0 {
        let fill = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width() * fraction, rect.height()));
        painter.rect_filled(fill, rounding, color32(style.accent_color, opacity));
    }
    if node.bindings.contains_key("text") {
        let centered = ResolvedStyle { text_align: TextAlign::Center, ..style.clone() };
        paint_text(painter, rect, &resolve_text(node, b), &centered, unit, opacity);
    }
}

fn paint_slider(painter: &egui::Painter, node: &UiNode, rect: egui::Rect, style: &ResolvedStyle, b: Option<&UiBindings>, unit: f32, opacity: f32) {
    let fraction = resolve_fraction(node, b);
    let track_h = (rect.height() * 0.25).max(2.0);
    let track = egui::Rect::from_center_size(rect.center(), Vec2::new(rect.width(), track_h));
    let track_color = if style.bg_color[3] > 0.0 { style.bg_color } else { [0.25, 0.25, 0.28, 1.0] };
    painter.rect_filled(track, Rounding::same(track_h * 0.5), color32(track_color, opacity));
    let filled = egui::Rect::from_min_size(track.min, Vec2::new(track.width() * fraction, track_h));
    painter.rect_filled(filled, Rounding::same(track_h * 0.5), color32(style.accent_color, opacity));
    let handle_r = (rect.height() * 0.5).max(2.0 * unit);
    let cx = rect.left() + rect.width() * fraction;
    painter.circle_filled(Pos2::new(cx.clamp(rect.left() + handle_r, rect.right() - handle_r), rect.center().y), handle_r, color32(style.accent_color, opacity));
}

#[allow(clippy::too_many_arguments)]
fn paint_image(
    painter: &egui::Painter,
    rect: egui::Rect,
    path: &str,
    nine_slice: Option<Insets>,
    style: &ResolvedStyle,
    images: Option<&dyn ImageProvider>,
    unit: f32,
    opacity: f32,
) {
    let tint = color32([1.0, 1.0, 1.0, 1.0], opacity);
    let Some((tex, tex_size)) = images.and_then(|p| p.texture(path)) else {
        // Placeholder so a missing texture is obvious in the designer.
        painter.rect_filled(rect, Rounding::ZERO, color32([0.25, 0.1, 0.3, 0.6], opacity));
        painter.line_segment([rect.left_top(), rect.right_bottom()], Stroke::new(1.0_f32, color32([1.0, 0.4, 1.0, 0.7], opacity)));
        painter.line_segment([rect.right_top(), rect.left_bottom()], Stroke::new(1.0_f32, color32([1.0, 0.4, 1.0, 0.7], opacity)));
        painter.text(rect.center(), Align2::CENTER_CENTER, path, FontId::proportional((11.0 * unit).max(1.0)), color32(style.text_color, opacity));
        return;
    };
    let mut mesh = egui::Mesh::with_texture(tex);
    match nine_slice {
        Some(borders) => {
            let dest = Rect::new(rect.left(), rect.top(), rect.width(), rect.height());
            for (r, uv) in nine_slice_quads(dest, borders, tex_size, unit) {
                let uv_rect = egui::Rect::from_min_max(Pos2::new(uv[0], uv[1]), Pos2::new(uv[2], uv[3]));
                mesh.add_rect_with_uv(egui::Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h)), uv_rect, tint);
            }
        }
        None => mesh.add_rect_with_uv(rect, egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint),
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Split `dest` into the 9 quads of a 9-slice image. `borders` are in
/// source pixels; they're drawn `unit` times larger (design-unit scale) and
/// shrunk proportionally if the destination is too small. Returns
/// `(destination rect, [u0, v0, u1, v1])` pairs, row-major, skipping
/// zero-area quads.
pub fn nine_slice_quads(dest: Rect, borders: Insets, tex_size: [f32; 2], unit: f32) -> Vec<(Rect, [f32; 4])> {
    let (tw, th) = (tex_size[0].max(1.0), tex_size[1].max(1.0));
    // Destination border sizes, clamped so left+right <= width etc.
    let squeeze = |a: f32, b: f32, total: f32| {
        let (a, b) = (a * unit, b * unit);
        if a + b > total && a + b > 0.0 {
            let k = total / (a + b);
            (a * k, b * k)
        } else {
            (a, b)
        }
    };
    let (dl, dr) = squeeze(borders.left, borders.right, dest.w);
    let (dt, db) = squeeze(borders.top, borders.bottom, dest.h);
    let xs = [dest.x, dest.x + dl, dest.right() - dr, dest.right()];
    let ys = [dest.y, dest.y + dt, dest.bottom() - db, dest.bottom()];
    let us = [0.0, borders.left / tw, 1.0 - borders.right / tw, 1.0];
    let vs = [0.0, borders.top / th, 1.0 - borders.bottom / th, 1.0];
    let mut out = Vec::with_capacity(9);
    for row in 0..3 {
        for col in 0..3 {
            let r = Rect::new(xs[col], ys[row], xs[col + 1] - xs[col], ys[row + 1] - ys[row]);
            if r.w > 0.0 && r.h > 0.0 {
                out.push((r, [us[col], vs[row], us[col + 1], vs[row + 1]]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_frame(ctx: &egui::Context, events: Vec<egui::Event>, f: &mut dyn FnMut(&egui::Context)) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| f(ctx));
    }

    fn click_doc() -> UiDoc {
        let button = UiNode::new(NodeKind::Button { text: "Go".into(), on_click: Some("go".into()) })
            .with_name("btn")
            .with_size(SizeMode::Fixed(100.0), SizeMode::Fixed(40.0))
            .at([0.0, 0.0], [50.0, 50.0])
            .with_class("button");
        let toggle = UiNode::new(NodeKind::Toggle { text: "Mute".into(), checked: false, on_toggle: Some("mute".into()) })
            .with_name("tog")
            .with_size(SizeMode::Fixed(100.0), SizeMode::Fixed(30.0))
            .at([0.0, 0.0], [50.0, 150.0]);
        UiDoc::new("click_test", UiNode::new(NodeKind::Panel).with_name("root").with_child(button).with_child(toggle))
    }

    /// Hover → press → release at `pos` over several frames (egui hit-tests
    /// against the previous frame's widget rects, so the pointer must rest first).
    fn click_at(ctx: &egui::Context, doc: &UiDoc, b: &UiBindings, pos: Pos2) -> Vec<UiFrameResult> {
        let mut results = Vec::new();
        let button_event = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        let frames = vec![
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerMoved(pos)],
            vec![button_event(true)],
            vec![button_event(false)],
            vec![],
        ];
        for events in frames {
            let mut r = None;
            run_frame(ctx, events, &mut |c| r = Some(draw_ui_tree(c, doc, b, Vec2::new(400.0, 300.0))));
            results.push(r.unwrap());
        }
        results
    }

    #[test]
    fn click_on_button_emits_event_and_reports_hover() {
        let ctx = egui::Context::default();
        let doc = click_doc();
        let results = click_at(&ctx, &doc, &UiBindings::new(), Pos2::new(100.0, 70.0));
        let names: Vec<String> = results.iter().flat_map(|r| r.events.iter().map(|e| e.name.clone())).collect();
        assert_eq!(names, vec!["go"], "exactly one click event");
        let btn = doc.root.children[0].id;
        assert_eq!(results[1].hovered, Some(btn));
        assert!(results.iter().any(|r| r.clicked == Some(btn)));
    }

    #[test]
    fn click_on_empty_space_does_nothing() {
        let ctx = egui::Context::default();
        let doc = click_doc();
        let results = click_at(&ctx, &doc, &UiBindings::new(), Pos2::new(350.0, 250.0));
        assert!(results.iter().all(|r| r.events.is_empty() && r.hovered.is_none()));
    }

    #[test]
    fn toggle_event_carries_new_value() {
        let ctx = egui::Context::default();
        let doc = click_doc();
        let results = click_at(&ctx, &doc, &UiBindings::new(), Pos2::new(80.0, 165.0));
        let ev: Vec<&UiEvent> = results.iter().flat_map(|r| r.events.iter()).collect();
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].name.as_str(), ev[0].value.clone()), ("mute", Some(UiValue::Bool(true))));
        // With the binding already checked, the click flips it off.
        let ctx2 = egui::Context::default();
        let mut doc2 = click_doc();
        doc2.root.children[1].bindings.insert("checked".into(), "muted".into());
        let mut b = UiBindings::new();
        b.set_bool("muted", true);
        let results = click_at(&ctx2, &doc2, &b, Pos2::new(80.0, 165.0));
        let ev: Vec<&UiEvent> = results.iter().flat_map(|r| r.events.iter()).collect();
        assert_eq!(ev[0].value, Some(UiValue::Bool(false)));
    }

    #[test]
    fn disabled_button_does_not_fire() {
        let ctx = egui::Context::default();
        let mut doc = click_doc();
        doc.root.children[0].enabled = false;
        let results = click_at(&ctx, &doc, &UiBindings::new(), Pos2::new(100.0, 70.0));
        assert!(results.iter().all(|r| r.events.is_empty()));
    }

    #[test]
    fn draw_produces_shapes_for_visible_nodes_only() {
        let ctx = egui::Context::default();
        let mut doc = click_doc();
        let count = |doc: &UiDoc| {
            let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0))), ..Default::default() };
            let out = ctx.run(raw, |c| {
                draw_ui_tree(c, doc, &UiBindings::new(), Vec2::new(400.0, 300.0));
            });
            out.shapes.len()
        };
        let all = count(&doc);
        doc.root.children[0].visible = false;
        doc.root.children[1].visible = false;
        let fewer = count(&doc);
        assert!(all > fewer, "{all} vs {fewer}");
    }

    #[test]
    fn nine_slice_corners_keep_size_and_cover_destination() {
        let dest = Rect::new(10.0, 20.0, 100.0, 60.0);
        let q = nine_slice_quads(dest, Insets::new(8.0, 6.0, 10.0, 4.0), [32.0, 32.0], 1.0);
        assert_eq!(q.len(), 9);
        // top-left corner: 8x6 at dest origin, uv (0,0)-(0.25, 0.1875)
        assert_eq!(q[0].0, Rect::new(10.0, 20.0, 8.0, 6.0));
        assert_eq!(q[0].1, [0.0, 0.0, 0.25, 6.0 / 32.0]);
        // bottom-right corner: 10x4 flush with dest bottom-right
        assert_eq!(q[8].0, Rect::new(100.0, 76.0, 10.0, 4.0));
        assert_eq!(q[8].1[2..], [1.0, 1.0]);
        let area: f32 = q.iter().map(|(r, _)| r.w * r.h).sum();
        assert!((area - dest.w * dest.h).abs() < 1e-3);
    }

    #[test]
    fn nine_slice_scales_borders_and_shrinks_when_too_small() {
        let q = nine_slice_quads(Rect::new(0.0, 0.0, 100.0, 100.0), Insets::uniform(8.0), [32.0, 32.0], 2.0);
        assert_eq!(q[0].0.w, 16.0, "border drawn at 2× unit");
        // Destination narrower than both borders: they share the width, centre column vanishes.
        let small = nine_slice_quads(Rect::new(0.0, 0.0, 10.0, 100.0), Insets::uniform(8.0), [32.0, 32.0], 1.0);
        assert_eq!(small.len(), 6);
        let total_w: f32 = small.iter().take(2).map(|(r, _)| r.w).sum();
        assert!((total_w - 10.0).abs() < 1e-4);
    }
}
