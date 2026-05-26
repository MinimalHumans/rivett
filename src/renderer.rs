use egui_glow::glow;
use glow::HasContext;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

pub struct GammaRenderer {
    program: glow::Program,
    vertex_array: glow::VertexArray,
    vbo: glow::Buffer,
    /// GPU textures keyed by image path.
    tex_cache: HashMap<PathBuf, (glow::Texture, u32, u32)>,
    /// Which path is currently being displayed by paint().
    pub active_path: Option<PathBuf>,
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

            // a_pos: (x, y) coordinates for interpolation (0..1)
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
                tex_cache: HashMap::new(),
                active_path: None,
            }
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_program(self.program);
            gl.delete_vertex_array(self.vertex_array);
            gl.delete_buffer(self.vbo);
            for (tex, _, _) in self.tex_cache.values() {
                gl.delete_texture(*tex);
            }
        }
    }

    /// Upload f32 RGBA data to the GPU and store under `path`.
    /// Must be called from a PaintCallback (GL context live).
    pub fn cache_texture(&mut self, gl: &glow::Context, path: PathBuf, width: u32, height: u32, rgba_f32: &[f32]) {
        unsafe {
            // Delete any existing texture for this path.
            if let Some((old_tex, _, _)) = self.tex_cache.remove(&path) {
                gl.delete_texture(old_tex);
            }

            let tex = gl.create_texture().expect("Cannot create texture");
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

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

            self.tex_cache.insert(path, (tex, width, height));
        }
    }

    /// Switch the displayed image to `path`. Zero-cost — just updates a field.
    /// Returns true if the path is already in the GPU cache (navigation is instant).
    pub fn set_active(&mut self, path: PathBuf) -> bool {
        let found = self.tex_cache.contains_key(&path);
        self.active_path = Some(path);
        found
    }

    /// Check whether a GPU texture exists for `path`.
    pub fn has_cached(&self, path: &PathBuf) -> bool {
        self.tex_cache.contains_key(path)
    }

    /// Delete GPU textures for paths NOT in `keep`. Must be called from a PaintCallback.
    pub fn evict_stale(&mut self, gl: &glow::Context, keep: &HashSet<PathBuf>) {
        self.tex_cache.retain(|path, (tex, _, _)| {
            if keep.contains(path) {
                true
            } else {
                unsafe { gl.delete_texture(*tex); }
                false
            }
        });
    }

    /// Remove a specific path from the cache and clear active_path if it matches.
    /// Pass `gl = Some(...)` from a PaintCallback to immediately free GPU memory;
    /// pass `None` to defer cleanup to the next evict_stale call (at most one frame).
    pub fn invalidate(&mut self, gl: Option<&glow::Context>, path: &PathBuf) {
        if let Some((tex, _, _)) = self.tex_cache.remove(path) {
            if let Some(gl) = gl {
                unsafe { gl.delete_texture(tex); }
            }
            // else: leaks until evict_stale next frame — acceptable
        }
        if self.active_path.as_ref() == Some(path) {
            self.active_path = None;
        }
    }

    pub fn paint(&self, gl: &glow::Context, image_rect: egui::Rect, canvas_rect: egui::Rect, adj: crate::session::ImageAdjustments) {
        let tex = match &self.active_path {
            Some(p) => match self.tex_cache.get(p) {
                Some((tex, _, _)) => *tex,
                None => return,
            },
            None => return,
        };

        unsafe {
            gl.use_program(Some(self.program));

            let gamma_loc = gl.get_uniform_location(self.program, "u_gamma");
            gl.uniform_1_f32(gamma_loc.as_ref(), adj.gamma);

            let expo_loc = gl.get_uniform_location(self.program, "u_exposure");
            gl.uniform_1_f32(expo_loc.as_ref(), adj.exposure);

            let rmin_loc = gl.get_uniform_location(self.program, "u_remap_min");
            gl.uniform_1_f32(rmin_loc.as_ref(), adj.remap_min);

            let rmax_loc = gl.get_uniform_location(self.program, "u_remap_max");
            gl.uniform_1_f32(rmax_loc.as_ref(), adj.remap_max);

            let img_loc = gl.get_uniform_location(self.program, "u_image_rect");
            gl.uniform_4_f32(img_loc.as_ref(), image_rect.min.x, image_rect.min.y, image_rect.max.x, image_rect.max.y);

            let canvas_loc = gl.get_uniform_location(self.program, "u_canvas_rect");
            gl.uniform_4_f32(canvas_loc.as_ref(), canvas_rect.min.x, canvas_rect.min.y, canvas_rect.max.x, canvas_rect.max.y);

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let tex_loc = gl.get_uniform_location(self.program, "u_texture");
            gl.uniform_1_i32(tex_loc.as_ref(), 0);

            gl.bind_vertex_array(Some(self.vertex_array));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }
}
