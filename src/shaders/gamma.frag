#version 300 es
precision mediump float;

uniform sampler2D u_texture;
uniform float u_gamma;

in vec2 v_tex_coord;
out vec4 out_color;

void main() {
    vec4 color = texture(u_texture, v_tex_coord);
    
    // Apply gamma correction to RGB channels
    // HDR values > 1.0 are naturally handled by pow()
    if (u_gamma != 1.0) {
        float inv_gamma = 1.0 / u_gamma;
        color.rgb = pow(max(color.rgb, vec3(0.0)), vec3(inv_gamma));
    }
    
    out_color = color;
}
