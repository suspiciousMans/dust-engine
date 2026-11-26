#version 330 core
// Dust built-in composite for HDR scene targets: exposure, the ACES filmic
// curve (Narkowicz fit), a damage-flash tint, then linear -> sRGB. Also
// declares the classic posterize/dither uniforms so retro profiles can
// still layer them on (uColorLevels >= 256 and uDitherStrength = 0 = off).
in vec2 vUV;
out vec4 FragColor;

uniform sampler2D uSourceTex;
uniform float uExposure;
uniform vec3 uTintColor;
uniform float uTintStrength;
uniform float uColorLevels;
uniform float uDitherStrength;

const mat4 kBayer = mat4(
     0.0,  8.0,  2.0, 10.0,
    12.0,  4.0, 14.0,  6.0,
     3.0, 11.0,  1.0,  9.0,
    15.0,  7.0, 13.0,  5.0
) / 16.0;

vec3 aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

void main() {
    vec4 hdr = texture(uSourceTex, vUV);
    vec3 mapped = aces(hdr.rgb * uExposure);
    vec3 srgb = pow(mapped, vec3(1.0 / 2.2));
    srgb = mix(srgb, uTintColor, uTintStrength);
    float levels = max(uColorLevels, 1.0);
    if (levels < 255.5 || uDitherStrength > 0.0) {
        ivec2 cell = ivec2(mod(gl_FragCoord.xy, 4.0));
        float threshold = kBayer[cell.x][cell.y] - 0.5;
        srgb = floor((srgb + threshold * uDitherStrength / levels) * levels + 0.5) / levels;
    }
    FragColor = vec4(clamp(srgb, 0.0, 1.0), 1.0);
}
