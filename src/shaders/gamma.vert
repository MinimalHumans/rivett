#version 300 es
precision mediump float;

uniform vec4 u_image_rect;  // [min_x, min_y, max_x, max_y] in logical pixels
uniform vec4 u_canvas_rect; // [min_x, min_y, max_x, max_y] in logical pixels
uniform int  u_rotation;    // 0: 0, 1: 90 CW, 2: 180, 3: 270 CW

layout (location = 0) in vec2 a_pos; // 0..1 coordinates

out vec2 v_tex_coord;

void main() {
    // Interpolate to find logical pixel position of this vertex
    vec2 pos = mix(u_image_rect.xy, u_image_rect.zw, a_pos);
    
    // Map from logical pixel [canvas_min, canvas_max] to [0, 1]
    vec2 canvas_size = u_canvas_rect.zw - u_canvas_rect.xy;
    vec2 rel_pos = (pos - u_canvas_rect.xy) / canvas_size;
    
    // Map from [0, 1] to [-1, 1] clip space
    // Flip Y because OpenGL is bottom-up
    float x = rel_pos.x * 2.0 - 1.0;
    float y = 1.0 - rel_pos.y * 2.0;
    
    gl_Position = vec4(x, y, 0.0, 1.0);
    
    // UV transformation based on rotation
    // a_pos is (0,0) top-left, (1,1) bottom-right in the quad
    if (u_rotation == 1) { // 90 CW
        v_tex_coord = vec2(a_pos.y, 1.0 - a_pos.x);
    } else if (u_rotation == 2) { // 180
        v_tex_coord = vec2(1.0 - a_pos.x, 1.0 - a_pos.y);
    } else if (u_rotation == 3) { // 270 CW
        v_tex_coord = vec2(1.0 - a_pos.y, a_pos.x);
    } else { // 0
        v_tex_coord = a_pos;
    }
}
