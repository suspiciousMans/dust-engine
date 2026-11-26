// Dust built-in "hi-fi" mesh vertex stage. No version directive: the
// ShaderVariantCache prepends it plus the AFFINE_UV / INSTANCED defines.
// Pass this pair through renderer::patch_mesh_shaders to receive shadows.
layout (location = 0) in vec3 aPos;
layout (location = 1) in vec3 aNormal;
layout (location = 2) in vec2 aUV;
layout (location = 3) in vec3 aColor;
#ifdef INSTANCED
layout (location = 4) in mat4 aModel;
#endif

uniform mat4 uModel;
uniform mat4 uView;
uniform mat4 uProj;
uniform float uFogStart;
uniform float uFogEnd;

#ifdef AFFINE_UV
noperspective out vec2 vUV;
#else
out vec2 vUV;
#endif
out vec3 vWorldPos;
out vec3 vWorldNormal;
out vec3 vBakedLight;
out float vFogFactor;

void main() {
#ifdef INSTANCED
    mat4 model = aModel;
#else
    mat4 model = uModel;
#endif
    vec4 worldPos = model * vec4(aPos, 1.0);
    vec4 viewPos = uView * worldPos;
    gl_Position = uProj * viewPos;
    vUV = aUV;
    vWorldPos = worldPos.xyz;
    // inverse-transpose keeps normals right under non-uniform scale
    vWorldNormal = normalize(transpose(inverse(mat3(model))) * aNormal);
    vBakedLight = aColor;
    float d = length(viewPos.xyz);
    vFogFactor = clamp((d - uFogStart) / max(uFogEnd - uFogStart, 0.001), 0.0, 1.0);
}
