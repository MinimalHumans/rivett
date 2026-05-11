use egui_glow::glow;
use glow::HasContext;
use std::sync::Arc;

pub struct GammaRenderer {
    program: glow::Program,
    vertex_array: glow::VertexArray,
    vbo: glow::Buffer,
    texture: Option<glow::Texture>,
    #[allow(dead_code)]
    tex_size: (u32, u32),
}

impl GammaRenderer {
    pub fn new(gl: &Arc<glow::Context>) -> Self {
        use glow::HasContext as _;
        unsafe {
            let program = gl.create_program().expect("Cannot create program");

            let shader_sources = [
                (glow::VERTEX_SHADER, include_str!("shaders/gamma.vert")),
                (glow::FRAGMENT_SHADER, include_str!("shaders/gamma.frag")),
            ];

            let shaders: Vec<_> = shader_sources
                .iter()
                .map(|(shader_type, shader_source)| {
                    let shader = gl.create_shader(*shader_type).expect("Cannot create shader");
                    gl.shader_source(shader, shader_source);
                    gl.compile_shader(shader);
                    if !gl.get_shader_compile_status(shader) {
                        panic!("{}", gl.get_shader_info_log(shader));
                    }
                    gl.attach_shader(program, shader);
                    shader
                })
                .collect();

            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                panic!("{}", gl.get_program_info_log(program));
            }

            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }

            let vertex_array = gl.create_vertex_array().expect("Cannot create vertex array");
            let vbo = gl.create_buffer().expect("Cannot create vbo");
            
            gl.bind_vertex_array(Some(vertex_array));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            
            // 0..1 quad
            let vertices: [f32; 8] = [
                0.0, 0.0,
                1.0, 0.0,
                0.0, 1.0,
                1.0, 1.0,
            ];
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck::cast_slice(&vertices), glow::STATIC_DRAW);
            
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            gl.enable_vertex_attrib_array(0);

            Self {
                program,
                vertex_array,
                vbo,
                texture: None,
                tex_size: (0, 0),
            }
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_program(self.program);
            gl.delete_vertex_array(self.vertex_array);
            gl.delete_buffer(self.vbo);
            if let Some(tex) = self.texture {
                gl.delete_texture(tex);
            }
        }
    }

    pub fn update_texture(&mut self, gl: &glow::Context, width: u32, height: u32, rgba_f32: &[f32]) {
        unsafe {
            if let Some(tex) = self.texture {
                gl.delete_texture(tex);
            }

            let tex = gl.create_texture().expect("Cannot create texture");
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            // Upload as RGBA32F
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA32F as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::FLOAT,
                Some(bytemuck::cast_slice(rgba_f32)),
            );

            self.texture = Some(tex);
            self.tex_size = (width, height);
        }
    }

    pub fn paint(&self, gl: &glow::Context, rect: egui::Rect, screen_size: egui::Vec2, gamma: f32) {
        unsafe {
            let Some(tex) = self.texture else { return };

            gl.use_program(Some(self.program));

            let gamma_loc = gl.get_uniform_location(self.program, "u_gamma");
            gl.uniform_1_f32(gamma_loc.as_ref(), gamma);

            let rect_loc = gl.get_uniform_location(self.program, "u_rect");
            gl.uniform_4_f32(rect_loc.as_ref(), rect.min.x, rect.min.y, rect.max.x, rect.max.y);

            let screen_loc = gl.get_uniform_location(self.program, "u_screen_size");
            gl.uniform_2_f32(screen_loc.as_ref(), screen_size.x, screen_size.y);

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let tex_loc = gl.get_uniform_location(self.program, "u_texture");
            gl.uniform_1_i32(tex_loc.as_ref(), 0);

            gl.bind_vertex_array(Some(self.vertex_array));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }
}
