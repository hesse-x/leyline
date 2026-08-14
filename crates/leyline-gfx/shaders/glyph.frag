#version 450

layout(set = 0, binding = 0) uniform sampler2D atlas;
layout(location = 0) in vec2 glyph_uv;
layout(location = 1) in vec4 glyph_color;
layout(location = 0) out vec4 output_color;

void main() {
    float coverage = texture(atlas, glyph_uv).r;
    output_color = vec4(glyph_color.rgb * glyph_color.a * coverage, glyph_color.a * coverage);
}
