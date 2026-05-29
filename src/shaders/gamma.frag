#version 330 core

uniform sampler2D u_texture;
uniform float u_gamma;
uniform float u_exposure;
uniform float u_remap_min;
uniform float u_remap_max;

in vec2 v_tex_coord;
out vec4 out_color;

void main() {
    vec4 color = texture(u_texture, v_tex_coord);
    
    // 1. Apply remap (black/white points)
    // Formula: (x - min) / (max - min)
    float range = u_remap_max - u_remap_min;
    if (abs(range) > 1e-6) {
        color.rgb = (color.rgb - vec3(u_remap_min)) / range;
    }

    // 2. Apply exposure offset (stops)
    // Formula: x * 2^exposure
    color.rgb *= pow(2.0, u_exposure);
    
    // 3. Apply gamma correction
    // Formula: x^(1/gamma)
    if (u_gamma != 1.0 && u_gamma > 1e-6) {
        float inv_gamma = 1.0 / u_gamma;
        color.rgb = pow(max(color.rgb, vec3(0.0)), vec3(inv_gamma));
    }
    
    out_color = color;
}
