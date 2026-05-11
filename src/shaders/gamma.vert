#version 300 es
precision mediump float;

uniform vec4 u_rect; // min_x, min_y, max_x, max_y in logical pixels
uniform vec2 u_screen_size; // width, height in logical pixels

layout (location = 0) in vec2 a_pos; // Normalized 0..1 coordinates

out vec2 v_tex_coord;

void main() {
    // Interpolate between rect min and max
    vec2 pos = mix(u_rect.xy, u_rect.zw, a_pos);
    
    // Convert from logical pixels [0, screen_size] to clip space [-1, 1]
    // Also flip Y because OpenGL is bottom-up
    vec2 clip_pos = (pos / u_screen_size) * 2.0 - 1.0;
    clip_pos.y = -clip_pos.y;
    
    gl_Position = vec4(clip_pos, 0.0, 1.0);
    v_tex_coord = vec2(a_pos.x, a_pos.y);
}
