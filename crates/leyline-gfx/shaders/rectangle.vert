#version 450

layout(location = 0) in vec2 origin_px;
layout(location = 1) in vec2 size_px;
layout(location = 2) in vec4 color;
layout(location = 0) out vec4 rectangle_color;

layout(push_constant) uniform Viewport {
    vec2 size_px;
} viewport;

const vec2 QUAD[6] = vec2[](
    vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    vec2(0.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0)
);

void main() {
    vec2 pixel = origin_px + QUAD[gl_VertexIndex] * size_px;
    vec2 clip = pixel / viewport.size_px * 2.0 - 1.0;
    gl_Position = vec4(clip, 0.0, 1.0);
    rectangle_color = color;
}
