#version 330 core
layout (location = 0) in vec3 aPos;
layout (location = 1) in vec3 aNormal;
uniform mat4 uModel;
uniform mat4 uView;
uniform mat4 uProj;
uniform vec3 uLightDir;
out float vLight;
void main() {
    gl_Position = uProj * uView * uModel * vec4(aPos, 1.0);
    vec3 worldNormal = normalize(mat3(uModel) * aNormal);
    vLight = max(dot(worldNormal, normalize(uLightDir)), 0.0) * 0.5 + 0.5;
}
