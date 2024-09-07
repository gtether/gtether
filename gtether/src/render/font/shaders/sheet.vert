#version 450

layout(location = 0) in vec2 position;
layout(location = 1) in float index;
layout(location = 2) in vec2 offset;
layout(location = 3) in vec2 scale;
layout(location = 4) in vec3 color;

layout(location = 0) out vec3 tex_coords;
layout(location = 1) out vec3 tex_color;

void main() {
    gl_Position = vec4(position * scale + offset, 0.0, 1.0);
    tex_coords = vec3(position.x, position.y, index);
    tex_color = color;
}