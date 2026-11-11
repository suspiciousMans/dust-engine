//! GLSL for the shadow pass and an opt-in patcher that adds shadow receiving
//! to an existing mesh shader pair **without editing the shader files**.
//!
//! A game that never calls [`patch_mesh_shaders`] keeps byte-identical
//! shaders. A game that opts in passes its own `mesh.vert` / `mesh.frag`
//! sources through it and links the returned pair.
//!
//! # Contract with the source shaders
//! * Vertex: declares `uModel` (mat4), `aPos` (vec3, location 0) and
//!   `aNormal` (vec3) and has a `void main()`.
//! * Fragment: has a `void main()` and writes its colour to `frag_output`
//!   (default `FragColor`, a `vec4`).
//!
//! The patcher renames the user's `main` to `jame_user_main`, appends the
//! shadow code and a new `main` that calls it. The fragment's final colour
//! is multiplied by `1 - strength * facing * (1 - lit)`.
//!
//! # Uniforms the patched program expects (set by `Renderer::bind_shadows`)
//! `uShadowMap` (sampler2D), `uShadowMatrix`, `uShadowLightDir`,
//! `uShadowNormalOffset` (world units), `uShadowBias` (vec2: constant, slope),
//! `uShadowTexel` (1/resolution), `uShadowPcfRadius`, `uShadowPcfStep`,
//! `uShadowDither`, `uShadowStrength` (0 = shadows off for this draw).

/// Depth-only vertex shader of the shadow pass.
pub const SHADOW_DEPTH_VERT: &str = "#version 330 core
layout (location = 0) in vec3 aPos;
uniform mat4 uLightViewProj;
uniform mat4 uModel;
void main() {
    gl_Position = uLightViewProj * uModel * vec4(aPos, 1.0);
}
";

/// Depth-only fragment shader (depth is written by fixed function).
pub const SHADOW_DEPTH_FRAG: &str = "#version 330 core
void main() {
}
";

const VERT_APPEND: &str = "
// ---- jame shadow receiver (appended by renderer::shadow_shader) ----
uniform mat4 uShadowMatrix;
uniform vec3 uShadowLightDir;
uniform float uShadowNormalOffset;
out vec4 vJameShadowPos;
out float vJameShadowNdotL;
void main() {
    jame_user_main();
    vec4 wp = uModel * vec4(aPos, 1.0);
    vec3 wn = normalize(mat3(uModel) * aNormal);
    float ndl = dot(wn, normalize(uShadowLightDir));
    float c = clamp(ndl, 0.0, 1.0);
    wp.xyz += wn * (uShadowNormalOffset * sqrt(max(1.0 - c * c, 0.0)));
    vJameShadowPos = uShadowMatrix * wp;
    vJameShadowNdotL = ndl;
}
";

const FRAG_APPEND: &str = "
// ---- jame shadow receiver (appended by renderer::shadow_shader) ----
in vec4 vJameShadowPos;
in float vJameShadowNdotL;
uniform sampler2D uShadowMap;
uniform vec2 uShadowBias;
uniform float uShadowTexel;
uniform int uShadowPcfRadius;
uniform float uShadowPcfStep;
uniform int uShadowDither;
uniform float uShadowStrength;

float jame_bayer4(vec2 fc) {
    float m[16] = float[16](0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0,
                            3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
    ivec2 i = ivec2(mod(fc, 4.0));
    return (m[i.x + i.y * 4] + 0.5) / 16.0;
}

float jame_shadow_lit() {
    vec3 p = vJameShadowPos.xyz / vJameShadowPos.w;
    if (p.x < 0.0 || p.x > 1.0 || p.y < 0.0 || p.y > 1.0 || p.z > 1.0) return 1.0;
    float ndl = clamp(vJameShadowNdotL, 0.0, 1.0);
    float tanT = min(sqrt(max(1.0 - ndl * ndl, 0.0)) / max(ndl, 0.05), 8.0);
    float ref = p.z - (uShadowBias.x + uShadowBias.y * tanT);
    float sum = 0.0;
    float n = 0.0;
    for (int y = -uShadowPcfRadius; y <= uShadowPcfRadius; ++y) {
        for (int x = -uShadowPcfRadius; x <= uShadowPcfRadius; ++x) {
            vec2 uv = p.xy + vec2(float(x), float(y)) * uShadowPcfStep * uShadowTexel;
            sum += (texture(uShadowMap, uv).r < ref) ? 0.0 : 1.0;
            n += 1.0;
        }
    }
    float lit = sum / n;
    if (uShadowDither != 0) {
        lit = (lit > jame_bayer4(gl_FragCoord.xy)) ? 1.0 : 0.0;
    }
    return lit;
}

void main() {
    jame_user_main();
    if (uShadowStrength > 0.0) {
        float lit = jame_shadow_lit();
        float facing = smoothstep(0.0, 0.25, vJameShadowNdotL);
        __OUT__.rgb *= 1.0 - uShadowStrength * facing * (1.0 - lit);
    }
}
";

/// Renames the first `void main(` in `src` to `void jame_user_main(`.
fn rename_main(src: &str) -> anyhow::Result<String> {
    let mut search_from = 0;
    while let Some(rel) = src[search_from..].find("void") {
        let at = search_from + rel;
        let after = &src[at + 4..];
        let trimmed = after.trim_start();
        if let Some(rest) = trimmed.strip_prefix("main") {
            if rest.trim_start().starts_with('(') {
                let name_start = src.len() - rest.len() - 4;
                let mut out = String::with_capacity(src.len() + 16);
                out.push_str(&src[..name_start]);
                out.push_str("jame_user_main");
                out.push_str(rest);
                return Ok(out);
            }
        }
        search_from = at + 4;
    }
    anyhow::bail!("shader has no `void main()` to wrap")
}

/// Adds shadow receiving to a mesh vertex/fragment shader pair.
///
/// `frag_output` is the name of the fragment colour output (usually
/// `FragColor`). Returns `(vertex, fragment)`. Errors if either shader has
/// no `void main(`, or if the sources already contain shadow code.
pub fn patch_mesh_shaders(
    vert: &str,
    frag: &str,
    frag_output: &str,
) -> anyhow::Result<(String, String)> {
    if vert.contains("vJameShadowPos") || frag.contains("vJameShadowPos") {
        anyhow::bail!("shaders are already shadow-patched");
    }
    let mut v = rename_main(vert)?;
    v.push_str(VERT_APPEND);
    let mut f = rename_main(frag)?;
    f.push_str(&FRAG_APPEND.replace("__OUT__", frag_output));
    Ok((v, f))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERT: &str = "#version 330 core
layout (location = 0) in vec3 aPos;
layout (location = 1) in vec3 aNormal;
uniform mat4 uModel;
void main() {
    gl_Position = uModel * vec4(aPos, 1.0);
}
";
    const FRAG: &str = "#version 330 core
out vec4 FragColor;
void main() { FragColor = vec4(1.0); }
";

    #[test]
    fn patch_wraps_main_and_keeps_originals() {
        let (v, f) = patch_mesh_shaders(VERT, FRAG, "FragColor").unwrap();
        assert!(v.starts_with("#version 330 core"));
        assert!(v.contains("void jame_user_main()"));
        assert_eq!(v.matches("void main()").count(), 1);
        assert!(v.contains("jame_user_main();"));
        assert!(f.contains("void jame_user_main() { FragColor"));
        assert_eq!(f.matches("void main()").count(), 1);
        assert!(f.contains("FragColor.rgb *="));
        assert!(!f.contains("__OUT__"));
        // Original text is a prefix modulo the rename.
        assert!(v.replace("jame_user_main", "main").starts_with(VERT.trim_end()));
    }

    #[test]
    fn patch_handles_spacing_and_void_args() {
        let (v, _) = patch_mesh_shaders("void  main ( void ) { }", "void main(){}", "C").unwrap();
        assert!(v.starts_with("void  jame_user_main ( void )") || v.starts_with("void jame_user_main ( void )"), "{}", &v[..40]);
    }

    #[test]
    fn patch_errors() {
        assert!(patch_mesh_shaders("int x;", FRAG, "FragColor").is_err());
        assert!(patch_mesh_shaders(VERT, "float domain;", "FragColor").is_err());
        let (v, f) = patch_mesh_shaders(VERT, FRAG, "FragColor").unwrap();
        assert!(patch_mesh_shaders(&v, &f, "FragColor").is_err());
    }

    #[test]
    fn unpatched_sources_are_untouched_constants() {
        // The helper is pure: it never mutates its inputs.
        let a = VERT.to_string();
        let _ = patch_mesh_shaders(&a, FRAG, "FragColor").unwrap();
        assert_eq!(a, VERT);
    }

    // ---- default path is untouched -------------------------------------

    fn fnv1a(s: &str) -> u64 {
        s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    }

    /// Every game's real mesh shader pair, as shipped (read at compile time).
    const GAME_SHADERS: [(&str, &str, &str, u64, u64); 4] = [
        (
            "physics-sandbox",
            include_str!("../../../games/physics-sandbox/assets/shaders/mesh.vert"),
            include_str!("../../../games/physics-sandbox/assets/shaders/mesh.frag"),
            0x4efde7aea43ebc46,
            0x85e80abc1d5a3a92,
        ),
        (
            "walkaround_demo",
            include_str!("../../../games/walkaround_demo/assets/shaders/mesh.vert"),
            include_str!("../../../games/walkaround_demo/assets/shaders/mesh.frag"),
            0x10e82c4202e2216,
            0x56229633d0213087,
        ),
        (
            "mushroom_man",
            include_str!("../../../games/mushroom_man/assets/shaders/mesh.vert"),
            include_str!("../../../games/mushroom_man/assets/shaders/mesh.frag"),
            0x10e82c4202e2216,
            0x56229633d0213087,
        ),
        (
            "sandbox",
            include_str!("../../../games/sandbox/assets/shaders/mesh.vert"),
            include_str!("../../../games/sandbox/assets/shaders/mesh.frag"),
            0x10e82c4202e2216,
            0x56229633d0213087,
        ),
    ];

    /// Games that do not opt in link their shader files verbatim. This pins
    /// the shipped mesh shader sources (FNV-1a) so any in-place edit to add
    /// shadows fails here; shadow code may only arrive via `patch_mesh_shaders`.
    /// If you change a game shader on purpose, update the hashes.
    #[test]
    fn default_path_shader_sources_are_unchanged() {
        for (game, vert, frag, hv, hf) in GAME_SHADERS {
            assert_eq!(fnv1a(vert), hv, "{game} mesh.vert changed");
            assert_eq!(fnv1a(frag), hf, "{game} mesh.frag changed");
            for src in [vert, frag] {
                assert!(!src.contains("jame_user_main") && !src.contains("uShadow") && !src.contains("vJameShadow"), "{game}");
            }
        }
    }

    #[test]
    fn patching_real_game_shaders_is_opt_in_and_pure() {
        let (_, vert, frag, _, _) = GAME_SHADERS[0];
        let (v, f) = patch_mesh_shaders(vert, frag, "FragColor").unwrap();
        assert_ne!(v, vert);
        assert_ne!(f, frag);
        // Un-renaming the wrapper gives back the original source as a prefix.
        assert!(v.replace("jame_user_main", "main").starts_with(vert.trim_end()));
        assert!(f.replace("jame_user_main", "main").starts_with(frag.trim_end()));
        // The game's own uniforms/outs are still declared exactly once.
        assert_eq!(v.matches("uniform mat4 uModel;").count(), 1);
        assert_eq!(f.matches("out vec4 FragColor;").count(), 1);
    }

    #[test]
    fn render_params_without_shadows_field_load_disabled() {
        let default = crate::profile::RenderParams::default();
        assert!(!default.shadows.enabled);
        let mut ron_text = ron::to_string(&default).unwrap();
        let start = ron_text.find("shadows:").expect("serialized shadows field");
        // Cut the `shadows: (...)` field out to emulate a pre-shadows profile.
        let mut depth = 0i32;
        let mut end = start;
        for (i, c) in ron_text[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        ron_text.replace_range(start..end, "");
        let ron_text = ron_text.replace(",,", ",").replace("(,", "(").replace(",)", ")");
        let old: crate::profile::RenderParams = ron::from_str(&ron_text).unwrap();
        assert!(!old.shadows.enabled);
    }
}
