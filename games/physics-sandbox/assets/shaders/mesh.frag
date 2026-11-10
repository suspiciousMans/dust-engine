#version 330 core
in float vLight;
uniform vec4 uColor;
out vec4 FragColor;
void main() {
    FragColor = vec4(uColor.rgb * vLight, 1.0);
}
