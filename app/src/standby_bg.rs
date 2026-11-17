//! Fondo animado del stand-by (los mismos shaders que la versión web): aurora, ondas y estrellas.
//! Cambia cada 3 minutos con un fundido; durante el fundido se calculan las dos animaciones.

use glow::HasContext;

const VS: &str = r#"#version 300 es
void main() {
    vec2 p = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}"#;

const FS: &str = r#"#version 300 es
precision highp float;
uniform vec2 R; uniform vec2 O; uniform float T; uniform float A; uniform float B; uniform float M;
out vec4 color;
float h(vec2 p){ return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }
float n(vec2 p){ vec2 i = floor(p), f = fract(p); f = f*f*(3.-2.*f);
  return mix(mix(h(i), h(i+vec2(1,0)), f.x), mix(h(i+vec2(0,1)), h(i+vec2(1,1)), f.x), f.y); }
float fbm(vec2 p){ float v = 0., a = .5; for (int i = 0; i < 4; i++){ v += a*n(p); p = p*2.03 + vec2(1.7, 9.2); a *= .5; } return v; }
vec3 pal(float t){ t = fract(t);
  vec3 a = vec3(.478,.635,.969), b = vec3(.733,.604,.969), c = vec3(.490,.812,1.);
  return t < .33 ? mix(a, b, t/.33) : t < .66 ? mix(b, c, (t-.33)/.33) : mix(c, a, (t-.66)/.34); }
const vec3 BASE = vec3(.035, .038, .065);

vec3 aurora(vec2 uv, vec2 p){
  vec3 col = BASE + vec3(.0,.01,.03)*uv.y;
  for (int i = 0; i < 4; i++){ float fi = float(i);
    float yc = .32 + .13*fi + .22*(fbm(vec2(p.x*.7 + fi*4.1, T*.035 + fi)) - .5);
    float d = uv.y - yc;
    float core = exp(-d*d*120.);
    float rays = d > 0. ? exp(-d*5.) * (.35 + .65*fbm(vec2(p.x*7. + fi*3., T*.12))) : 0.;
    float flick = .55 + .45*fbm(vec2(p.x*3. - T*.08, fi*7.));
    col += pal(fi*.23 + p.x*.08 + T*.006) * (core*.55 + rays*.28) * flick * .55;
  }
  return col;
}
vec3 waves(vec2 uv, vec2 p){
  vec3 col = BASE;
  for (int i = 0; i < 7; i++){ float fi = float(i);
    float y = .5 + (fi - 3.)*.035
      + .17*sin(p.x*(1.1 + fi*.17) + T*(.22 + fi*.035) + fi*1.9) * (.6 + .4*sin(T*.11 + fi*1.3))
      + .05*sin(p.x*3.1 - T*.31 + fi);
    float d = abs(uv.y - y);
    col += pal(fi/7. + T*.01) * (.0035/(d + .0035)) * .16;
    col += pal(fi/7. + .2) * exp(-d*28.) * .05;
  }
  return col;
}
vec3 stars(vec2 uv, vec2 p){
  vec3 col = BASE + pal(.35 + fbm(p*1.5 + T*.01)*.6) * pow(fbm(p*1.8 - vec2(T*.008, 0.)), 2.5) * .35;
  for (int l = 0; l < 3; l++){ float fl = float(l);
    float sc = 7. + fl*7.;
    vec2 g = p*sc + vec2(T*.03*(fl + 1.), T*.006*(fl + 1.)) + fl*13.;
    vec2 id = floor(g), f = fract(g) - .5;
    float r = h(id);
    vec2 o = (vec2(h(id + 3.1), h(id + 7.7)) - .5)*.7;
    float d = length(f - o);
    float tw = .55 + .45*sin(T*(.8 + r*2.2) + r*40.);
    float on = step(.62, r);
    col += mix(vec3(1.), pal(r*3.), .5) * on * tw * (.012/(d*d*sc*.9 + .012)) * (.18 + fl*.12);
  }
  return col;
}
vec3 scene(float m, vec2 uv, vec2 p){
  if (m < .5) return aurora(uv, p);
  if (m < 1.5) return waves(uv, p);
  return stars(uv, p);
}
void main(){
  vec2 uv = (gl_FragCoord.xy - O) / R;
  vec2 p = vec2(uv.x * R.x / R.y, uv.y);
  vec3 col = scene(A, uv, p);
  if (M > .001) col = mix(col, scene(B, uv, p), M);
  float v = smoothstep(1.25, .25, length(uv - .5)*1.35);
  col *= mix(.55, 1., v);
  col += (h(gl_FragCoord.xy + fract(T)) - .5) / 255.;
  color = vec4(col, 1.);
}"#;

const BLIT_FS: &str = r#"#version 300 es
precision mediump float;
uniform sampler2D tex; uniform vec2 R; uniform vec2 O;
out vec4 color;
void main() { color = texture(tex, (gl_FragCoord.xy - O) / R); }"#;

/// Escala a la que se calcula el fondo (luego se amplía: es difuso y no se nota, y va mucho más fluido)
const SCALE: f32 = 0.35;

pub struct Background {
    pub gl_id: usize,
    program: glow::Program,
    blit: glow::Program,
    vao: glow::VertexArray,
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    size: (i32, i32),
}
unsafe impl Send for Background {}

impl Background {
    fn compile(gl: &glow::Context, fs: &str) -> glow::Program {
        unsafe {
            let program = gl.create_program().unwrap();
            for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, fs)] {
                let s = gl.create_shader(kind).unwrap();
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) { log::error!("stand-by shader: {}", gl.get_shader_info_log(s)); }
                gl.attach_shader(program, s);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) { log::error!("stand-by programa: {}", gl.get_program_info_log(program)); }
            program
        }
    }

    pub fn new(gl: &glow::Context, gl_id: usize) -> Background {
        unsafe {
            Background {
                gl_id,
                program: Self::compile(gl, FS),
                blit: Self::compile(gl, BLIT_FS),
                vao: gl.create_vertex_array().unwrap(),
                fbo: gl.create_framebuffer().unwrap(),
                tex: gl.create_texture().unwrap(),
                size: (0, 0),
            }
        }
    }

    /// viewport: (x, y, ancho, alto) en píxeles; t en segundos; a/b modos; m mezcla 0..1
    pub fn paint(&mut self, gl: &glow::Context, viewport: [f32; 4], t: f32, a: f32, b: f32, m: f32) {
        unsafe {
            let (sw, sh) = (((viewport[2] * SCALE) as i32).max(2), ((viewport[3] * SCALE) as i32).max(2));
            if self.size != (sw, sh) {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, sw, sh, 0, glow::RGBA, glow::UNSIGNED_BYTE,
                                glow::PixelUnpackData::Slice(None));
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.tex), 0);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.size = (sw, sh);
            }
            let prev_fb = gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_vertex_array(Some(self.vao));

            // 1) la animación, a baja resolución
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.viewport(0, 0, sw, sh);
            gl.use_program(Some(self.program));
            let u = |n: &str| gl.get_uniform_location(self.program, n);
            gl.uniform_2_f32(u("R").as_ref(), sw as f32, sh as f32);
            gl.uniform_2_f32(u("O").as_ref(), 0.0, 0.0);
            gl.uniform_1_f32(u("T").as_ref(), t);
            gl.uniform_1_f32(u("A").as_ref(), a);
            gl.uniform_1_f32(u("B").as_ref(), b);
            gl.uniform_1_f32(u("M").as_ref(), m);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // 2) ampliada a la pantalla (filtrado lineal)
            gl.bind_framebuffer(glow::FRAMEBUFFER, std::num::NonZeroU32::new(prev_fb as u32).map(glow::NativeFramebuffer));
            gl.viewport(viewport[0] as i32, viewport[1] as i32, viewport[2] as i32, viewport[3] as i32);
            gl.use_program(Some(self.blit));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
            let u = |n: &str| gl.get_uniform_location(self.blit, n);
            gl.uniform_1_i32(u("tex").as_ref(), 0);
            gl.uniform_2_f32(u("R").as_ref(), viewport[2], viewport[3]);
            gl.uniform_2_f32(u("O").as_ref(), viewport[0], viewport[1]);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.enable(glow::SCISSOR_TEST);
        }
    }
}
