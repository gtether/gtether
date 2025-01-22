#version 460

layout(location = 0) in vec3 position;
layout(location = 1) in vec3 normal;
layout(location = 2) in vec3 offset;
layout(location = 3) in vec3 color;

layout(location = 0) out vec3 out_color;
layout(location = 1) out vec3 out_normal;

layout(set = 0, binding = 0) uniform VP_Data {
    mat4 vp;
} vp_uniforms;

layout(set = 0, binding = 1) uniform Model_Data {
    mat4 model;
    mat4 normals;
} model;

void main() {
    gl_Position = vp_uniforms.vp * model.model * vec4((position + offset), 1.0);
    out_color = color;
    out_normal = mat3(model.normals) * normal;
}
