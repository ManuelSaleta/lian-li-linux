use super::{create_program, egl, sampling_shader, Converter, TextureTarget};
use crate::dmabuf::{DmaBuffer, DmaImage};
use anyhow::{ensure, Context, Result};
use glow::HasContext;
use std::os::fd::BorrowedFd;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

pub struct GpuFrame<'a> {
    pub image: &'a DmaBuffer,
    pub device: BorrowedFd<'a>,
    pub timestamp: Option<crate::frame::CaptureTimestamp>,
    pub damage: crate::frame::FrameDamage,
}

pub struct CursorLayer<'a> {
    pub image: &'a DmaImage,
    pub destination: [i32; 4],
    pub source: [u32; 4],
}

impl CursorLayer<'_> {
    fn coordinates(&self) -> Result<([f32; 4], [f32; 4])> {
        self.image.validate()?;
        ensure!(
            (1..=4096).contains(&self.destination[2]) && (1..=4096).contains(&self.destination[3]),
            "Invalid GPU cursor destination"
        );
        ensure!(
            self.image.width <= 1024
                && self.image.height <= 1024
                && self.source[2] > 0
                && self.source[3] > 0,
            "Invalid GPU cursor image or crop"
        );
        ensure!(
            u64::from(self.source[0]) + u64::from(self.source[2])
                <= u64::from(self.image.width) << 16
                && u64::from(self.source[1]) + u64::from(self.source[3])
                    <= u64::from(self.image.height) << 16,
            "GPU cursor crop exceeds its image"
        );
        let mut source = self.source.map(|value| value as f32 / 65536.0);
        source[0] /= self.image.width as f32;
        source[1] /= self.image.height as f32;
        source[2] /= self.image.width as f32;
        source[3] /= self.image.height as f32;
        Ok((self.destination.map(|value| value as f32), source))
    }
}

impl Converter {
    pub fn compose_export(
        &mut self,
        desktop: &DmaImage,
        cursor: Option<CursorLayer<'_>>,
        cancel: &AtomicBool,
    ) -> Result<GpuFrame<'_>> {
        self.owner.activate()?;
        self.targets = [None, None];
        self.prepare_image(desktop, cancel)?;
        let coordinates = cursor.as_ref().map(CursorLayer::coordinates).transpose()?;
        if let Some(cursor) = &cursor {
            self.prepare_image(cursor.image, cancel)?;
        }
        let targets = [
            self.formats[&(desktop.fourcc, desktop.modifier)],
            cursor
                .as_ref()
                .map_or(TextureTarget::TwoDimensional, |cursor| {
                    self.formats[&(cursor.image.fourcc, cursor.image.modifier)]
                }),
        ];
        if self
            .composition
            .as_ref()
            .is_none_or(|composition| composition.targets != targets)
        {
            let next = Composition::new(&self.gl, targets)?;
            if let Some(previous) = self.composition.replace(next) {
                unsafe {
                    self.gl.delete_program(previous.program);
                }
            }
        }
        if self.export_target.as_ref().is_none_or(|target| {
            (target.buffer.image.width, target.buffer.image.height)
                != (desktop.width, desktop.height)
        }) {
            self.export_target = None;
            self.export_target = Some(ExportTarget::new(self, desktop.width, desktop.height)?);
        }
        let primary = self.import(desktop)?;
        let overlay = cursor
            .as_ref()
            .map(|cursor| self.import(cursor.image))
            .transpose()?;
        let target = self.export_target.as_ref().unwrap();
        let composition = self.composition.as_ref().unwrap();
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, target.framebuffer);
            self.gl
                .viewport(0, 0, desktop.width as i32, desktop.height as i32);
            self.gl.disable(glow::BLEND);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::DITHER);
            self.gl.use_program(Some(composition.program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(primary.target.gl(), primary.texture);
            self.gl.uniform_1_i32(Some(&composition.desktop), 0);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(
                targets[1].gl(),
                overlay
                    .as_ref()
                    .map_or(primary.texture, |image| image.texture),
            );
            self.gl.uniform_1_i32(Some(&composition.cursor), 1);
            self.gl
                .uniform_1_i32(Some(&composition.visible), i32::from(cursor.is_some()));
            if let Some((destination, source)) = coordinates {
                self.gl
                    .uniform_4_f32_slice(Some(&composition.destination), &destination);
                self.gl
                    .uniform_4_f32_slice(Some(&composition.source), &source);
            }
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            ensure!(
                self.gl.get_error() == glow::NO_ERROR,
                "GPU desktop/cursor composition failed"
            );
        }
        self.wait_gpu(cancel)?;
        Ok(GpuFrame {
            image: &target.buffer.image,
            device: self.owner.device(),
            timestamp: None,
            damage: crate::frame::FrameDamage::Unknown,
        })
    }

    pub fn discard_export(&mut self) -> Result<()> {
        self.owner.activate()?;
        self.export_target = None;
        for _ in 0..8 {
            if unsafe { self.gl.get_error() } == glow::NO_ERROR {
                return Ok(());
            }
        }
        anyhow::bail!("Capture GPU error state did not clear after discarding failed output")
    }
}

pub(super) struct ExportTarget {
    gl: Rc<glow::Context>,
    framebuffer: Option<glow::Framebuffer>,
    texture: Option<glow::Texture>,
    image: egl::Handle,
    display: egl::Handle,
    destroy_image: unsafe extern "C" fn(egl::Handle, egl::Handle) -> u32,
    buffer: egl::LinearBuffer,
}

impl ExportTarget {
    fn new(converter: &Converter, width: u32, height: u32) -> Result<Self> {
        let buffer = converter.owner.linear_buffer(width, height)?;
        let mut imported = converter.import_as(&buffer.image, TextureTarget::TwoDimensional)?;
        let mut target = Self {
            gl: converter.gl.clone(),
            framebuffer: None,
            texture: imported.texture.take(),
            image: std::mem::replace(&mut imported.image, ptr::null_mut()),
            display: converter.owner.display,
            destroy_image: converter.owner.api.destroy_image,
            buffer,
        };
        unsafe {
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
                target.gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE
                    && target.gl.get_error() == glow::NO_ERROR,
                "Linear RGB buffer cannot be rendered by the capture GPU"
            );
        }
        Ok(target)
    }

    pub(super) fn forget_gl_objects(&mut self) {
        self.framebuffer = None;
        self.texture = None;
    }
}

impl Drop for ExportTarget {
    fn drop(&mut self) {
        unsafe {
            if let Some(framebuffer) = self.framebuffer {
                self.gl.delete_framebuffer(framebuffer);
            }
            if let Some(texture) = self.texture {
                self.gl.delete_texture(texture);
            }
            (self.destroy_image)(self.display, self.image);
        }
    }
}

pub(super) struct Composition {
    targets: [TextureTarget; 2],
    pub(super) program: glow::Program,
    desktop: glow::UniformLocation,
    cursor: glow::UniformLocation,
    visible: glow::UniformLocation,
    destination: glow::UniformLocation,
    source: glow::UniformLocation,
}

impl Composition {
    fn new(gl: &glow::Context, targets: [TextureTarget; 2]) -> Result<Self> {
        let source = sampling_shader(
            include_str!("compose.frag"),
            &[("desktop_image", targets[0]), ("cursor_image", targets[1])],
        );
        let program = create_program(gl, &source)?;
        let location = |name| {
            unsafe { gl.get_uniform_location(program, name) }
                .with_context(|| format!("GPU composition uniform {name} is missing"))
        };
        let result = (|| {
            Ok(Self {
                program,
                targets,
                desktop: location("desktop_image")?,
                cursor: location("cursor_image")?,
                visible: location("cursor_visible")?,
                destination: location("cursor_destination")?,
                source: location("cursor_source")?,
            })
        })();
        if result.is_err() {
            unsafe {
                gl.delete_program(program);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmabuf::Plane;
    use crate::frame::PixelFormat;

    #[test]
    fn cursor_crop_keeps_fixed_point_offsets_and_clipped_destination() {
        let image = DmaImage {
            buffer: DmaBuffer {
                width: 4,
                height: 2,
                fourcc: u32::from_le_bytes(*b"AR24"),
                format: PixelFormat::Argb8888,
                modifier: 0,
                planes: vec![Plane {
                    descriptor: tempfile::tempfile().unwrap().into(),
                    allocation_bytes: 32,
                    pitch: 16,
                    offset: 0,
                }],
            },
            fence: tempfile::tempfile().unwrap().into(),
        };
        let mut cursor = CursorLayer {
            image: &image,
            destination: [-1, 2, 4, 6],
            source: [1 << 16, 0, 2 << 16, 2 << 16],
        };
        let (destination, source) = cursor.coordinates().unwrap();
        assert_eq!(destination, [-1.0, 2.0, 4.0, 6.0]);
        assert_eq!(source, [0.25, 0.0, 0.5, 1.0]);
        cursor.source[0] = 3 << 16;
        assert!(cursor.coordinates().is_err());
        cursor.source[0] = 0;
        cursor.destination[2] = 0;
        assert!(cursor.coordinates().is_err());
    }
}
