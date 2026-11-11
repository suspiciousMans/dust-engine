use crate::hud::HudStyle;
use crate::renderer::ShadowFilter;
use crate::profile::{LightingMode, RenderParams, TextureFilterMode};

/// Draws sliders/checkboxes/dropdowns for every `RenderParams` field. Returns
/// `true` if anything changed this frame, so callers can react (e.g. the
/// affine-mapping toggle needs a shader-variant swap, not just a uniform).
pub fn render_params_editor(ui: &mut egui::Ui, params: &mut RenderParams) -> bool {
    let mut changed = false;

    ui.heading("Resolution");
    changed |= ui
        .add(egui::Slider::new(&mut params.resolution_scale, 0.05..=1.0).text("Internal scale"))
        .changed();

    ui.separator();
    ui.heading("Lighting");
    egui::ComboBox::from_label("Lighting mode")
        .selected_text(format!("{:?}", params.lighting_mode))
        .show_ui(ui, |ui| {
            changed |= ui
                .selectable_value(&mut params.lighting_mode, LightingMode::Unlit, "Unlit")
                .changed();
            changed |= ui
                .selectable_value(
                    &mut params.lighting_mode,
                    LightingMode::VertexLit,
                    "Vertex Lit",
                )
                .changed();
        });
    changed |= color_edit(ui, "Ambient color", &mut params.ambient_color);
    ui.label("Sun direction (toward the light)");
    ui.horizontal(|ui| {
        changed |= ui
            .add(egui::DragValue::new(&mut params.light_dir[0]).speed(0.05).prefix("x: "))
            .changed();
        changed |= ui
            .add(egui::DragValue::new(&mut params.light_dir[1]).speed(0.05).prefix("y: "))
            .changed();
        changed |= ui
            .add(egui::DragValue::new(&mut params.light_dir[2]).speed(0.05).prefix("z: "))
            .changed();
    });

    ui.separator();
    ui.heading("Sky");
    changed |= color_edit(ui, "Horizon color", &mut params.sky_horizon_color);
    changed |= color_edit(ui, "Zenith color", &mut params.sky_zenith_color);

    ui.separator();
    ui.heading("Fog");
    changed |= color_edit(ui, "Fog color", &mut params.fog_color);
    changed |= ui
        .add(egui::Slider::new(&mut params.fog_start, 0.0..=50.0).text("Fog start"))
        .changed();
    changed |= ui
        .add(egui::Slider::new(&mut params.fog_end, 0.0..=100.0).text("Fog end"))
        .changed();

    ui.separator();
    ui.heading("Posterize / Dither");
    let mut levels = params.color_levels as f32;
    if ui
        .add(egui::Slider::new(&mut levels, 2.0..=256.0).text("Color levels"))
        .changed()
    {
        params.color_levels = levels.round() as u32;
        changed = true;
    }
    changed |= ui
        .add(egui::Slider::new(&mut params.dither_strength, 0.0..=1.0).text("Dither strength"))
        .changed();

    ui.separator();
    ui.heading("Geometry / Texturing");
    changed |= ui
        .add(
            egui::Slider::new(&mut params.vertex_snap_amount, 0.0..=0.2)
                .text("Vertex snap amount"),
        )
        .changed();
    changed |= ui
        .checkbox(&mut params.affine_texture_mapping, "Affine texture mapping")
        .changed();
    changed |= ui
        .checkbox(&mut params.backface_culling, "Backface culling")
        .on_hover_text("Hides a triangle's interior-facing side — stops seeing inside solid geometry when the camera clips into it")
        .changed();
    egui::ComboBox::from_label("Texture filter")
        .selected_text(format!("{:?}", params.texture_filter))
        .show_ui(ui, |ui| {
            changed |= ui
                .selectable_value(
                    &mut params.texture_filter,
                    TextureFilterMode::Nearest,
                    "Nearest",
                )
                .changed();
            changed |= ui
                .selectable_value(
                    &mut params.texture_filter,
                    TextureFilterMode::Bilinear,
                    "Bilinear",
                )
                .changed();
        });

    ui.separator();
    ui.heading("Shadows");
    let sh = &mut params.shadows;
    changed |= ui
        .checkbox(&mut sh.enabled, "Enable shadows")
        .on_hover_text("Needs a game that wires Renderer::set_shadow_settings and shadow-patched shaders")
        .changed();
    if sh.enabled {
        let mut res = sh.resolution as f32;
        if ui.add(egui::Slider::new(&mut res, 64.0..=4096.0).logarithmic(true).text("Map size")).changed() {
            sh.resolution = (res.round() as u32).next_power_of_two().clamp(64, 4096);
            changed = true;
        }
        egui::ComboBox::from_label("Edge filter")
            .selected_text(format!("{:?}", sh.filter))
            .show_ui(ui, |ui| {
                for (f, n) in [
                    (ShadowFilter::Pcf, "PCF (soft)"),
                    (ShadowFilter::Hard, "Hard"),
                    (ShadowFilter::Dithered, "Dithered (PS2)"),
                ] {
                    changed |= ui.selectable_value(&mut sh.filter, f, n).changed();
                }
            });
        changed |= ui.add(egui::Slider::new(&mut sh.strength, 0.0..=1.0).text("Shadow strength")).changed();
        changed |= ui.add(egui::Slider::new(&mut sh.softness, 0.0..=2.0).text("Softness (texels)")).changed();
        changed |= ui.add(egui::Slider::new(&mut sh.bias, 0.0..=4.0).text("Bias (texels)")).changed();
        changed |= ui.add(egui::Slider::new(&mut sh.normal_bias, 0.0..=4.0).text("Normal bias (texels)")).changed();
        changed |= ui.add(egui::Slider::new(&mut sh.max_distance, 5.0..=200.0).text("Max distance")).changed();
        changed |= ui.checkbox(&mut sh.texel_snap, "Texel snapping").changed();
    }

    changed
}

/// Draws sliders/color pickers for every `HudStyle` field, the `HudStyle`
/// sibling of `render_params_editor`. Returns `true` if anything changed —
/// mirrors that function's contract, though (unlike a shader-variant swap)
/// nothing here needs special handling on change; `draw_hud` just reads the
/// live value every frame.
pub fn hud_style_editor(ui: &mut egui::Ui, style: &mut HudStyle) -> bool {
    let mut changed = false;

    ui.label("Anchor offset (from top-left, pixels)");
    ui.horizontal(|ui| {
        changed |= ui
            .add(egui::DragValue::new(&mut style.anchor_offset[0]).speed(1.0).prefix("x: "))
            .changed();
        changed |= ui
            .add(egui::DragValue::new(&mut style.anchor_offset[1]).speed(1.0).prefix("y: "))
            .changed();
    });
    changed |= ui
        .add(egui::Slider::new(&mut style.bar_width, 60.0..=400.0).text("Bar width"))
        .changed();
    changed |= color_edit(ui, "Bar fill color", &mut style.bar_fill_color);
    changed |= ui
        .add(egui::Slider::new(&mut style.title_font_size, 10.0..=48.0).text("Title font size"))
        .changed();
    changed |= color_edit(ui, "Toast text color", &mut style.toast_color);

    changed
}

fn color_edit(ui: &mut egui::Ui, label: &str, color: &mut [f32; 3]) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.color_edit_button_rgb(color).changed()
    })
    .inner
}
