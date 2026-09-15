use anyhow::{ensure, Context, Result};
use std::ffi::{c_char, c_void, CStr};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::ptr;

pub type Handle = *mut c_void;
type GetProc = unsafe extern "C" fn(*const c_char) -> *const c_void;

macro_rules! egl_api {
    ($($name:ident => $symbol:literal: $ty:ty),+ $(,)?) => {
        pub struct Api {
            $(pub $name: $ty,)+
            pub get_proc: GetProc,
            gbm_create: unsafe extern "C" fn(i32) -> Handle,
            gbm_destroy: unsafe extern "C" fn(Handle),
            _egl: libloading::Library,
            _gbm: libloading::Library,
        }

        impl Api {
            fn load() -> Result<Self> {
                // Copied entry points remain valid through the last display/context cleanup.
                unsafe {
                    let egl = libloading::Library::new("libEGL.so.1").context("Loading EGL for desktop capture")?;
                    let gbm = libloading::Library::new("libgbm.so.1").context("Loading GBM for desktop capture")?;
                    let get_proc = *egl.get::<GetProc>(b"eglGetProcAddress\0")?;
                    Ok(Self {
                        $($name: {
                            match egl.get::<$ty>(concat!($symbol, "\0").as_bytes()) {
                                Ok(symbol) => *symbol,
                                Err(_) => {
                                    let address = get_proc(concat!($symbol, "\0").as_ptr().cast());
                                    ensure!(!address.is_null(), "Missing EGL entry point {}", $symbol);
                                    std::mem::transmute::<*const c_void, $ty>(address)
                                }
                            }
                        },)+
                        get_proc,
                        gbm_create: *gbm.get(b"gbm_create_device\0")?,
                        gbm_destroy: *gbm.get(b"gbm_device_destroy\0")?,
                        _egl: egl,
                        _gbm: gbm,
                    })
                }
            }
        }
    };
}

egl_api! {
    get_display => "eglGetPlatformDisplayEXT": unsafe extern "C" fn(u32, Handle, *const i32) -> Handle,
    initialize => "eglInitialize": unsafe extern "C" fn(Handle, *mut i32, *mut i32) -> u32,
    terminate => "eglTerminate": unsafe extern "C" fn(Handle) -> u32,
    query_string => "eglQueryString": unsafe extern "C" fn(Handle, i32) -> *const c_char,
    bind_api => "eglBindAPI": unsafe extern "C" fn(u32) -> u32,
    choose_config => "eglChooseConfig": unsafe extern "C" fn(Handle, *const i32, *mut Handle, i32, *mut i32) -> u32,
    create_context => "eglCreateContext": unsafe extern "C" fn(Handle, Handle, Handle, *const i32) -> Handle,
    destroy_context => "eglDestroyContext": unsafe extern "C" fn(Handle, Handle) -> u32,
    make_current => "eglMakeCurrent": unsafe extern "C" fn(Handle, Handle, Handle, Handle) -> u32,
    get_current_context => "eglGetCurrentContext": unsafe extern "C" fn() -> Handle,
    create_image => "eglCreateImageKHR": unsafe extern "C" fn(Handle, Handle, u32, Handle, *const i32) -> Handle,
    destroy_image => "eglDestroyImageKHR": unsafe extern "C" fn(Handle, Handle) -> u32,
    query_modifiers => "eglQueryDmaBufModifiersEXT": unsafe extern "C" fn(Handle, i32, i32, *mut u64, *mut u32, *mut i32) -> u32,
}

pub struct ContextOwner {
    pub api: Api,
    pub display: Handle,
    context: Handle,
    gbm: Handle,
    initialized: bool,
    _device: File,
}

impl ContextOwner {
    pub fn device(&self) -> BorrowedFd<'_> {
        self._device.as_fd()
    }

    pub fn linear_buffer(&self, width: u32, height: u32) -> Result<LinearBuffer> {
        crate::frame::Mode {
            width,
            height,
            refresh_hz: 1,
        }
        .validate()?;
        unsafe {
            let create = self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle, u32, u32, u32, u32) -> Handle>(
                    b"gbm_bo_create\0",
                )?;
            let destroy = *self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle)>(b"gbm_bo_destroy\0")?;
            let get_fd = self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle) -> i32>(b"gbm_bo_get_fd\0")?;
            let get_stride = self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle) -> u32>(b"gbm_bo_get_stride\0")?;
            let get_offset = self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle, i32) -> u32>(b"gbm_bo_get_offset\0")?;
            let get_modifier = self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle) -> u64>(b"gbm_bo_get_modifier\0")?;
            let get_planes = self
                .api
                ._gbm
                .get::<unsafe extern "C" fn(Handle) -> i32>(b"gbm_bo_get_plane_count\0")?;
            let fourcc = u32::from_le_bytes(*b"AR24");
            let bo = create(self.gbm, width, height, fourcc, (1 << 2) | (1 << 4));
            ensure!(
                !bo.is_null(),
                "GPU cannot allocate a renderable linear RGB buffer"
            );
            let mut buffer = LinearBuffer {
                bo,
                destroy,
                image: crate::dmabuf::DmaBuffer {
                    width,
                    height,
                    fourcc,
                    format: crate::frame::PixelFormat::Argb8888,
                    modifier: 0,
                    planes: Vec::new(),
                },
            };
            ensure!(
                get_modifier(bo) == 0 && get_planes(bo) == 1,
                "GPU allocation is not packed linear RGB"
            );
            let fd = get_fd(bo);
            ensure!(fd >= 0, "GPU could not export its linear render buffer");
            let descriptor = OwnedFd::from_raw_fd(fd);
            ensure!(
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0,
                "Could not protect the exported render descriptor across exec"
            );
            buffer.image.planes.push(crate::dmabuf::Plane {
                pitch: get_stride(bo),
                offset: get_offset(bo, 0),
                allocation_bytes: crate::dmabuf::allocation_size(descriptor.as_fd())?,
                descriptor,
            });
            buffer.image.validate()?;
            Ok(buffer)
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name
                    .strip_prefix("renderD")
                    .is_some_and(|suffix| !suffix.is_empty()
                        && suffix.bytes().all(|byte| byte.is_ascii_digit()))),
            "GPU capture requires a render node, not a primary DRM device"
        );
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        ensure!(
            device.metadata()?.file_type().is_char_device(),
            "GPU capture requires a DRM render node"
        );
        let api = Api::load()?;
        let gbm = unsafe { (api.gbm_create)(device.as_raw_fd()) };
        ensure!(
            !gbm.is_null(),
            "GPU does not support GBM capture conversion"
        );
        let mut owner = Self {
            api,
            display: ptr::null_mut(),
            context: ptr::null_mut(),
            gbm,
            initialized: false,
            _device: device,
        };
        unsafe {
            // A separate GBM object gives each worker its own EGLDisplay lifetime.
            owner.display = (owner.api.get_display)(0x31d7, gbm, ptr::null());
            ensure!(!owner.display.is_null(), "No EGL display for capture GPU");
            let mut major = 0;
            let mut minor = 0;
            ensure!(
                (owner.api.initialize)(owner.display, &mut major, &mut minor) != 0,
                "Initializing capture EGL display failed"
            );
            owner.initialized = true;
            let extensions = (owner.api.query_string)(owner.display, 0x3055);
            ensure!(!extensions.is_null(), "Missing EGL capture capabilities");
            let extensions = CStr::from_ptr(extensions).to_str()?;
            for required in [
                "EGL_EXT_image_dma_buf_import",
                "EGL_EXT_image_dma_buf_import_modifiers",
                "EGL_KHR_surfaceless_context",
            ] {
                ensure!(
                    extensions
                        .split_ascii_whitespace()
                        .any(|extension| extension == required),
                    "Capture GPU requires {required}"
                );
            }
            ensure!(
                (owner.api.bind_api)(0x30a0) != 0,
                "OpenGL ES is unavailable for capture"
            );
            // Surfaceless GBM contexts do not require pbuffer-capable configurations.
            let attributes = [
                0x3040, 0x40, 0x3033, 0, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3038,
            ];
            let mut config = ptr::null_mut();
            let mut count = 0;
            ensure!(
                (owner.api.choose_config)(
                    owner.display,
                    attributes.as_ptr(),
                    &mut config,
                    1,
                    &mut count
                ) != 0
                    && count == 1,
                "No compatible OpenGL ES 3 capture configuration"
            );
            owner.context = (owner.api.create_context)(
                owner.display,
                config,
                ptr::null_mut(),
                [0x3098, 3, 0x3038].as_ptr(),
            );
            ensure!(
                !owner.context.is_null(),
                "Creating capture GPU context failed"
            );
            ensure!(
                (owner.api.make_current)(
                    owner.display,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    owner.context
                ) != 0,
                "Activating capture GPU context failed"
            );
        }
        Ok(owner)
    }

    pub fn modifier_target(
        &self,
        fourcc: u32,
        modifier: u64,
    ) -> Result<Option<super::TextureTarget>> {
        let mut count = 0;
        unsafe {
            ensure!(
                (self.api.query_modifiers)(
                    self.display,
                    fourcc as i32,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut count
                ) != 0,
                "GPU rejected the capture pixel format"
            );
            ensure!(
                (1..=1024).contains(&count),
                "Invalid GPU format modifier count"
            );
            let mut modifiers = vec![0u64; count as usize];
            let mut external_only = vec![0u32; count as usize];
            let capacity = count;
            ensure!(
                (self.api.query_modifiers)(
                    self.display,
                    fourcc as i32,
                    capacity,
                    modifiers.as_mut_ptr(),
                    external_only.as_mut_ptr(),
                    &mut count
                ) != 0
                    && count >= 0
                    && count <= capacity,
                "GPU format modifiers changed during negotiation"
            );
            Ok(super::TextureTarget::select(
                modifier,
                &modifiers[..count as usize],
                &external_only[..count as usize],
            ))
        }
    }

    pub fn activate(&self) -> Result<()> {
        ensure!(
            unsafe {
                (self.api.make_current)(
                    self.display,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    self.context,
                )
            } != 0,
            "Capture GPU context was lost"
        );
        Ok(())
    }
}

pub struct LinearBuffer {
    pub image: crate::dmabuf::DmaBuffer,
    bo: Handle,
    destroy: unsafe extern "C" fn(Handle),
}

impl Drop for LinearBuffer {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.bo);
        }
    }
}

impl Drop for ContextOwner {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                if (self.api.get_current_context)() == self.context {
                    (self.api.make_current)(
                        self.display,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                    );
                }
                if !self.context.is_null() {
                    (self.api.destroy_context)(self.display, self.context);
                }
                (self.api.terminate)(self.display);
            }
            (self.api.gbm_destroy)(self.gbm);
        }
    }
}
