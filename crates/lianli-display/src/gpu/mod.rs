mod egl;
mod export;
pub use export::{CursorLayer, GpuFrame};

use crate::dmabuf::{DmaBuffer, DmaImage};
use anyhow::{ensure, Context, Result};
use glow::HasContext;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

type ImportTexture = unsafe extern "C" fn(u32, *mut c_void);
type UnmapBuffer = unsafe extern "C" fn(u32) -> u8;

pub struct Converter {
    targets: [Option<Target>; 2],
    export_target: Option<export::ExportTarget>,
    composition: Option<export::Composition>,
    program: glow::Program,
    external_program: Option<glow::Program>,
    gl: Rc<glow::Context>,
    import_texture: ImportTexture,
    unmap_buffer: UnmapBuffer,
    formats: HashMap<(u32, u64), TextureTarget>,
    owner: egl::ContextOwner,
}

#[derive(Clone, Copy)]
pub enum Layer {
    Desktop,
    Cursor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextureTarget {
    TwoDimensional,
    External,
}

impl TextureTarget {
    fn gl(self) -> u32 {
        match self {
            Self::TwoDimensional => glow::TEXTURE_2D,
            Self::External => 0x8d65,
        }
    }

    fn select(modifier: u64, modifiers: &[u64], external_only: &[u32]) -> Option<Self> {
        let mut target = None;
        for (&candidate, &external) in modifiers.iter().zip(external_only) {
            if candidate == modifier {
                if external == 0 {
                    return Some(Self::TwoDimensional);
                }
                target = Some(Self::External);
            }
        }
        target
    }
}

fn sampling_shader(source: &str, samplers: &[(&str, TextureTarget)]) -> String {
    let mut source = source.to_owned();
    if samplers
        .iter()
        .any(|(_, target)| *target == TextureTarget::External)
    {
        source = source.replacen(
            "#version 300 es",
            "#version 300 es\n#extension GL_OES_EGL_image_external_essl3 : require",
            1,
        );
        for (name, target) in samplers {
            if *target == TextureTarget::External {
                source = source.replace(
                    &format!("uniform sampler2D {name};"),
                    &format!("uniform samplerExternalOES {name};"),
                );
            }
        }
    }
    source
}

impl Converter {
    pub fn open(path: &Path) -> Result<Self> {
        let owner = egl::ContextOwner::open(path)?;
        let gl = Rc::new(unsafe {
            glow::Context::from_loader_function_cstr(|name| (owner.api.get_proc)(name.as_ptr()))
        });
        let renderer = unsafe { gl.get_parameter_string(glow::RENDERER) }.to_ascii_lowercase();
        ensure!(
            ![
                "llvmpipe",
                "softpipe",
                "swrast",
                "swiftshader",
                "software rasterizer"
            ]
            .iter()
            .any(|name| renderer.contains(name)),
            "GPU sampling is unavailable. CPU access to imported scanout is unsupported."
        );
        ensure!(
            gl.version().is_embedded && gl.version().major >= 3,
            "Desktop capture requires OpenGL ES 3"
        );
        ensure!(
            gl.supported_extensions().contains("GL_OES_EGL_image"),
            "GPU cannot sample imported DMA-BUF images"
        );
        let address = unsafe { (owner.api.get_proc)(c"glEGLImageTargetTexture2DOES".as_ptr()) };
        ensure!(
            !address.is_null(),
            "GPU image import entry point is unavailable"
        );
        let import_texture =
            unsafe { std::mem::transmute::<*const c_void, ImportTexture>(address) };
        let address = unsafe { (owner.api.get_proc)(c"glUnmapBuffer".as_ptr()) };
        ensure!(
            !address.is_null(),
            "GPU readback entry point is unavailable"
        );
        let unmap_buffer = unsafe { std::mem::transmute::<*const c_void, UnmapBuffer>(address) };
        let program = create_program(&gl, include_str!("capture.frag"))?;
        Ok(Self {
            targets: [None, None],
            export_target: None,
            composition: None,
            program,
            external_program: None,
            gl,
            import_texture,
            unmap_buffer,
            formats: HashMap::new(),
            owner,
        })
    }

    pub fn readback(
        &mut self,
        image: &DmaImage,
        layer: Layer,
        pixels: &mut Vec<u8>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.owner.activate()?;
        self.prepare_image(image, cancel)?;
        let index = match layer {
            Layer::Desktop => 0,
            Layer::Cursor => 1,
        };
        if self.targets[index]
            .as_ref()
            .is_none_or(|target| (target.width, target.height) != (image.width, image.height))
        {
            self.targets[index] = None;
            self.targets[index] = Some(Target::new(self.gl.clone(), image.width, image.height)?);
        }
        let imported = self.import(image)?;
        let target = self.targets[index].as_ref().unwrap();
        // Sample into our own GPU allocation before readback; never map the compositor's scanout.
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, target.framebuffer);
            self.gl
                .viewport(0, 0, target.width as i32, target.height as i32);
            self.gl.disable(glow::BLEND);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::DITHER);
            self.gl.use_program(Some(match imported.target {
                TextureTarget::TwoDimensional => self.program,
                TextureTarget::External => self
                    .external_program
                    .context("External capture shader is unavailable")?,
            }));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(imported.target.gl(), imported.texture);
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            self.gl
                .bind_buffer(glow::PIXEL_PACK_BUFFER, target.readback);
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(
                0,
                0,
                target.width as i32,
                target.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::BufferOffset(0),
            );
            ensure!(
                self.gl.get_error() == glow::NO_ERROR,
                "GPU capture conversion failed"
            );
        }
        self.wait_gpu(cancel)?;
        let bytes = target.width as usize * target.height as usize * 4;
        pixels.resize(bytes, 0);
        unsafe {
            let mapped = self.gl.map_buffer_range(
                glow::PIXEL_PACK_BUFFER,
                0,
                bytes as i32,
                glow::MAP_READ_BIT,
            );
            ensure!(
                !mapped.is_null(),
                "Mapping owned capture readback storage failed"
            );
            pixels.copy_from_slice(std::slice::from_raw_parts(mapped, bytes));
            let valid = (self.unmap_buffer)(glow::PIXEL_PACK_BUFFER) != 0;
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            ensure!(
                valid && self.gl.get_error() == glow::NO_ERROR,
                "Reading owned GPU capture storage failed"
            );
        }
        Ok(())
    }

    fn prepare_image(&mut self, image: &DmaImage, cancel: &AtomicBool) -> Result<()> {
        image.validate()?;
        image.wait_ready(Duration::from_millis(500), cancel)?;
        if !self.formats.contains_key(&(image.fourcc, image.modifier)) {
            ensure!(self.formats.len() < 64, "Too many capture format changes");
            let target = self
                .owner
                .modifier_target(image.fourcc, image.modifier)?
                .with_context(|| {
                    format!(
                        "GPU cannot sample DMA-BUF fourcc {:#010x}, modifier {:#018x}",
                        image.fourcc, image.modifier
                    )
                })?;
            if target == TextureTarget::External && self.external_program.is_none() {
                ensure!(self.gl.supported_extensions().contains("GL_OES_EGL_image_external_essl3"), "GPU requires external-image sampling for this DMA-BUF but lacks GL_OES_EGL_image_external_essl3");
                self.external_program = Some(create_program(
                    &self.gl,
                    &sampling_shader(include_str!("capture.frag"), &[("source", target)]),
                )?);
            }
            self.formats.insert((image.fourcc, image.modifier), target);
        }
        Ok(())
    }

    fn import<'a>(&'a self, image: &DmaBuffer) -> Result<Imported<'a>> {
        let target = self
            .formats
            .get(&(image.fourcc, image.modifier))
            .copied()
            .unwrap_or(TextureTarget::TwoDimensional);
        self.import_as(image, target)
    }

    fn import_as<'a>(&'a self, image: &DmaBuffer, target: TextureTarget) -> Result<Imported<'a>> {
        let attributes = import_attributes(image)?;
        let raw = unsafe {
            (self.owner.api.create_image)(
                self.owner.display,
                ptr::null_mut(),
                0x3270,
                ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        ensure!(
            !raw.is_null(),
            "EGL rejected the synchronized DMA-BUF image"
        );
        let mut imported = Imported {
            converter: self,
            image: raw,
            texture: None,
            target,
        };
        unsafe {
            imported.texture = Some(self.gl.create_texture().map_err(anyhow::Error::msg)?);
            self.gl.bind_texture(target.gl(), imported.texture);
            self.gl
                .tex_parameter_i32(target.gl(), glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            self.gl
                .tex_parameter_i32(target.gl(), glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            self.gl.tex_parameter_i32(
                target.gl(),
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                target.gl(),
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            (self.import_texture)(target.gl(), raw);
            ensure!(
                self.gl.get_error() == glow::NO_ERROR,
                "GPU rejected imported capture texture"
            );
        }
        Ok(imported)
    }

    fn wait_gpu(&self, cancel: &AtomicBool) -> Result<()> {
        let fence = unsafe { self.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0) }
            .map_err(anyhow::Error::msg)?;
        let result = (|| {
            unsafe {
                self.gl.flush();
            }
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                ensure!(
                    !cancel.load(Ordering::Relaxed),
                    "GPU capture conversion cancelled"
                );
                ensure!(
                    Instant::now() < deadline,
                    "GPU capture conversion timed out"
                );
                match unsafe { self.gl.client_wait_sync(fence, 0, 50_000_000) } {
                    glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => return Ok(()),
                    glow::TIMEOUT_EXPIRED => {}
                    _ => anyhow::bail!("GPU capture synchronization failed"),
                }
            }
        })();
        unsafe {
            self.gl.delete_sync(fence);
        }
        result
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        if self.owner.activate().is_ok() {
            unsafe {
                self.gl.delete_program(self.program);
                if let Some(program) = self.external_program.take() {
                    self.gl.delete_program(program);
                }
            }
            self.targets = [None, None];
            self.export_target = None;
            if let Some(composition) = self.composition.take() {
                unsafe {
                    self.gl.delete_program(composition.program);
                }
            }
        } else {
            // Context destruction releases resources when a lost context cannot accept GL calls.
            for target in self.targets.iter_mut().flatten() {
                target.texture = None;
                target.framebuffer = None;
                target.readback = None;
            }
            if let Some(target) = &mut self.export_target {
                target.forget_gl_objects();
            }
        }
    }
}

struct Imported<'a> {
    target: TextureTarget,
    converter: &'a Converter,
    image: egl::Handle,
    texture: Option<glow::Texture>,
}

impl Drop for Imported<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(texture) = self.texture {
                self.converter.gl.delete_texture(texture);
            }
            if !self.image.is_null() {
                (self.converter.owner.api.destroy_image)(self.converter.owner.display, self.image);
            }
        }
    }
}

struct Target {
    gl: Rc<glow::Context>,
    texture: Option<glow::Texture>,
    framebuffer: Option<glow::Framebuffer>,
    readback: Option<glow::Buffer>,
    width: u32,
    height: u32,
}

impl Target {
    fn new(gl: Rc<glow::Context>, width: u32, height: u32) -> Result<Self> {
        let mut target = Self {
            gl,
            texture: None,
            framebuffer: None,
            readback: None,
            width,
            height,
        };
        unsafe {
            target.texture = Some(target.gl.create_texture().map_err(anyhow::Error::msg)?);
            target.gl.bind_texture(glow::TEXTURE_2D, target.texture);
            target.gl.tex_storage_2d(
                glow::TEXTURE_2D,
                1,
                glow::RGBA8,
                width as i32,
                height as i32,
            );
            target.framebuffer = Some(target.gl.create_framebuffer().map_err(anyhow::Error::msg)?);
            target
                .gl
                .bind_framebuffer(glow::FRAMEBUFFER, target.framebuffer);
            target.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                target.texture,
                0,
            );
            ensure!(
                target.gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE,
                "GPU capture target is incomplete"
            );
            target.readback = Some(target.gl.create_buffer().map_err(anyhow::Error::msg)?);
            target
                .gl
                .bind_buffer(glow::PIXEL_PACK_BUFFER, target.readback);
            target.gl.buffer_data_size(
                glow::PIXEL_PACK_BUFFER,
                (width * height * 4) as i32,
                glow::STREAM_READ,
            );
            ensure!(
                target.gl.get_error() == glow::NO_ERROR,
                "Allocating bounded GPU capture storage failed"
            );
            target.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }
        Ok(target)
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        unsafe {
            if let Some(buffer) = self.readback {
                self.gl.delete_buffer(buffer);
            }
            if let Some(framebuffer) = self.framebuffer {
                self.gl.delete_framebuffer(framebuffer);
            }
            if let Some(texture) = self.texture {
                self.gl.delete_texture(texture);
            }
        }
    }
}

fn import_attributes(image: &DmaBuffer) -> Result<Vec<i32>> {
    image.validate()?;
    let mut attributes = vec![
        0x3057,
        image.width as i32,
        0x3056,
        image.height as i32,
        0x3271,
        image.fourcc as i32,
    ];
    for (index, plane) in image.planes.iter().enumerate() {
        let base = [0x3272, 0x3275, 0x3278, 0x3440][index];
        let modifier = 0x3443 + index as i32 * 2;
        attributes.extend([
            base,
            plane.descriptor.as_raw_fd(),
            base + 1,
            i32::try_from(plane.offset).context("GPU plane offset is too large")?,
            base + 2,
            i32::try_from(plane.pitch).context("GPU plane pitch is too large")?,
            modifier,
            image.modifier as u32 as i32,
            modifier + 1,
            (image.modifier >> 32) as u32 as i32,
        ]);
    }
    attributes.push(0x3038);
    Ok(attributes)
}

fn create_program(gl: &glow::Context, fragment: &str) -> Result<glow::Program> {
    const VERTEX: &str = include_str!("capture.vert");
    let mut shaders = Vec::new();
    let mut program = None;
    let result = (|| unsafe {
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX),
            (glow::FRAGMENT_SHADER, fragment),
        ] {
            let shader = gl.create_shader(kind).map_err(anyhow::Error::msg)?;
            shaders.push(shader);
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            ensure!(
                gl.get_shader_compile_status(shader),
                "Capture GPU shader failed: {}",
                gl.get_shader_info_log(shader)
            );
        }
        let linked = gl.create_program().map_err(anyhow::Error::msg)?;
        program = Some(linked);
        for shader in &shaders {
            gl.attach_shader(linked, *shader);
        }
        gl.link_program(linked);
        ensure!(
            gl.get_program_link_status(linked),
            "Capture GPU program failed: {}",
            gl.get_program_info_log(linked)
        );
        Ok(linked)
    })();
    unsafe {
        for shader in shaders {
            if let Some(program) = program {
                gl.detach_shader(program, shader);
            }
            gl.delete_shader(shader);
        }
        if result.is_err() {
            if let Some(program) = program {
                gl.delete_program(program);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmabuf::Plane;
    use crate::frame::PixelFormat;

    #[test]
    fn external_only_modifiers_are_usable_without_treating_unknown_formats_as_supported() {
        assert_eq!(
            TextureTarget::select(17, &[17], &[1]),
            Some(TextureTarget::External)
        );
        assert_eq!(
            TextureTarget::select(17, &[17, 17], &[1, 0]),
            Some(TextureTarget::TwoDimensional)
        );
        assert_eq!(TextureTarget::select(18, &[17], &[0]), None);
        assert_eq!(TextureTarget::select(17, &[], &[]), None);
    }

    #[test]
    fn import_attributes_preserve_offsets_and_both_modifier_halves() {
        let image = DmaImage {
            buffer: DmaBuffer {
                width: 2,
                height: 3,
                format: PixelFormat::Xrgb8888,
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: 0xabcdef0112345678,
                planes: vec![Plane {
                    descriptor: tempfile::tempfile().unwrap().into(),
                    pitch: 64,
                    offset: 128,
                    allocation_bytes: 4096,
                }],
            },
            fence: tempfile::tempfile().unwrap().into(),
        };
        let attributes = import_attributes(&image).unwrap();
        assert_eq!(
            &attributes[..6],
            &[0x3057, 2, 0x3056, 3, 0x3271, image.fourcc as i32]
        );
        assert_eq!(
            &attributes[8..],
            &[
                0x3273,
                128,
                0x3274,
                64,
                0x3443,
                0x12345678,
                0x3444,
                0xabcdef01u32 as i32,
                0x3038
            ]
        );
    }
}
