#version 450

layout(set = 0, binding = 0) uniform sampler2D atlas;
layout(location = 0) in vec2 glyph_uv;
layout(location = 1) in vec4 glyph_color;
layout(location = 2) flat in uint glyph_render_mode;
layout(location = 3) flat in float glyph_color_scale;
layout(location = 0) out vec4 output_color;

void main() {
    vec4 sample_color = texture(atlas, glyph_uv);
    if (glyph_render_mode == 0) {
        float coverage = sample_color.r;
        output_color = vec4(glyph_color.rgb * glyph_color.a * coverage, glyph_color.a * coverage);
    } else {
        output_color = vec4(sample_color.rgb * sample_color.a * glyph_color_scale, sample_color.a);
    }
}
