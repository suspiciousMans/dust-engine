use glow::HasContext;

use crate::shader::link_program;

const VERTEX_SRC: &str = r#"
#version 330 core
out vec2 vClipPos;
void main() {
    vec2 positions[3] = vec2[3](vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    gl_Position = vec4(positions[gl_VertexID], 0.0, 1.0);
    vClipPos = positions[gl_VertexID];
}
"#;

// Infinite ground grid on y = 0: every pixel's view ray is intersected with
// the plane analytically, lines are anti-aliased with fwidth, minor/major
// spacing is 1 / 10 units, the X axis is red and the Z axis blue, and the
// whole thing fades out with distance. Depth is written so scene geometry
// drawn afterwards (or before) occludes it correctly.
const FRAGMENT_SRC: &str = r#"
#version 330 core
in vec2 vClipPos;
out vec4 FragColor;
uniform mat4 uInvViewProj;
uniform mat4 uViewProj;
uniform float uFadeDistance;
void main() {
    vec4 n = uInvViewProj * vec4(vClipPos, -1.0, 1.0);
    vec4 f = uInvViewProj * vec4(vClipPos, 1.0, 1.0);
    n /= n.w;
    f /= f.w;
    vec3 dir = f.xyz - n.xyz;
    if (abs(dir.y) < 1e-6) discard;
    float t = -n.y / dir.y;
    if (t < 0.0 || t > 1.0) discard;
    vec3 p = n.xyz + dir * t;

    vec2 c = p.xz;
    vec2 w = fwidth(c);
    vec2 g1 = abs(fract(c - 0.5) - 0.5) / max(w, vec2(1e-5));
    float minor = 1.0 - min(min(g1.x, g1.y), 1.0);
    vec2 c10 = c / 10.0;
    vec2 g10 = abs(fract(c10 - 0.5) - 0.5) / max(w / 10.0, vec2(1e-5));
    float major = 1.0 - min(min(g10.x, g10.y), 1.0);
    // Axis lines (x-axis is where z == 0, z-axis where x == 0).
    float axisX = 1.0 - min(abs(c.y) / max(w.y, 1e-5), 1.0);
    float axisZ = 1.0 - min(abs(c.x) / max(w.x, 1e-5), 1.0);

    // Lines that would be denser than ~3px vanish instead of aliasing.
    float density = 1.0 - smoothstep(0.25, 0.5, max(w.x, w.y));
    vec4 col = vec4(0.5, 0.5, 0.5, minor * 0.25 * density);
    col = mix(col, vec4(0.6, 0.6, 0.6, 0.55), major);
    col = mix(col, vec4(0.85, 0.25, 0.25, 0.95), axisX);
    col = mix(col, vec4(0.25, 0.4, 0.9, 0.95), axisZ);

    float dist = length(p - n.xyz);
    float fade = 1.0 - smoothstep(uFadeDistance * 0.5, uFadeDistance, dist);
    col.a *= fade;
    if (col.a < 0.01) discard;
    FragColor = col;
    vec4 clip = uViewProj * vec4(p, 1.0);
    gl_FragDepth = clamp(clip.z / clip.w * 0.5 + 0.5, 0.0, 1.0);
}
"#;

/// Editor ground grid, drawn as a fullscreen triangle (no vertex data).
pub struct GridPass {
    program: glow::Program,
    empty_vao: glow::VertexArray,
    u_inv: Option<glow::UniformLocation>,
    u_vp: Option<glow::UniformLocation>,
    u_fade: Option<glow::UniformLocation>,
}

impl GridPass {
    pub fn new(gl: &glow::Context) -> anyhow::Result<Self> {
        let program = link_program(gl, VERTEX_SRC, FRAGMENT_SRC)?;
        unsafe {
            Ok(Self {
                u_inv: gl.get_uniform_location(program, "uInvViewProj"),
                u_vp: gl.get_uniform_location(program, "uViewProj"),
                u_fade: gl.get_uniform_location(program, "uFadeDistance"),
                empty_vao: gl.create_vertex_array().map_err(anyhow::Error::msg)?,
                program,
            })
        }
    }

    /// Draws into the bound framebuffer with alpha blending and depth testing.
    /// `fade_distance` is the world distance (from the near-plane point under
    /// each pixel) at which the grid has fully faded.
    pub fn draw(&self, gl: &glow::Context, view_proj: [f32; 16], inv_view_proj: [f32; 16], fade_distance: f32) {
        unsafe {
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.enable(glow::DEPTH_TEST);
            gl.depth_mask(false);
            gl.use_program(Some(self.program));
            gl.uniform_matrix_4_f32_slice(self.u_inv.as_ref(), false, &inv_view_proj);
            gl.uniform_matrix_4_f32_slice(self.u_vp.as_ref(), false, &view_proj);
            gl.uniform_1_f32(self.u_fade.as_ref(), fade_distance);
            gl.bind_vertex_array(Some(self.empty_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.depth_mask(true);
            gl.disable(glow::BLEND);
        }
    }

    /// # Safety
    /// The pass must not be used after this call.
    pub unsafe fn destroy(&self, gl: &glow::Context) {
        gl.delete_program(self.program);
        gl.delete_vertex_array(self.empty_vao);
    }
}
