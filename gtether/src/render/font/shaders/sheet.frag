#version 450

layout(location = 0) in vec3 tex_coords;
layout(location = 1) in vec3 color;
layout(location = 0) out vec4 f_color;

layout(set = 0, binding = 0) uniform sampler2DArray s;

void main() {
    f_color = texture(s, tex_coords) * vec4(color, 1.0);
    if (f_color.w < 0.05) { discard; }
}