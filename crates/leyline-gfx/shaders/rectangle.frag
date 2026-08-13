#version 450

layout(location = 0) in vec4 rectangle_color;
layout(location = 0) out vec4 output_color;

void main() {
    output_color = rectangle_color;
}
