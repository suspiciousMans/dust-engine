// Dust built-in "hi-fi" mesh fragment stage: per-pixel metal/rough shading
// (GGX specular, Lambert diffuse, Schlick Fresnel), an optional tangent-space
// normal map with no precomputed tangents, an optional metal/rough map
// (glTF layout: G = roughness, B = metallic), emissive, a sky/ground
// hemisphere ambient, one directional light and up to 8 point lights.
// Writes LINEAR colour: render into an HDR target and composite with
// TONEMAP_COMPOSITE_FRAG. Fog is applied in linear space.
#ifdef AFFINE_UV
noperspective in vec2 vUV;
#else
in vec2 vUV;
#endif
in vec3 vWorldPos;
in vec3 vWorldNormal;
in vec3 vBakedLight;
in float vFogFactor;

out vec4 FragColor;

uniform sampler2D uTex;            // base colour, sRGB-encoded
uniform sampler2D uNormalMap;
uniform sampler2D uMetalRoughMap;
uniform int uHasNormalMap;
uniform int uHasMetalRoughMap;
uniform float uNormalScale;        // 1 = as authored
uniform vec4 uBaseColorFactor;     // multiplies the texture (linear rgb, alpha)
uniform float uRoughness;
uniform float uMetallic;
uniform vec3 uEmissive;            // linear, added after lighting
uniform int uUnlit;                // 1 = base colour only (UI-ish props)

uniform vec3 uCameraPos;
uniform vec3 uLightDir;            // toward the sun
uniform vec3 uLightColor;          // linear radiance of the sun
uniform vec3 uSkyColor;            // hemisphere ambient, up
uniform vec3 uGroundColor;         // hemisphere ambient, down
uniform vec3 uFogColor;            // linear

#define DUST_MAX_POINT_LIGHTS 8
uniform vec3 uPointLightPos[DUST_MAX_POINT_LIGHTS];
uniform vec3 uPointLightColor[DUST_MAX_POINT_LIGHTS];
uniform float uPointLightIntensity[DUST_MAX_POINT_LIGHTS];
uniform float uPointLightRange[DUST_MAX_POINT_LIGHTS];
uniform int uPointLightCount;

const float PI = 3.14159265;

vec3 srgbToLinear(vec3 c) { return pow(c, vec3(2.2)); }

mat3 cotangentFrame(vec3 N, vec3 p, vec2 uv) {
    vec3 dp1 = dFdx(p);
    vec3 dp2 = dFdy(p);
    vec2 duv1 = dFdx(uv);
    vec2 duv2 = dFdy(uv);
    vec3 dp2perp = cross(dp2, N);
    vec3 dp1perp = cross(N, dp1);
    vec3 T = dp2perp * duv1.x + dp1perp * duv2.x;
    vec3 B = dp2perp * duv1.y + dp1perp * duv2.y;
    float invmax = inversesqrt(max(max(dot(T, T), dot(B, B)), 1e-12));
    return mat3(T * invmax, B * invmax, N);
}

float distributionGGX(float NdotH, float a) {
    float a2 = a * a;
    float d = NdotH * NdotH * (a2 - 1.0) + 1.0;
    return a2 / max(PI * d * d, 1e-6);
}

float geometrySmith(float NdotV, float NdotL, float rough) {
    float k = (rough + 1.0) * (rough + 1.0) / 8.0;
    float gv = NdotV / (NdotV * (1.0 - k) + k);
    float gl = NdotL / (NdotL * (1.0 - k) + k);
    return gv * gl;
}

vec3 fresnelSchlick(float cosT, vec3 F0) {
    return F0 + (1.0 - F0) * pow(clamp(1.0 - cosT, 0.0, 1.0), 5.0);
}

vec3 brdf(vec3 N, vec3 V, vec3 L, vec3 radiance, vec3 albedo, float metal, float rough) {
    vec3 H = normalize(V + L);
    float NdotL = max(dot(N, L), 0.0);
    if (NdotL <= 0.0) return vec3(0.0);
    float NdotV = max(dot(N, V), 1e-4);
    float NdotH = max(dot(N, H), 0.0);
    vec3 F0 = mix(vec3(0.04), albedo, metal);
    vec3 F = fresnelSchlick(max(dot(H, V), 0.0), F0);
    float D = distributionGGX(NdotH, rough * rough);
    float G = geometrySmith(NdotV, NdotL, rough);
    vec3 spec = D * G * F / max(4.0 * NdotV * NdotL, 1e-4);
    vec3 kd = (1.0 - F) * (1.0 - metal);
    return (kd * albedo / PI + spec) * radiance * NdotL;
}

void main() {
    vec4 base = texture(uTex, vUV);
    vec3 albedo = srgbToLinear(base.rgb) * uBaseColorFactor.rgb;
    float alpha = base.a * uBaseColorFactor.a;
    if (uUnlit == 1) {
        FragColor = vec4(mix(albedo + uEmissive, uFogColor, vFogFactor), alpha);
        return;
    }

    vec3 N = normalize(vWorldNormal);
    if (!gl_FrontFacing) N = -N;
    if (uHasNormalMap == 1) {
        vec3 t = texture(uNormalMap, vUV).xyz * 2.0 - 1.0;
        t.xy *= uNormalScale;
        N = normalize(cotangentFrame(N, vWorldPos, vUV) * t);
    }
    float rough = clamp(uRoughness, 0.04, 1.0);
    float metal = clamp(uMetallic, 0.0, 1.0);
    if (uHasMetalRoughMap == 1) {
        vec3 mr = texture(uMetalRoughMap, vUV).rgb;
        rough = clamp(rough * mr.g, 0.04, 1.0);
        metal = clamp(metal * mr.b, 0.0, 1.0);
    }
    vec3 V = normalize(uCameraPos - vWorldPos);

    vec3 color = brdf(N, V, normalize(uLightDir), uLightColor, albedo, metal, rough);
    for (int i = 0; i < DUST_MAX_POINT_LIGHTS; i++) {
        if (i >= uPointLightCount) break;
        vec3 toL = uPointLightPos[i] - vWorldPos;
        float dist = length(toL);
        float range = max(uPointLightRange[i], 0.001);
        // smooth windowed inverse-square falloff, zero at `range`
        float win = clamp(1.0 - pow(dist / range, 4.0), 0.0, 1.0);
        float atten = win * win / (dist * dist + 1.0);
        vec3 radiance = uPointLightColor[i] * uPointLightIntensity[i] * atten;
        color += brdf(N, V, toL / max(dist, 1e-4), radiance, albedo, metal, rough);
    }

    // hemisphere ambient: diffuse part, plus a cheap specular part that
    // reflects the same sky/ground gradient (no environment map needed)
    vec3 ambient = mix(uGroundColor, uSkyColor, N.y * 0.5 + 0.5);
    vec3 F0 = mix(vec3(0.04), albedo, metal);
    vec3 Fa = fresnelSchlick(max(dot(N, V), 0.0), F0);
    vec3 R = reflect(-V, N);
    vec3 envSpec = mix(uGroundColor, uSkyColor, smoothstep(-0.2, 0.6, R.y)) * 1.5;
    color += ambient * albedo * (1.0 - metal) * (1.0 - Fa);
    color += envSpec * Fa * (1.0 - 0.6 * rough);
    color += vBakedLight * albedo;
    color += uEmissive;

    FragColor = vec4(mix(color, uFogColor, vFogFactor), alpha);
}
