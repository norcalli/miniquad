use anyhow::{anyhow, ensure, Result};
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString},
    path::PathBuf,
    ptr::{copy_nonoverlapping, null, null_mut, NonNull},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use libc::c_void;

use crate::{
    native::{egl, Clipboard, NativeDisplayData},
    EventHandler, KeyMods,
};

use super::module;

use keyboard_layouts::{available_layouts, string_to_keys_and_modifiers};

#[derive(Debug)]
#[repr(C)]
pub struct drmModeRes {
    count_fbs: i32,
    fbs: *const u32,
    count_crtcs: i32,
    crtcs: *const u32,

    count_connectors: i32,
    connectors: *const u32,

    count_encoders: i32,
    encoders: *const u32,

    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

impl drmModeRes {
    pub unsafe fn connector_ids(&self) -> &[u32] {
        std::slice::from_raw_parts(self.connectors, self.count_connectors as usize)
    }

    pub unsafe fn encoder_ids(&self) -> &[u32] {
        std::slice::from_raw_parts(self.encoders, self.count_encoders as usize)
    }

    pub unsafe fn crtc_ids(&self) -> &[u32] {
        std::slice::from_raw_parts(self.crtcs, self.count_crtcs as usize)
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct drmModeEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(C)]
pub enum drmModeConnection {
    DRM_MODE_CONNECTED = 1,
    DRM_MODE_DISCONNECTED = 2,
    DRM_MODE_UNKNOWNCONNECTION = 3,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(C)]
pub enum drmModeSubPixel {
    DRM_MODE_SUBPIXEL_UNKNOWN = 1,
    DRM_MODE_SUBPIXEL_HORIZONTAL_RGB = 2,
    DRM_MODE_SUBPIXEL_HORIZONTAL_BGR = 3,
    DRM_MODE_SUBPIXEL_VERTICAL_RGB = 4,
    DRM_MODE_SUBPIXEL_VERTICAL_BGR = 5,
    DRM_MODE_SUBPIXEL_NONE = 6,
}

pub const DRM_DISPLAY_MODE_LEN: usize = 32;

#[derive(Debug)]
#[repr(C)]
pub struct drmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,

    pub vrefresh: u32,

    pub flags: u32,
    pub r#type: u32,
    pub name: [u8; DRM_DISPLAY_MODE_LEN],
}

impl drmModeModeInfo {
    pub fn name(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.name).unwrap()
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct drmModeConnector {
    connector_id: u32,
    encoder_id: u32,
    /**< Encoder currently connected to */
    connector_type: u32,
    connector_type_id: u32,
    connection: drmModeConnection,
    mmWidth: u32,
    mmHeight: u32,
    /**< HxW in millimeters */
    subpixel: drmModeSubPixel,

    count_modes: i32,
    modes: drmModeModeInfoPtr,

    count_props: i32,
    /**< List of property ids */
    props: *const u32,

    /**< List of property values */
    prop_values: *const u64,

    count_encoders: i32,
    /**< List of encoder ids */
    encoders: *const u32,
}

impl drmModeConnector {
    pub unsafe fn modes(&self) -> &[drmModeModeInfo] {
        std::slice::from_raw_parts(self.modes, self.count_modes as usize)
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct drmModeCrtc {
    crtc_id: u32,
    /**< FB id to connect to 0 = disconnect */
    buffer_id: u32,

    /**< Position on the framebuffer */
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    mode_valid: i32,
    mode: drmModeModeInfo,

    /**< Number of gamma stops */
    gamma_size: i32,
}

pub type drmModeConnectorPtr = *const drmModeConnector;
pub type drmModeModeInfoPtr = *const drmModeModeInfo;
pub type drmModeResPtr = *const drmModeRes;
pub type drmModeEncoderPtr = *const drmModeEncoder;
pub type drmModeCrtcPtr = *const drmModeCrtc;

/**
 * Retrieves all of the resources associated with a card.
 */
pub type drmModeGetResources = unsafe extern "C" fn(i32) -> drmModeResPtr;

/**
 * Retrieve all information about the connector connectorId. This will do a
 * forced probe on the connector to retrieve remote information such as EDIDs
 * from the display device.
 */
pub type drmModeGetConnector =
    unsafe extern "C" fn(fd: i32, connectorId: u32) -> drmModeConnectorPtr;
pub type drmModeGetEncoder = unsafe extern "C" fn(fd: i32, encoderId: u32) -> drmModeEncoderPtr;
pub type drmModeGetCrtc = unsafe extern "C" fn(fd: i32, crtcId: u32) -> drmModeCrtcPtr;
pub type drmHandleEvent = unsafe extern "C" fn(fd: i32, evctx: *const drmEventContext) -> i32;

/**
 * Set the mode on a crtc crtcId with the given mode modeId.
 */
pub type drmModeSetCrtc = unsafe extern "C" fn(
    fd: i32,
    crtcI: u32,
    bufferId: u32,
    x: u32,
    y: u32,
    connectors: *const u32,
    connector_count: i32,
    mode: drmModeModeInfoPtr,
) -> i32;

pub type drmModeFreeModeInfo = unsafe extern "C" fn(drmModeModeInfoPtr);
pub type drmModeFreeResources = unsafe extern "C" fn(drmModeResPtr);
// pub type drmModeFreeFB = unsafe extern "C" fn(drmModeFBPtr);
// pub type drmModeFreeFB2 = unsafe extern "C" fn(drmModeFB2Ptr);
pub type drmModeFreeCrtc = unsafe extern "C" fn(drmModeCrtcPtr);
pub type drmModeFreeConnector = unsafe extern "C" fn(drmModeConnectorPtr);
pub type drmModeFreeEncoder = unsafe extern "C" fn(drmModeEncoderPtr);
// pub type drmModeFreePlane = unsafe extern "C" fn(drmModePlanePtr);
// pub type drmModeFreePlaneResources = unsafe extern "C" fn(drmModePlaneResPtr);

/**
 * Creates a new framebuffer with an buffer object as its scanout buffer.
 */
pub type drmModeAddFB = unsafe extern "C" fn(
    fd: i32,
    width: u32,
    height: u32,
    depth: u8,
    bpp: u8,
    pitch: u32,
    bo_handle: u32,
    buf_id: *mut u32,
) -> i32;
pub type drmModeRmFB = unsafe extern "C" fn(fd: i32, buf_id: u32) -> i32;
pub type drmModePageFlip = unsafe extern "C" fn(
    fd: i32,
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    user_data: *mut c_void,
) -> i32;
pub type drmSetMaster = unsafe extern "C" fn(fd: i32) -> i32;

#[derive(Clone)]
pub struct LibDrm {
    pub module: std::rc::Rc<module::Module>,
    pub drmModeGetResources: drmModeGetResources,
    pub drmModeFreeResources: drmModeFreeResources,
    pub drmModeGetConnector: drmModeGetConnector,
    pub drmModeFreeModeInfo: drmModeFreeModeInfo,
    pub drmModeFreeConnector: drmModeFreeConnector,
    pub drmModeFreeEncoder: drmModeFreeEncoder,
    pub drmModeGetEncoder: drmModeGetEncoder,
    pub drmModeGetCrtc: drmModeGetCrtc,
    pub drmModeFreeCrtc: drmModeFreeCrtc,
    pub drmModeSetCrtc: drmModeSetCrtc,
    pub drmModeAddFB: drmModeAddFB,
    pub drmModeRmFB: drmModeRmFB,
    pub drmHandleEvent: drmHandleEvent,
    pub drmModePageFlip: drmModePageFlip,
    pub drmSetMaster: drmSetMaster,
}

impl LibDrm {
    pub fn try_load() -> Option<Self> {
        crate::native::module::Module::load("libdrm.so")
            // .or_else(|_| crate::native::module::Module::load("libX11.so.6"))
            .map(|module| Self {
                drmModeGetResources: module.get_symbol("drmModeGetResources").unwrap(),
                drmModeFreeResources: module.get_symbol("drmModeFreeResources").unwrap(),
                drmModeGetConnector: module.get_symbol("drmModeGetConnector").unwrap(),
                drmModeFreeModeInfo: module.get_symbol("drmModeFreeModeInfo").unwrap(),
                drmModeFreeConnector: module.get_symbol("drmModeFreeConnector").unwrap(),
                drmModeGetEncoder: module.get_symbol("drmModeGetEncoder").unwrap(),
                drmModeFreeEncoder: module.get_symbol("drmModeFreeEncoder").unwrap(),
                drmModeGetCrtc: module.get_symbol("drmModeGetCrtc").unwrap(),
                drmModeFreeCrtc: module.get_symbol("drmModeFreeCrtc").unwrap(),
                drmModeSetCrtc: module.get_symbol("drmModeSetCrtc").unwrap(),
                drmModeAddFB: module.get_symbol("drmModeAddFB").unwrap(),
                drmModeRmFB: module.get_symbol("drmModeRmFB").unwrap(),
                drmHandleEvent: module.get_symbol("drmHandleEvent").unwrap(),
                drmModePageFlip: module.get_symbol("drmModePageFlip").unwrap(),
                drmSetMaster: module.get_symbol("drmSetMaster").unwrap(),
                module: std::rc::Rc::new(module),
            })
            .ok()
    }
}

pub struct gbm_device;
pub struct gbm_surface;
pub struct gbm_bo;

pub type GbmDestroyCallback = unsafe extern "C" fn(*mut gbm_bo, *mut c_void);

pub struct LibGbm {
    pub module: std::rc::Rc<module::Module>,
    pub gbm_create_device: unsafe extern "C" fn(fd: i32) -> *const gbm_device,
    pub gbm_device_is_format_supported:
        unsafe extern "C" fn(gbm: *const gbm_device, format: u32, flags: u32) -> i32,
    pub gbm_surface_create: unsafe extern "C" fn(
        gbm: *const gbm_device,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut gbm_surface,
    pub gbm_surface_release_buffer:
        unsafe extern "C" fn(surface: *mut gbm_surface, bo: *mut gbm_bo),
    pub gbm_surface_lock_front_buffer:
        unsafe extern "C" fn(surface: *mut gbm_surface) -> *mut gbm_bo,
    pub gbm_bo_get_user_data: unsafe extern "C" fn(bo: *mut gbm_bo) -> *mut c_void,
    pub gbm_bo_set_user_data:
        unsafe extern "C" fn(bo: *mut gbm_bo, data: *mut c_void, destructor: GbmDestroyCallback),
    pub gbm_bo_get_width: unsafe extern "C" fn(bo: *mut gbm_bo) -> u32,
    pub gbm_bo_get_height: unsafe extern "C" fn(bo: *mut gbm_bo) -> u32,
    pub gbm_bo_get_stride: unsafe extern "C" fn(bo: *mut gbm_bo) -> u32,
    pub gbm_bo_get_handle: unsafe extern "C" fn(bo: *mut gbm_bo) -> gbm_bo_handle,
    pub gbm_device_get_fd: unsafe extern "C" fn(gbm: *const gbm_device) -> i32,
    pub gbm_bo_get_device: unsafe extern "C" fn(bo: *mut gbm_bo) -> *mut gbm_device,
    pub gbm_surface_destroy: unsafe extern "C" fn(surface: *const gbm_surface),
    pub gbm_device_destroy: unsafe extern "C" fn(device: *const gbm_device),
}

#[repr(C)]
pub union gbm_bo_handle {
    pub ptr: *mut c_void,
    pub s32: i32,
    pub u32: u32,
    pub s64: i64,
    pub u64: u64,
}

impl LibGbm {
    pub fn try_load() -> Option<Self> {
        crate::native::module::Module::load("libgbm.so")
            .map(|module| Self {
                gbm_create_device: module.get_symbol("gbm_create_device").unwrap(),
                gbm_device_is_format_supported: module
                    .get_symbol("gbm_device_is_format_supported")
                    .unwrap(),
                gbm_surface_create: module.get_symbol("gbm_surface_create").unwrap(),
                gbm_surface_release_buffer: module
                    .get_symbol("gbm_surface_release_buffer")
                    .unwrap(),
                gbm_surface_lock_front_buffer: module
                    .get_symbol("gbm_surface_lock_front_buffer")
                    .unwrap(),
                gbm_bo_get_user_data: module.get_symbol("gbm_bo_get_user_data").unwrap(),
                gbm_bo_set_user_data: module.get_symbol("gbm_bo_set_user_data").unwrap(),
                gbm_bo_get_width: module.get_symbol("gbm_bo_get_width").unwrap(),
                gbm_bo_get_height: module.get_symbol("gbm_bo_get_height").unwrap(),
                gbm_bo_get_stride: module.get_symbol("gbm_bo_get_stride").unwrap(),
                gbm_bo_get_handle: module.get_symbol("gbm_bo_get_handle").unwrap(),
                gbm_device_get_fd: module.get_symbol("gbm_device_get_fd").unwrap(),
                gbm_bo_get_device: module.get_symbol("gbm_bo_get_device").unwrap(),
                gbm_surface_destroy: module.get_symbol("gbm_surface_destroy").unwrap(),
                gbm_device_destroy: module.get_symbol("gbm_device_destroy").unwrap(),
                module: std::rc::Rc::new(module),
            })
            .ok()
    }
}

unsafe fn errno() -> i32 {
    *libc::__errno_location()
}

pub fn run<F>(conf: &crate::conf::Conf, f: &mut Option<F>) -> Option<()>
where
    F: 'static + FnOnce() -> Box<dyn EventHandler>,
{
    run2(conf, f).ok()
}

pub fn run2<F>(conf: &crate::conf::Conf, f: &mut Option<F>) -> anyhow::Result<()>
where
    F: 'static + FnOnce() -> Box<dyn EventHandler>,
{
    unsafe {
        let libdrm = LibDrm::try_load().ok_or_else(|| anyhow!("Failed to load libdrm"))?;

        let (resources, drm_fd, path) = find_device_resource(&libdrm);
        // TODO use this?
        ensure!(0 == (libdrm.drmSetMaster)(drm_fd));

        dbg!(&resources);
        dbg!(resources.connector_ids());
        let connector = find_connected_connector(&libdrm, &resources, drm_fd)
            .ok_or_else(|| anyhow!("No connector found"))?;
        for mode in connector.modes() {
            eprintln!(
                "mode: {}, {}x{}@{}",
                mode.name().to_str().unwrap(),
                mode.hdisplay,
                mode.vdisplay,
                mode.vrefresh
            );
        }
        // let mode = &connector.modes()[0];
        let encoder = find_encoder_for_connector(&libdrm, &resources, drm_fd, &connector)
            .ok_or_else(|| anyhow!("Failed to find encoder for connector"))?;
        dbg!(encoder);
        let crtcs = find_crtcs_for_connector(&libdrm, &resources, drm_fd, &connector);
        for (encoder, crtcs) in crtcs.iter() {
            eprintln!("Encoder: {encoder:?}");
            for crtc in crtcs.iter() {
                eprintln!(
                    "Crtc {:?}: {}x{}+{}+{}@{}; {crtc:?}",
                    crtc.mode.name().to_str().unwrap(),
                    crtc.width,
                    crtc.height,
                    crtc.x,
                    crtc.y,
                    crtc.mode.vrefresh
                )
            }
        }
        let crtc = &crtcs[0].1[0];
        let status = (libdrm.drmModeSetCrtc)(
            drm_fd,
            crtc.crtc_id,
            // TODO buffer_id?
            // -1
            u32::MAX,
            // crtc.buffer_id,
            // TODO crtc.x, crtc.y?
            0,
            0,
            &connector.connector_id,
            1,
            &crtc.mode,
        );
        if status != 0 {
            if status == -libc::EINVAL {
                panic!("Crtc ID is invalid");
            } else if status == -1 {
                panic!(
                    "If count is invalid, or the list specified by connectors \
                is incompatible with the CRTC."
                );
            } else {
                let message = strerror(status);
                let message = message.to_str().unwrap_or_default();
                panic!(
                    "{}",
                    format!(
                        "Mode set failed for {crtc:?}.\n\
                    Status code: {status}. Message {message:?}"
                    )
                );
            }
            std::process::exit(1);
        }
        eprintln!("Successfully set crtc to {crtc:?}");

        let libegl = egl::LibEgl::try_load().ok_or_else(|| anyhow!("Failed to load libegl"))?;
        let qs = (libegl.eglQueryString.unwrap())(egl::EGL_NO_DISPLAY, egl::EGL_EXTENSIONS as i32);
        let qs = CStr::from_ptr(qs).to_str().unwrap();
        let extensions: BTreeSet<&str> = qs.split_whitespace().collect();
        eprintln!("egl client extensions: {extensions:?}");
        ensure!(extensions.contains("EGL_EXT_platform_base"));
        ensure!(libegl.eglGetProcAddress.is_some());
        let libgbm = LibGbm::try_load().ok_or_else(|| anyhow!("Failed to load libgbm"))?;
        let device = (libgbm.gbm_create_device)(drm_fd);
        ensure!(!device.is_null(), "Failed to create gbm device");
        const GBM_FORMAT_XRGB8888: u32 = 0x34325258;
        let surface_format: u32 = GBM_FORMAT_XRGB8888;
        let surface_flags: u32 =
            gbm_bo_flags::GBM_BO_USE_SCANOUT as u32 | gbm_bo_flags::GBM_BO_USE_RENDERING as u32;
        ensure!(
            (libgbm.gbm_device_is_format_supported)(device, surface_format, surface_flags) == 1
        );
        let surface = (libgbm.gbm_surface_create)(
            device,
            crtc.width,
            crtc.height,
            surface_format,
            surface_flags,
        );
        ensure!(!surface.is_null(), "Failed to create gbm surface");
        let display = (libegl.eglGetPlatformDisplayEXT.unwrap())(
            egl::EGL_PLATFORM_GBM_MESA as i32,
            device as *const c_void,
            std::ptr::null(),
        );
        ensure!(!display.is_null(), "Failed to create egl display");
        let mut major = 0i32;
        let mut minor = 0i32;
        ensure!((libegl.eglInitialize.unwrap())(display, &mut major, &mut minor) == 1);
        eprintln!("Initialized egl {major}.{minor}");
        let eglQueryString = |code: u32| {
            let result = (libegl.eglQueryString.unwrap())(display, code as i32);
            if result.is_null() {
                None
            } else {
                Some(CStr::from_ptr(result).to_str().unwrap())
            }
        };
        eprintln!(
            "EGL_VENDOR = {}",
            eglQueryString(egl::EGL_VENDOR).unwrap_or("")
        );
        eprintln!(
            "EGL_VERSION = {}",
            eglQueryString(egl::EGL_VERSION).unwrap_or("")
        );
        eprintln!(
            "EGL_CLIENT_APIS = {}",
            eglQueryString(egl::EGL_CLIENT_APIS).unwrap_or("")
        );
        eprintln!(
            "EGL_EXTENSIONS = {}",
            eglQueryString(egl::EGL_EXTENSIONS).unwrap_or("")
        );
        let (egl_context, egl_config, egl_display) =
            egl::create_egl_context(&libegl, display, conf.platform.framebuffer_alpha, false)
                .expect("Failed to make egl context");
        assert_eq!(egl_display, display);
        let egl_surface = (libegl.eglCreatePlatformWindowSurfaceEXT.unwrap())(
            display,
            egl_config,
            surface as _,
            null(),
        );
        assert!(!egl_surface.is_null());
        assert!(
            1 == (libegl.eglMakeCurrent.unwrap())(display, egl_surface, egl_surface, egl_context)
        );
        eprintln!("Initialized egl!");
        crate::native::gl::load_gl_funcs(|proc| {
            let name = std::ffi::CString::new(proc).unwrap();
            libegl.eglGetProcAddress.expect("non-null function pointer")(name.as_ptr() as _)
        });

        let (tx, rx) = std::sync::mpsc::channel();
        // let clipboard = Box::new(clipboard::X11Clipboard::new(
        //     display.libx11.clone(),
        //     display.display,
        //     display.window,
        // ));
        struct MemoryClipboard(String);
        impl Clipboard for MemoryClipboard {
            fn get(&mut self) -> Option<String> {
                if self.0.is_empty() {
                    None
                } else {
                    Some(self.0.clone())
                }
            }

            fn set(&mut self, string: &str) {
                self.0.clear();
                self.0.push_str(string);
            }
        }

        let clipboard = Box::new(MemoryClipboard(String::new()));
        let (w, h) = (crtc.width as i32, crtc.height as i32);
        crate::set_display(NativeDisplayData {
            high_dpi: conf.high_dpi,
            dpi_scale: 1.0,
            // dpi_scale: display.libx11.update_system_dpi(display.display),
            ..NativeDisplayData::new(w, h, tx, clipboard)
        });

        static WAITING_FOR_PAGE_FLIP: AtomicBool = AtomicBool::new(false);

        extern "C" fn page_flip_set(
            fd: i32,
            sequence: u32,
            tv_sec: u32,
            tv_usec: u32,
            user_data: *mut c_void,
        ) {
            let duration =
                Duration::from_micros(tv_usec as u64) + Duration::from_secs(tv_sec as u64);
            // eprintln!("Got flip: {sequence}, {:.6}", duration.as_secs_f64());
            WAITING_FOR_PAGE_FLIP.store(false, Ordering::SeqCst);
        }

        let page_flip_ctx = drmEventContext {
            version: 4,
            page_flip_handler: Some(page_flip_set),
            ..Default::default()
        };
        let mut last_bo = None;

        let wait_for_flip = || {
            while WAITING_FOR_PAGE_FLIP.load(Ordering::SeqCst) {
                let mut poll = libc::pollfd {
                    fd: drm_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let timeout = -1;
                let count = libc::poll(&mut poll, 1, timeout);
                if count < 0 {
                    let errno = errno();
                    if errno == libc::EINTR {
                        continue;
                    }
                    panic!("Poll error: {}", strerror(errno).to_str().unwrap());
                }
                if (poll.revents & libc::POLLIN) != 0 {
                    // TODO check result
                    (libdrm.drmHandleEvent)(drm_fd, &page_flip_ctx);
                } else if count == 0 {
                    // timeout reached
                    return false;
                }
            }
            true
        };
        let mut output_present = || {
            if let Some(last_bo) = last_bo.take() {
                // let dri = (libgbm.gbm_device_get_fd)((libgbm.gbm_bo_get_device)(bo));
                (libgbm.gbm_surface_release_buffer)(surface, last_bo);
                // assert!(0 == (libdrm.drmModeRmFB)(drm_fd, fb));
            }
            assert!((libegl.eglSwapBuffers.unwrap())(egl_display, egl_surface) == 1);
            let bo = (libgbm.gbm_surface_lock_front_buffer)(surface);
            last_bo = Some(bo);
            assert!(!bo.is_null());
            pub struct BoUser {
                drmModeRmFb: drmModeRmFB,
                drm_fd: i32,
                fb: u32,
            }
            let bo_user = (libgbm.gbm_bo_get_user_data)(bo);
            let fb = if bo_user.is_null() {
                let width: u32 = (libgbm.gbm_bo_get_width)(bo);
                let height: u32 = (libgbm.gbm_bo_get_height)(bo);
                let stride: u32 = (libgbm.gbm_bo_get_stride)(bo);
                let fd: u32 = (libgbm.gbm_bo_get_handle)(bo).u32;
                let mut fb = 0;
                // int ok = drmModeAddFB(Drm->Fd, width, height, 24, 32, stride, fd, &Fb);
                assert!(
                    0 == (libdrm.drmModeAddFB)(drm_fd, width, height, 24, 32, stride, fd, &mut fb)
                );
                assert!(
                    0 == (libdrm.drmModeSetCrtc)(
                        drm_fd,
                        crtc.crtc_id,
                        fb,
                        0,
                        0,
                        &connector.connector_id,
                        1,
                        &crtc.mode
                    )
                );
                unsafe extern "C" fn cleanup_fb_handler(_bo: *mut gbm_bo, user: *mut c_void) {
                    let user: Box<BoUser> = Box::from_raw(user as *mut _);
                    (user.drmModeRmFb)(user.drm_fd, user.fb);
                }
                (libgbm.gbm_bo_set_user_data)(
                    bo,
                    Box::into_raw(Box::new(BoUser {
                        drmModeRmFb: libdrm.drmModeRmFB,
                        drm_fd,
                        fb,
                    })) as *mut _,
                    cleanup_fb_handler,
                );
                fb
            } else {
                (&*(bo_user as *mut BoUser)).fb
            };
            /**
             * DRM_MODE_PAGE_FLIP_EVENT
             *
             * Request that the kernel sends back a vblank event (see
             * struct drm_event_vblank) with the &DRM_EVENT_FLIP_COMPLETE type when the
             * page-flip is done.
             */
            pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x1;
            if 0 == (libdrm.drmModePageFlip)(
                drm_fd,
                crtc.crtc_id,
                fb,
                DRM_MODE_PAGE_FLIP_EVENT,
                null_mut(),
            ) {
                WAITING_FOR_PAGE_FLIP.store(true, Ordering::SeqCst);
            }
        };

        let mut event_handler = (f.take().unwrap())();
        let mut start = Instant::now();
        let (mut keyboards, mut mice) = find_devices();
        dbg!(&keyboards, &mice);
        // crate::native_display().try_lock().unwrap().quit_ordered = true;
        // dbg!(available_layouts());
        // let mut lookup = std::collections::HashMap::new();
        // for c in 0..char::MAX as u32 {
        //     let Some(c) = char::from_u32(c) else { continue };
        //     let mut buf = [0u8; 4];
        //     let Ok(keys) =
        //         string_to_keys_and_modifiers("LAYOUT_US_ENGLISH", c.encode_utf8(&mut buf)) else {
        //         continue;
        //     };
        //     if c == 'A' {
        //         println!("{:?}", (c, &keys));
        //     }
        //     if c == 'a' {
        //         println!("{:?}", (c, &keys));
        //     }
        //     if keys.len() == 1 {
        //         // lookup.insert(c, keys[0]);
        //     } else if keys.len() > 1 {
        //         // dbg!((c, keys));
        //     }
        // }
        let mut poll = vec![];
        for kbd in &keyboards {
            poll.push(libc::pollfd {
                fd: kbd.fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for mouse in &mice {
            poll.push(libc::pollfd {
                fd: mouse.fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        'main: while !crate::native_display().try_lock().unwrap().quit_ordered {
            if keyboards.is_empty() && start.elapsed().as_secs() > 1 {
                break;
            }
            while let Ok(request) = rx.try_recv() {
                match request {
                    crate::native::Request::SetCursorGrab(_) => {
                        // TODO idk what this means
                    }
                    crate::native::Request::ShowMouse(_) => {
                        // TODO hardware cursor
                    }
                    crate::native::Request::SetMouseCursor(_) => {
                        // TODO hardware cursor
                    }
                    crate::native::Request::SetWindowSize {
                        new_width,
                        new_height,
                    } => {
                        // TODO look up crtc for mode set
                    }
                    // Irrelevant
                    crate::native::Request::ShowKeyboard(_) => (),
                    // We're always fullscreen
                    crate::native::Request::SetFullscreen(_) => (),
                }
                // dbg!(request);
            }
            {
                for poll in poll.iter_mut() {
                    poll.revents = 0;
                }
                let timeout = Duration::from_millis(3);
                // TODO what unit is timeout?
                let count = libc::poll(
                    poll.as_mut_ptr(),
                    poll.len() as u64,
                    timeout.as_millis() as i32,
                );
                if count == 0 {
                    // timeout
                } else if count < 0 {
                    assert!(
                        errno() == libc::EINTR,
                        "Poll error: {}",
                        strerror(errno()).to_string_lossy()
                    );
                } else {
                    let mut event_buf = [libc::input_event {
                        time: libc::timeval {
                            tv_sec: 0,
                            tv_usec: 0,
                        },
                        type_: 0,
                        code: 0,
                        value: 0,
                    }; 16];
                    for (poll, kbd) in poll.iter().zip(keyboards.iter_mut()) {
                        if (poll.revents & libc::POLLIN) != 0 {
                            match read_input_events(poll, &mut event_buf) {
                                Ok(events) => {
                                    for event in events.iter() {
                                        if let Some((keycode, action)) = key_from_event(event) {
                                            if action != KeyAction::Repeat {
                                                match keycode {
                                                    KeyCode::KEY_LEFTCTRL
                                                    | KeyCode::KEY_RIGHTCTRL => {
                                                        kbd.key_mods.ctrl = action != KeyAction::Up;
                                                    }
                                                    KeyCode::KEY_LEFTALT
                                                    | KeyCode::KEY_RIGHTALT => {
                                                        kbd.key_mods.alt = action != KeyAction::Up;
                                                    }
                                                    KeyCode::KEY_LEFTSHIFT
                                                    | KeyCode::KEY_RIGHTSHIFT => {
                                                        kbd.key_mods.shift =
                                                            action != KeyAction::Up;
                                                    }
                                                    KeyCode::KEY_LEFTMETA
                                                    | KeyCode::KEY_RIGHTMETA => {
                                                        kbd.key_mods.logo = action != KeyAction::Up;
                                                    }
                                                    // key if key >= KeyCode::KEY_A
                                                    //     && key <= KeyCode::KEY_Z
                                                    //     && action != KeyAction::Up =>
                                                    // {
                                                    //     let offset = keycode as u8
                                                    //         - KeyCode::KEY_A as u8
                                                    //         + if kbd.key_mods.shift {
                                                    //             b'A'
                                                    //         } else {
                                                    //             b'a'
                                                    //         };
                                                    //     event_handler.char_event(
                                                    //         offset as char,
                                                    //         kbd.key_mods,
                                                    //         action == KeyAction::Repeat,
                                                    //     );
                                                    // }
                                                    _ => (),
                                                }
                                            }
                                            if action == KeyAction::Repeat
                                                || action == KeyAction::Down
                                            {
                                                if let Some((c, shift_c)) =
                                                    chars_from_keycode(keycode)
                                                {
                                                    let c = if kbd.key_mods.shift {
                                                        shift_c
                                                    } else {
                                                        c
                                                    };
                                                    event_handler.char_event(
                                                        c,
                                                        kbd.key_mods,
                                                        action == KeyAction::Repeat,
                                                    );
                                                }
                                            }
                                            fn shortmods(
                                                buffer: &mut [u8; 4],
                                                mods: KeyMods,
                                            ) -> &str {
                                                let mut len = 0;
                                                if mods.alt {
                                                    buffer[len] = b'A';
                                                    len += 1;
                                                }
                                                if mods.shift {
                                                    buffer[len] = b'S';
                                                    len += 1;
                                                }
                                                if mods.ctrl {
                                                    buffer[len] = b'C';
                                                    len += 1;
                                                }
                                                if mods.logo {
                                                    buffer[len] = b'M';
                                                    len += 1;
                                                }
                                                unsafe {
                                                    std::str::from_utf8_unchecked(&buffer[..len])
                                                }
                                            }
                                            crate::debug!(
                                                "Key: {:4} {:?}",
                                                shortmods(&mut [0u8; 4], kbd.key_mods),
                                                (action, keycode)
                                            );
                                            if {
                                                let m = kbd.key_mods;
                                                m.alt && m.ctrl && m.shift && m.logo
                                            } && action == KeyAction::Down
                                                && keycode == KeyCode::KEY_ESC
                                            {
                                                break 'main;
                                            }
                                            if let Some(keycode) = keycode.to_macroquad_keycode() {
                                                match action {
                                                    KeyAction::Up => {
                                                        event_handler
                                                            .key_up_event(keycode, kbd.key_mods);
                                                    }
                                                    KeyAction::Down => {
                                                        event_handler.key_down_event(
                                                            keycode,
                                                            kbd.key_mods,
                                                            false,
                                                        );
                                                    }
                                                    KeyAction::Repeat => {
                                                        event_handler
                                                            .key_up_event(keycode, kbd.key_mods);
                                                        event_handler.key_down_event(
                                                            keycode,
                                                            kbd.key_mods,
                                                            false,
                                                            // true,
                                                        );
                                                    }
                                                }
                                            }
                                            // eprintln!(
                                            //     "key event: name: {}, input_event: {:?}",
                                            //     kbd.name,
                                            //     (keycode, action)
                                            // );
                                        }
                                    }
                                }
                                Err(errno) => {
                                    eprintln!(
                                        "Failed to read from {}: {}",
                                        kbd.name,
                                        strerror(errno).to_string_lossy()
                                    );
                                }
                            }
                        }
                    }
                    for (poll, mouse) in poll.iter().skip(keyboards.len()).zip(mice.iter_mut()) {
                        if (poll.revents & libc::POLLIN) != 0 {
                            match read_input_events(poll, &mut event_buf) {
                                Ok(events) => {
                                    let old_pos = (mouse.x, mouse.y);
                                    let mut touch_x = None;
                                    let mut touch_y = None;
                                    for event in events.iter() {
                                        // eprintln!(
                                        //     "mouse {}. type: {}. code: {}, value: {}",
                                        //     mouse.name, event.type_, event.code, event.value
                                        // );
                                        if event.type_ == EventType::EV_ABS as u16 {
                                            // eprintln!("ABS: {}, {}", event.code, event.value);
                                            match event.code {
                                                x if x == AbsAxis::ABS_X as u16 => {
                                                    let abs_screen_pos = event.value as f32;
                                                    // / mouse.x_abs_info.maximum as f32
                                                    // * w as f32;
                                                    let pos = touch_x.get_or_insert((
                                                        abs_screen_pos,
                                                        abs_screen_pos,
                                                    ));
                                                    pos.1 = abs_screen_pos;
                                                }
                                                x if x == AbsAxis::ABS_Y as u16 => {
                                                    let abs_screen_pos = event.value as f32;
                                                    // / mouse.y_abs_info.maximum as f32
                                                    // * h as f32;
                                                    let pos = touch_y.get_or_insert((
                                                        abs_screen_pos,
                                                        abs_screen_pos,
                                                    ));
                                                    pos.1 = abs_screen_pos;
                                                }
                                                _ => (),
                                            }
                                        }
                                    }
                                    let (touch_start_x, touch_stop_x) = touch_x.unwrap_or_default();
                                    let (touch_start_y, touch_stop_y) = touch_y.unwrap_or_default();
                                    mouse.x += (touch_stop_x - touch_start_x);
                                    mouse.y += (touch_stop_y - touch_start_y);
                                    mouse.x = mouse.x.clamp(0.0, w as f32);
                                    mouse.y = mouse.y.clamp(0.0, h as f32);
                                    if old_pos != (mouse.x, mouse.y) {
                                        // event_handler.mouse_motion_event(mouse.x, mouse.y);
                                        event_handler.mouse_motion_event(
                                            mouse.x,
                                            mouse.y,
                                            // mouse.x - old_pos.0,
                                            // mouse.y - old_pos.1,
                                        );
                                    }
                                    for event in events.iter() {
                                        if event.type_ == EventType::EV_KEY as u16 {
                                            let button = if event.code
                                                == MouseButton::BTN_LEFT as u16
                                            {
                                                crate::MouseButton::Left
                                            } else if event.code == MouseButton::BTN_RIGHT as u16 {
                                                crate::MouseButton::Right
                                            } else if event.code == MouseButton::BTN_MIDDLE as u16 {
                                                crate::MouseButton::Middle
                                            } else {
                                                continue;
                                            };
                                            if event.value == 0 {
                                                event_handler.mouse_button_up_event(
                                                    button, mouse.x, mouse.y,
                                                );
                                            } else {
                                                event_handler.mouse_button_down_event(
                                                    button, mouse.x, mouse.y,
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(errno) => {
                                    eprintln!(
                                        "Failed to read from {}: {}",
                                        mouse.name,
                                        strerror(errno).to_string_lossy()
                                    );
                                }
                            }
                        }
                    }
                }

                // TODO what's the point of this code?
                let mut d = crate::native_display().try_lock().unwrap();
                if d.quit_requested && !d.quit_ordered {
                    drop(d);
                    event_handler.quit_requested_event();
                    let mut d = crate::native_display().try_lock().unwrap();
                    if d.quit_requested {
                        d.quit_ordered = true
                    }
                }
            }
            event_handler.update();
            event_handler.draw();
            output_present();
            wait_for_flip();
        }

        // Cleanup
        {
            (libegl.eglMakeCurrent.unwrap())(egl_display, null_mut(), null_mut(), null_mut());
            (libegl.eglDestroyContext.unwrap())(egl_display, egl_context);
            (libegl.eglDestroySurface.unwrap())(egl_display, egl_surface);
            (libegl.eglTerminate.unwrap())(egl_display);
            assert!(
                0 == (libdrm.drmModeSetCrtc)(
                    drm_fd,
                    crtc.crtc_id,
                    crtc.buffer_id,
                    crtc.x,
                    crtc.y,
                    &connector.connector_id,
                    1,
                    &crtc.mode
                )
            );
            if let Some(last_bo) = last_bo.take() {
                // let dri = (libgbm.gbm_device_get_fd)((libgbm.gbm_bo_get_device)(bo));
                (libgbm.gbm_surface_release_buffer)(surface, last_bo);
                // assert!(0 == (libdrm.drmModeRmFB)(drm_fd, fb));
            }
            (libgbm.gbm_surface_destroy)(surface);
            (libgbm.gbm_device_destroy)(device);
            libc::close(drm_fd);
        }
        Ok(())
    }
}

pub fn key_from_event(event: &libc::input_event) -> Option<(KeyCode, KeyAction)> {
    if event.type_ == EventType::EV_KEY as u16 {
        let action = match event.value {
            x if x == KeyAction::Up as i32 => KeyAction::Up,
            x if x == KeyAction::Down as i32 => KeyAction::Down,
            x if x == KeyAction::Repeat as i32 => KeyAction::Repeat,
            _ => return None,
        };
        let keycode = KeyCode::from_code(event.code).unwrap_or(KeyCode::KEY_UNKNOWN);
        Some((keycode, action))
    } else {
        None
    }
}

unsafe fn read_input_events<'a>(
    poll: &libc::pollfd,
    event_buf: &'a mut [libc::input_event; 16],
) -> Result<&'a [libc::input_event], i32> {
    let read_status = libc::read(
        poll.fd,
        event_buf.as_mut_ptr() as *mut _,
        std::mem::size_of_val(&*event_buf),
    );
    if read_status < 0 {
        // eprintln!("Had fail on {}", kbd.name);
        Err(errno())
    } else {
        Ok(&event_buf[..read_status as usize / std::mem::size_of_val(&event_buf[0])])
    }
}

const EVIOCGRAB: u64 = 0x40044590;
const EVIOCGREP: u64 = 0x80084503;
const EVIOCSREP: u64 = 0x40084503;
const fn EVIOCGABS(abs: AbsAxis) -> u64 {
    0x80184540 + abs as u64
}
#[repr(u64)]
enum AbsAxis {
    ABS_X = 0x00,
    ABS_Y = 0x01,
    ABS_Z = 0x02,
    ABS_MT_POSITION_X = 0x35,
    ABS_MT_POSITION_Y = 0x36,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
#[repr(u16)]
pub enum KeyAction {
    Up,
    Down,
    Repeat,
}

pub enum EventType {
    EV_SYN = 0x00,
    EV_KEY = 0x01,
    EV_REL = 0x02,
    EV_ABS = 0x03,
    EV_MSC = 0x04,
    EV_SW = 0x05,
    EV_LED = 0x11,
    EV_SND = 0x12,
    EV_REP = 0x14,
    EV_FF = 0x15,
    EV_PWR = 0x16,
    EV_FF_STATUS = 0x17,
    EV_MAX = 0x1f,
    EV_CNT,
}

#[derive(Debug, Default)]
pub struct Keyboard {
    name: String,
    path: PathBuf,
    default_kbd_repeat: KeyboardRepeat,
    fd: i32,
    key_mods: KeyMods,
}

unsafe fn find_devices() -> (Vec<Keyboard>, Vec<Mouse>) {
    let mut keyboards = vec![];
    let mut mice = vec![];
    for entry in std::fs::read_dir("/dev/input/").unwrap() {
        let entry = entry.unwrap();
        let Some(path) = entry.file_name().to_str().and_then(|file_name| {
            let digit = file_name.strip_prefix("event")?;
            digit.parse::<usize>().ok()?;
            Some(entry.path())
        }) else {
            continue;
        };
        let cpath = CString::new(path.clone().to_string_lossy().into_owned()).unwrap();
        let fd = libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK);
        if fd < 0 {
            eprintln!(
                "Failed to open {}: {}",
                path.display(),
                strerror(errno()).to_string_lossy()
            );
            continue;
        }
        let mut kbd_repeat = KeyboardRepeat::default();
        // Get device name with 256 length buffer.
        const EVIOCGNAME_256: u64 = 0x81004506;
        let mut name_buffer = [0u8; 256];
        let mut name = None;
        if 0 <= libc::ioctl(fd, EVIOCGNAME_256, name_buffer.as_ptr()) {
            name = Some(
                CStr::from_bytes_until_nul(&name_buffer)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if let Some(name) = &name {
            eprintln!("Name: {name}. Path: {}", path.display());
        }
        // let mut abs_info = libc::input_absinfo {
        let mut x_abs_info = InputAbsinfo::default();
        let mut y_abs_info = InputAbsinfo::default();
        if 0 <= libc::ioctl(fd, EVIOCGREP, &mut kbd_repeat) {
            eprintln!("{} {kbd_repeat:?}", path.display());
            // Get exclusive access to the keyboard.
            // https://stackoverflow.com/questions/1698423/how-can-you-take-ownership-of-a-hid-device/1698686#1698686
            // https://unix.stackexchange.com/questions/77756/can-i-stop-linux-from-listening-to-a-usb-input-device-as-a-keyboard-but-still-c
            assert!(0 <= libc::ioctl(fd, EVIOCGRAB, 1));
            keyboards.push(Keyboard {
                name: name.unwrap_or_default(),
                path,
                default_kbd_repeat: kbd_repeat,
                fd,
                key_mods: KeyMods::default(),
            });
        } else if 0 <= libc::ioctl(fd, EVIOCGABS(AbsAxis::ABS_X), &mut x_abs_info)
            && 0 <= libc::ioctl(fd, EVIOCGABS(AbsAxis::ABS_Y), &mut y_abs_info)
        {
            eprintln!("{} X: {x_abs_info:?}. Y: {y_abs_info:?}", path.display());
            assert!(0 <= libc::ioctl(fd, EVIOCGRAB, 1));
            mice.push(Mouse {
                path,
                name: name.unwrap_or_default(),
                fd,
                x_abs_info,
                y_abs_info,
                x: 0.0,
                y: 0.0,
            });
        } else {
            // NB: Closing an event device is slow.
            // libc::close(fd);
        }
    }
    (keyboards, mice)
}

fn strerror(status: i32) -> &'static CStr {
    unsafe { CStr::from_ptr(libc::strerror(status) as *const i8) }
}

#[derive(Debug, Default)]
#[repr(C)]
pub struct drmEventContext {
    /* This struct is versioned so we can add more pointers if we
     * add more events. */
    pub version: i32,

    pub vblank_handler: Option<
        extern "C" fn(fd: i32, sequence: u32, tv_sec: u32, tv_usec: u32, user_data: *mut c_void),
    >,

    pub page_flip_handler: Option<
        extern "C" fn(fd: i32, sequence: u32, tv_sec: u32, tv_usec: u32, user_data: *mut c_void),
    >,
    pub page_flip_handler2: Option<
        extern "C" fn(
            fd: i32,
            sequence: u32,
            tv_sec: u32,
            tv_usec: u32,
            crtc_id: u32,
            user_data: *mut c_void,
        ),
    >,
    pub sequence_handler: Option<extern "C" fn(fd: i32, sequence: u64, ns: u64, user_data: u64)>,
}

/**
 * Flags to indicate the intended use for the buffer - these are passed into
 * gbm_bo_create(). The caller must set the union of all the flags that are
 * appropriate
 *
 * \sa Use gbm_device_is_format_supported() to check if the combination of format
 * and use flags are supported
 */
#[repr(C)]
enum gbm_bo_flags {
    /**
     * Buffer is going to be presented to the screen using an API such as KMS
     */
    GBM_BO_USE_SCANOUT = (1 << 0),
    /**
     * Buffer is going to be used as cursor
     */
    GBM_BO_USE_CURSOR = (1 << 1),
    /**
     * Deprecated
     */
    // GBM_BO_USE_CURSOR_64X64 = GBM_BO_USE_CURSOR,
    // GBM_BO_USE_CURSOR_64X64 = (1 << 1),
    /**
     * Buffer is to be used for rendering - for example it is going to be used
     * as the storage for a color buffer
     */
    GBM_BO_USE_RENDERING = (1 << 2),
    /**
     * Buffer can be used for gbm_bo_write.  This is guaranteed to work
     * with GBM_BO_USE_CURSOR, but may not work for other combinations.
     */
    GBM_BO_USE_WRITE = (1 << 3),
    /**
     * Buffer is linear, i.e. not tiled.
     */
    GBM_BO_USE_LINEAR = (1 << 4),
    /**
     * Buffer is protected, i.e. encrypted and not readable by CPU or any
     * other non-secure / non-trusted components nor by non-trusted OpenGL,
     * OpenCL, and Vulkan applications.
     */
    GBM_BO_USE_PROTECTED = (1 << 5),

    /**
     * The buffer will be used for front buffer rendering.  On some
     * platforms this may (for example) disable framebuffer compression
     * to avoid problems with compression flags data being out of sync
     * with pixel data.
     */
    GBM_BO_USE_FRONT_RENDERING = (1 << 6),
}

impl drmModeEncoder {
    pub fn has_crtc(&self, crtc_index: usize) -> bool {
        ((self.possible_crtcs >> crtc_index) & 1) == 1
    }
}

#[derive(Clone)]
pub struct LibDrmPointer<T> {
    value: *const T,
    free_fn: unsafe extern "C" fn(*const T),
    module: std::rc::Rc<module::Module>,
    gc: std::rc::Rc<()>,
}

impl<T: std::fmt::Debug> std::fmt::Debug for LibDrmPointer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value: &T = self;
        value.fmt(f)
    }
}

impl<T> Drop for LibDrmPointer<T> {
    fn drop(&mut self) {
        if std::rc::Rc::strong_count(&self.gc) == 1 {
            unsafe { (self.free_fn)(self.value) };
        }
    }
}

impl<T> std::ops::Deref for LibDrmPointer<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.value }
    }
}

pub type drmModeResGc = LibDrmPointer<drmModeRes>;
pub type drmModeCrtcGc = LibDrmPointer<drmModeCrtc>;
pub type drmModeEncoderGc = LibDrmPointer<drmModeEncoder>;
pub type drmModeConnectorGc = LibDrmPointer<drmModeConnector>;

impl LibDrm {
    pub fn drmModeGetEncoder(&self, fd: i32, id: u32) -> Option<drmModeEncoderGc> {
        let encoder = unsafe { (self.drmModeGetEncoder)(fd, id) };
        if encoder.is_null() {
            None
        } else {
            Some(LibDrmPointer {
                value: encoder,
                free_fn: self.drmModeFreeEncoder,
                module: self.module.clone(),
                gc: std::rc::Rc::new(()),
            })
        }
    }
    pub fn drmModeGetCrtc(&self, fd: i32, id: u32) -> Option<drmModeCrtcGc> {
        let encoder = unsafe { (self.drmModeGetCrtc)(fd, id) };
        if encoder.is_null() {
            None
        } else {
            Some(LibDrmPointer {
                value: encoder,
                free_fn: self.drmModeFreeCrtc,
                module: self.module.clone(),
                gc: std::rc::Rc::new(()),
            })
        }
    }

    pub fn drmModeGetConnector(&self, fd: i32, id: u32) -> Option<drmModeConnectorGc> {
        let encoder = unsafe { (self.drmModeGetConnector)(fd, id) };
        if encoder.is_null() {
            None
        } else {
            Some(LibDrmPointer {
                value: encoder,
                free_fn: self.drmModeFreeConnector,
                module: self.module.clone(),
                gc: std::rc::Rc::new(()),
            })
        }
    }

    pub fn drmModeGetResources(&self, fd: i32) -> Option<drmModeResGc> {
        let encoder = unsafe { (self.drmModeGetResources)(fd) };
        if encoder.is_null() {
            None
        } else {
            Some(LibDrmPointer {
                value: encoder,
                free_fn: self.drmModeFreeResources,
                module: self.module.clone(),
                gc: std::rc::Rc::new(()),
            })
        }
    }
}

unsafe fn find_encoder_for_connector(
    libdrm: &LibDrm,
    resources: &drmModeResGc,
    fd: i32,
    connector: &drmModeConnectorGc,
) -> Option<drmModeEncoderGc> {
    for &encoder_id in resources.encoder_ids() {
        let Some(encoder) = libdrm.drmModeGetEncoder(fd, encoder_id) else {
            continue;
        };
        if encoder.encoder_id == connector.encoder_id {
            return Some(encoder);
        }
    }
    None
}

unsafe fn find_crtcs_for_connector(
    libdrm: &LibDrm,
    resources: &drmModeResGc,
    fd: i32,
    connector: &drmModeConnectorGc,
) -> Vec<(drmModeEncoderGc, Vec<drmModeCrtcGc>)> {
    let mut res = vec![];
    for &encoder_id in resources.encoder_ids() {
        let Some(encoder) = libdrm.drmModeGetEncoder(fd, encoder_id) else {
            continue;
        };
        let mut crtcs = vec![];
        for (index, &crtc_id) in resources.crtc_ids().iter().enumerate() {
            if encoder.has_crtc(index) {
                let crtc = libdrm.drmModeGetCrtc(fd, crtc_id);
                crtcs.extend(crtc.filter(|x| x.mode_valid != 0));
            }
        }
        res.push((encoder, crtcs));
    }
    res
}

unsafe fn find_connected_connector(
    libdrm: &LibDrm,
    resources: &drmModeResGc,
    fd: i32,
) -> Option<drmModeConnectorGc> {
    for &connector_id in resources.connector_ids() {
        let Some(connector) = libdrm.drmModeGetConnector(fd, connector_id) else {
            continue;
        };
        if connector.connection == drmModeConnection::DRM_MODE_CONNECTED
            && connector.count_modes > 0
        {
            return Some(connector);
        }
        // let connector = unsafe { (libdrm.drmModeGetConnector)(fd, connector_id) };
        // if !connector.is_null() {
        //     let connector = unsafe { &*connector };
        //     if connector.connection == drmModeConnection::DRM_MODE_CONNECTED
        //         && connector.count_modes > 0
        //     {
        //         return Some(connector);
        //         // break;
        //     } else {
        //         unsafe { (libdrm.drmModeFreeConnector)(connector) };
        //     }
        // }
    }
    None
}

fn find_device_resource(libdrm: &LibDrm) -> (drmModeResGc, i32, String) {
    for device_id in 0..32 {
        let path = format!("/dev/dri/card{device_id}");
        let path = CString::new(path).unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            let errno = unsafe { errno() };
            // if device_id == 0 {
            //     assert_ne!(
            //         errno,
            //         libc::ENOENT,
            //         "DRI device not found, have you enabled vc4 driver in /boot/config.txt file?"
            //     );
            // } else {
            // }
            if errno == libc::ENOENT {
                continue;
            }
            assert_ne!(
                errno,
                libc::EACCES,
                "no permission to open DRI device {path:?}, is your user in 'video' group?"
            );
        } else {
            // let resources = unsafe { (libdrm.drmModeGetResources)(fd) };
            // if !resources.is_null() {
            //     return (resources, fd, path.into_string().unwrap());
            // }
            if let Some(resources) = libdrm.drmModeGetResources(fd) {
                return (resources, fd, path.into_string().unwrap());
            }
            unsafe { libc::close(fd) };
        }
    }
    panic!("Failed to find a valid DRI device");
}

#[derive(Default, Debug)]
pub struct KeyboardRepeat {
    pub delay: u32,
    pub period: u32,
}

#[derive(Debug)]
pub struct Mouse {
    pub path: PathBuf,
    pub name: String,
    pub fd: i32,
    pub x_abs_info: InputAbsinfo,
    pub y_abs_info: InputAbsinfo,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Default)]
pub struct InputAbsinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/*
 * Keys and buttons
 *
 * Most of the keys/buttons are modeled after USB HUT 1.12
 * (see http://www.usb.org/developers/hidpage).
 * Abbreviations in the comments:
 * AC - Application Control
 * AL - Application Launch Button
 * SC - System Control
 */

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum KeyCode {
    KEY_RESERVED = 0,
    KEY_ESC = 1,
    KEY_1 = 2,
    KEY_2 = 3,
    KEY_3 = 4,
    KEY_4 = 5,
    KEY_5 = 6,
    KEY_6 = 7,
    KEY_7 = 8,
    KEY_8 = 9,
    KEY_9 = 10,
    KEY_0 = 11,
    KEY_MINUS = 12,
    KEY_EQUAL = 13,
    KEY_BACKSPACE = 14,
    KEY_TAB = 15,
    KEY_Q = 16,
    KEY_W = 17,
    KEY_E = 18,
    KEY_R = 19,
    KEY_T = 20,
    KEY_Y = 21,
    KEY_U = 22,
    KEY_I = 23,
    KEY_O = 24,
    KEY_P = 25,
    KEY_LEFTBRACE = 26,
    KEY_RIGHTBRACE = 27,
    KEY_ENTER = 28,
    KEY_LEFTCTRL = 29,
    KEY_A = 30,
    KEY_S = 31,
    KEY_D = 32,
    KEY_F = 33,
    KEY_G = 34,
    KEY_H = 35,
    KEY_J = 36,
    KEY_K = 37,
    KEY_L = 38,
    KEY_SEMICOLON = 39,
    KEY_APOSTROPHE = 40,
    KEY_GRAVE = 41,
    KEY_LEFTSHIFT = 42,
    KEY_BACKSLASH = 43,
    KEY_Z = 44,
    KEY_X = 45,
    KEY_C = 46,
    KEY_V = 47,
    KEY_B = 48,
    KEY_N = 49,
    KEY_M = 50,
    KEY_COMMA = 51,
    KEY_DOT = 52,
    KEY_SLASH = 53,
    KEY_RIGHTSHIFT = 54,
    KEY_KPASTERISK = 55,
    KEY_LEFTALT = 56,
    KEY_SPACE = 57,
    KEY_CAPSLOCK = 58,
    KEY_F1 = 59,
    KEY_F2 = 60,
    KEY_F3 = 61,
    KEY_F4 = 62,
    KEY_F5 = 63,
    KEY_F6 = 64,
    KEY_F7 = 65,
    KEY_F8 = 66,
    KEY_F9 = 67,
    KEY_F10 = 68,
    KEY_NUMLOCK = 69,
    KEY_SCROLLLOCK = 70,
    KEY_KP7 = 71,
    KEY_KP8 = 72,
    KEY_KP9 = 73,
    KEY_KPMINUS = 74,
    KEY_KP4 = 75,
    KEY_KP5 = 76,
    KEY_KP6 = 77,
    KEY_KPPLUS = 78,
    KEY_KP1 = 79,
    KEY_KP2 = 80,
    KEY_KP3 = 81,
    KEY_KP0 = 82,
    KEY_KPDOT = 83,

    KEY_ZENKAKUHANKAKU = 85,
    KEY_102ND = 86,
    KEY_F11 = 87,
    KEY_F12 = 88,
    KEY_RO = 89,
    KEY_KATAKANA = 90,
    KEY_HIRAGANA = 91,
    KEY_HENKAN = 92,
    KEY_KATAKANAHIRAGANA = 93,
    KEY_MUHENKAN = 94,
    KEY_KPJPCOMMA = 95,
    KEY_KPENTER = 96,
    KEY_RIGHTCTRL = 97,
    KEY_KPSLASH = 98,
    KEY_SYSRQ = 99,
    KEY_RIGHTALT = 100,
    KEY_LINEFEED = 101,
    KEY_HOME = 102,
    KEY_UP = 103,
    KEY_PAGEUP = 104,
    KEY_LEFT = 105,
    KEY_RIGHT = 106,
    KEY_END = 107,
    KEY_DOWN = 108,
    KEY_PAGEDOWN = 109,
    KEY_INSERT = 110,
    KEY_DELETE = 111,
    KEY_MACRO = 112,
    KEY_MUTE = 113,
    KEY_VOLUMEDOWN = 114,
    KEY_VOLUMEUP = 115,
    /* SC System Power Down */
    KEY_POWER = 116,
    KEY_KPEQUAL = 117,
    KEY_KPPLUSMINUS = 118,
    KEY_PAUSE = 119,
    /* AL Compiz Scale (Expose) */
    KEY_SCALE = 120,

    KEY_KPCOMMA = 121,
    KEY_HANGEUL = 122,
    // KEY_HANGUEL = KEY_HANGEUL,
    KEY_HANJA = 123,
    KEY_YEN = 124,
    KEY_LEFTMETA = 125,
    KEY_RIGHTMETA = 126,
    KEY_COMPOSE = 127,

    /* AC Stop */
    KEY_STOP = 128,
    KEY_AGAIN = 129,
    /* AC Properties */
    KEY_PROPS = 130,
    /* AC Undo */
    KEY_UNDO = 131,
    KEY_FRONT = 132,
    /* AC Copy */
    KEY_COPY = 133,
    /* AC Open */
    KEY_OPEN = 134,
    /* AC Paste */
    KEY_PASTE = 135,
    /* AC Search */
    KEY_FIND = 136,
    /* AC Cut */
    KEY_CUT = 137,
    /* AL Integrated Help Center */
    KEY_HELP = 138,
    /* Menu (show menu) */
    KEY_MENU = 139,
    /* AL Calculator */
    KEY_CALC = 140,
    KEY_SETUP = 141,
    /* SC System Sleep */
    KEY_SLEEP = 142,
    /* System Wake Up */
    KEY_WAKEUP = 143,
    /* AL Local Machine Browser */
    KEY_FILE = 144,
    KEY_SENDFILE = 145,
    KEY_DELETEFILE = 146,
    KEY_XFER = 147,
    KEY_PROG1 = 148,
    KEY_PROG2 = 149,
    /* AL Internet Browser */
    KEY_WWW = 150,
    KEY_MSDOS = 151,
    /* AL Terminal Lock/Screensaver */
    // KEY_SCREENLOCK = KEY_COFFEE,
    KEY_SCREENLOCK = 152,
    /* Display orientation for e.g. tablets */
    KEY_ROTATE_DISPLAY = 153,
    // KEY_DIRECTION = KEY_ROTATE_DISPLAY,
    KEY_CYCLEWINDOWS = 154,
    KEY_MAIL = 155,
    /* AC Bookmarks */
    KEY_BOOKMARKS = 156,
    KEY_COMPUTER = 157,
    /* AC Back */
    KEY_BACK = 158,
    /* AC Forward */
    KEY_FORWARD = 159,
    KEY_CLOSECD = 160,
    KEY_EJECTCD = 161,
    KEY_EJECTCLOSECD = 162,
    KEY_NEXTSONG = 163,
    KEY_PLAYPAUSE = 164,
    KEY_PREVIOUSSONG = 165,
    KEY_STOPCD = 166,
    KEY_RECORD = 167,
    KEY_REWIND = 168,
    /* Media Select Telephone */
    KEY_PHONE = 169,
    KEY_ISO = 170,
    /* AL Consumer Control Configuration */
    KEY_CONFIG = 171,
    /* AC Home */
    KEY_HOMEPAGE = 172,
    /* AC Refresh */
    KEY_REFRESH = 173,
    /* AC Exit */
    KEY_EXIT = 174,
    KEY_MOVE = 175,
    KEY_EDIT = 176,
    KEY_SCROLLUP = 177,
    KEY_SCROLLDOWN = 178,
    KEY_KPLEFTPAREN = 179,
    KEY_KPRIGHTPAREN = 180,
    /* AC New */
    KEY_NEW = 181,
    /* AC Redo/Repeat */
    KEY_REDO = 182,

    KEY_F13 = 183,
    KEY_F14 = 184,
    KEY_F15 = 185,
    KEY_F16 = 186,
    KEY_F17 = 187,
    KEY_F18 = 188,
    KEY_F19 = 189,
    KEY_F20 = 190,
    KEY_F21 = 191,
    KEY_F22 = 192,
    KEY_F23 = 193,
    KEY_F24 = 194,

    KEY_PLAYCD = 200,
    KEY_PAUSECD = 201,
    KEY_PROG3 = 202,
    KEY_PROG4 = 203,
    /* AC Desktop Show All Applications */
    KEY_ALL_APPLICATIONS = 204,
    // KEY_DASHBOARD = KEY_ALL_APPLICATIONS,
    KEY_SUSPEND = 205,
    /* AC Close */
    KEY_CLOSE = 206,
    KEY_PLAY = 207,
    KEY_FASTFORWARD = 208,
    KEY_BASSBOOST = 209,
    /* AC Print */
    KEY_PRINT = 210,
    KEY_HP = 211,
    KEY_CAMERA = 212,
    KEY_SOUND = 213,
    KEY_QUESTION = 214,
    KEY_EMAIL = 215,
    KEY_CHAT = 216,
    KEY_SEARCH = 217,
    KEY_CONNECT = 218,
    /* AL Checkbook/Finance */
    KEY_FINANCE = 219,
    KEY_SPORT = 220,
    KEY_SHOP = 221,
    KEY_ALTERASE = 222,
    /* AC Cancel */
    KEY_CANCEL = 223,
    KEY_BRIGHTNESSDOWN = 224,
    KEY_BRIGHTNESSUP = 225,
    KEY_MEDIA = 226,

    /* Cycle between available video outputs (Monitor/LCD/TV-out/etc) */
    KEY_SWITCHVIDEOMODE = 227,
    KEY_KBDILLUMTOGGLE = 228,
    KEY_KBDILLUMDOWN = 229,
    KEY_KBDILLUMUP = 230,

    /* AC Send */
    KEY_SEND = 231,
    /* AC Reply */
    KEY_REPLY = 232,
    /* AC Forward Msg */
    KEY_FORWARDMAIL = 233,
    /* AC Save */
    KEY_SAVE = 234,
    KEY_DOCUMENTS = 235,

    KEY_BATTERY = 236,

    KEY_BLUETOOTH = 237,
    KEY_WLAN = 238,
    KEY_UWB = 239,

    KEY_UNKNOWN = 240,

    /* drive next video source */
    KEY_VIDEO_NEXT = 241,
    /* drive previous video source */
    KEY_VIDEO_PREV = 242,
    /* brightness up, after max is min */
    KEY_BRIGHTNESS_CYCLE = 243,
    /* Set Auto Brightness: manual brightness control is off, rely on ambient */
    KEY_BRIGHTNESS_AUTO = 244,
    // KEY_BRIGHTNESS_ZERO = KEY_BRIGHTNESS_AUTO,
    /* display device to off state */
    KEY_DISPLAY_OFF = 245,

    /* Wireless WAN (LTE, UMTS, GSM, etc.) */
    KEY_WWAN = 246,
    // KEY_WIMAX = KEY_WWAN,
    /* Key that controls all radios */
    KEY_RFKILL = 247,

    /* Mute / unmute the microphone */
    KEY_MICMUTE = 248,
    /* Code 255 is reserved for special needs of AT keyboard driver */
}

impl KeyCode {
    pub fn to_macroquad_keycode(&self) -> Option<crate::event::KeyCode> {
        Some(match self {
            Self::KEY_SPACE => crate::event::KeyCode::Space,
            Self::KEY_APOSTROPHE => crate::event::KeyCode::Apostrophe,
            Self::KEY_COMMA => crate::event::KeyCode::Comma,
            Self::KEY_MINUS => crate::event::KeyCode::Minus,
            Self::KEY_DOT => crate::event::KeyCode::Period,
            Self::KEY_SLASH => crate::event::KeyCode::Slash,
            Self::KEY_0 => crate::event::KeyCode::Key0,
            Self::KEY_1 => crate::event::KeyCode::Key1,
            Self::KEY_2 => crate::event::KeyCode::Key2,
            Self::KEY_3 => crate::event::KeyCode::Key3,
            Self::KEY_4 => crate::event::KeyCode::Key4,
            Self::KEY_5 => crate::event::KeyCode::Key5,
            Self::KEY_6 => crate::event::KeyCode::Key6,
            Self::KEY_7 => crate::event::KeyCode::Key7,
            Self::KEY_8 => crate::event::KeyCode::Key8,
            Self::KEY_9 => crate::event::KeyCode::Key9,
            Self::KEY_SEMICOLON => crate::event::KeyCode::Semicolon,
            Self::KEY_EQUAL => crate::event::KeyCode::Equal,
            Self::KEY_A => crate::event::KeyCode::A,
            Self::KEY_B => crate::event::KeyCode::B,
            Self::KEY_C => crate::event::KeyCode::C,
            Self::KEY_D => crate::event::KeyCode::D,
            Self::KEY_E => crate::event::KeyCode::E,
            Self::KEY_F => crate::event::KeyCode::F,
            Self::KEY_G => crate::event::KeyCode::G,
            Self::KEY_H => crate::event::KeyCode::H,
            Self::KEY_I => crate::event::KeyCode::I,
            Self::KEY_J => crate::event::KeyCode::J,
            Self::KEY_K => crate::event::KeyCode::K,
            Self::KEY_L => crate::event::KeyCode::L,
            Self::KEY_M => crate::event::KeyCode::M,
            Self::KEY_N => crate::event::KeyCode::N,
            Self::KEY_O => crate::event::KeyCode::O,
            Self::KEY_P => crate::event::KeyCode::P,
            Self::KEY_Q => crate::event::KeyCode::Q,
            Self::KEY_R => crate::event::KeyCode::R,
            Self::KEY_S => crate::event::KeyCode::S,
            Self::KEY_T => crate::event::KeyCode::T,
            Self::KEY_U => crate::event::KeyCode::U,
            Self::KEY_V => crate::event::KeyCode::V,
            Self::KEY_W => crate::event::KeyCode::W,
            Self::KEY_X => crate::event::KeyCode::X,
            Self::KEY_Y => crate::event::KeyCode::Y,
            Self::KEY_Z => crate::event::KeyCode::Z,
            Self::KEY_LEFTBRACE => crate::event::KeyCode::LeftBracket,
            Self::KEY_BACKSLASH => crate::event::KeyCode::Backslash,
            Self::KEY_RIGHTBRACE => crate::event::KeyCode::RightBracket,
            Self::KEY_GRAVE => crate::event::KeyCode::GraveAccent,
            // Self::KEY_WORLD1 => crate::event::KeyCode::World1,
            // Self::KEY_WORLD2 => crate::event::KeyCode::World2,
            Self::KEY_ESC => crate::event::KeyCode::Escape,
            Self::KEY_ENTER => crate::event::KeyCode::Enter,
            Self::KEY_TAB => crate::event::KeyCode::Tab,
            Self::KEY_BACKSPACE => crate::event::KeyCode::Backspace,
            Self::KEY_INSERT => crate::event::KeyCode::Insert,
            Self::KEY_DELETE => crate::event::KeyCode::Delete,
            Self::KEY_RIGHT => crate::event::KeyCode::Right,
            Self::KEY_LEFT => crate::event::KeyCode::Left,
            Self::KEY_DOWN => crate::event::KeyCode::Down,
            Self::KEY_UP => crate::event::KeyCode::Up,
            Self::KEY_PAGEUP => crate::event::KeyCode::PageUp,
            Self::KEY_PAGEDOWN => crate::event::KeyCode::PageDown,
            Self::KEY_HOME => crate::event::KeyCode::Home,
            Self::KEY_END => crate::event::KeyCode::End,
            Self::KEY_CAPSLOCK => crate::event::KeyCode::CapsLock,
            Self::KEY_SCROLLLOCK => crate::event::KeyCode::ScrollLock,
            Self::KEY_NUMLOCK => crate::event::KeyCode::NumLock,
            // TODO?
            // Self::KEY_PRINT => crate::event::KeyCode::PrintScreen,
            Self::KEY_PAUSE => crate::event::KeyCode::Pause,
            Self::KEY_F1 => crate::event::KeyCode::F1,
            Self::KEY_F2 => crate::event::KeyCode::F2,
            Self::KEY_F3 => crate::event::KeyCode::F3,
            Self::KEY_F4 => crate::event::KeyCode::F4,
            Self::KEY_F5 => crate::event::KeyCode::F5,
            Self::KEY_F6 => crate::event::KeyCode::F6,
            Self::KEY_F7 => crate::event::KeyCode::F7,
            Self::KEY_F8 => crate::event::KeyCode::F8,
            Self::KEY_F9 => crate::event::KeyCode::F9,
            Self::KEY_F10 => crate::event::KeyCode::F10,
            Self::KEY_F11 => crate::event::KeyCode::F11,
            Self::KEY_F12 => crate::event::KeyCode::F12,
            Self::KEY_F13 => crate::event::KeyCode::F13,
            Self::KEY_F14 => crate::event::KeyCode::F14,
            Self::KEY_F15 => crate::event::KeyCode::F15,
            Self::KEY_F16 => crate::event::KeyCode::F16,
            Self::KEY_F17 => crate::event::KeyCode::F17,
            Self::KEY_F18 => crate::event::KeyCode::F18,
            Self::KEY_F19 => crate::event::KeyCode::F19,
            Self::KEY_F20 => crate::event::KeyCode::F20,
            Self::KEY_F21 => crate::event::KeyCode::F21,
            Self::KEY_F22 => crate::event::KeyCode::F22,
            Self::KEY_F23 => crate::event::KeyCode::F23,
            Self::KEY_F24 => crate::event::KeyCode::F24,
            // Self::KEY_F25 => crate::event::KeyCode::F25,
            Self::KEY_KP0 => crate::event::KeyCode::Kp0,
            Self::KEY_KP1 => crate::event::KeyCode::Kp1,
            Self::KEY_KP2 => crate::event::KeyCode::Kp2,
            Self::KEY_KP3 => crate::event::KeyCode::Kp3,
            Self::KEY_KP4 => crate::event::KeyCode::Kp4,
            Self::KEY_KP5 => crate::event::KeyCode::Kp5,
            Self::KEY_KP6 => crate::event::KeyCode::Kp6,
            Self::KEY_KP7 => crate::event::KeyCode::Kp7,
            Self::KEY_KP8 => crate::event::KeyCode::Kp8,
            Self::KEY_KP9 => crate::event::KeyCode::Kp9,
            Self::KEY_KPDOT => crate::event::KeyCode::KpDecimal,
            Self::KEY_KPSLASH => crate::event::KeyCode::KpDivide,
            Self::KEY_KPASTERISK => crate::event::KeyCode::KpMultiply,
            Self::KEY_KPMINUS => crate::event::KeyCode::KpSubtract,
            Self::KEY_KPPLUS => crate::event::KeyCode::KpAdd,
            Self::KEY_KPENTER => crate::event::KeyCode::KpEnter,
            Self::KEY_KPEQUAL => crate::event::KeyCode::KpEqual,
            Self::KEY_LEFTSHIFT => crate::event::KeyCode::LeftShift,
            Self::KEY_LEFTCTRL => crate::event::KeyCode::LeftControl,
            Self::KEY_LEFTALT => crate::event::KeyCode::LeftAlt,
            Self::KEY_LEFTMETA => crate::event::KeyCode::LeftSuper,
            Self::KEY_RIGHTSHIFT => crate::event::KeyCode::RightShift,
            Self::KEY_RIGHTCTRL => crate::event::KeyCode::RightControl,
            Self::KEY_RIGHTALT => crate::event::KeyCode::RightAlt,
            Self::KEY_RIGHTMETA => crate::event::KeyCode::RightSuper,
            Self::KEY_MENU => crate::event::KeyCode::Menu,
            Self::KEY_UNKNOWN => crate::event::KeyCode::Unknown,
            _ => return None,
        })
    }
    pub fn from_code(code: u16) -> Option<Self> {
        use KeyCode::*;
        Some(match code {
            0 => KEY_RESERVED,
            1 => KEY_ESC,
            2 => KEY_1,
            3 => KEY_2,
            4 => KEY_3,
            5 => KEY_4,
            6 => KEY_5,
            7 => KEY_6,
            8 => KEY_7,
            9 => KEY_8,
            10 => KEY_9,
            11 => KEY_0,
            12 => KEY_MINUS,
            13 => KEY_EQUAL,
            14 => KEY_BACKSPACE,
            15 => KEY_TAB,
            16 => KEY_Q,
            17 => KEY_W,
            18 => KEY_E,
            19 => KEY_R,
            20 => KEY_T,
            21 => KEY_Y,
            22 => KEY_U,
            23 => KEY_I,
            24 => KEY_O,
            25 => KEY_P,
            26 => KEY_LEFTBRACE,
            27 => KEY_RIGHTBRACE,
            28 => KEY_ENTER,
            29 => KEY_LEFTCTRL,
            30 => KEY_A,
            31 => KEY_S,
            32 => KEY_D,
            33 => KEY_F,
            34 => KEY_G,
            35 => KEY_H,
            36 => KEY_J,
            37 => KEY_K,
            38 => KEY_L,
            39 => KEY_SEMICOLON,
            40 => KEY_APOSTROPHE,
            41 => KEY_GRAVE,
            42 => KEY_LEFTSHIFT,
            43 => KEY_BACKSLASH,
            44 => KEY_Z,
            45 => KEY_X,
            46 => KEY_C,
            47 => KEY_V,
            48 => KEY_B,
            49 => KEY_N,
            50 => KEY_M,
            51 => KEY_COMMA,
            52 => KEY_DOT,
            53 => KEY_SLASH,
            54 => KEY_RIGHTSHIFT,
            55 => KEY_KPASTERISK,
            56 => KEY_LEFTALT,
            57 => KEY_SPACE,
            58 => KEY_CAPSLOCK,
            59 => KEY_F1,
            60 => KEY_F2,
            61 => KEY_F3,
            62 => KEY_F4,
            63 => KEY_F5,
            64 => KEY_F6,
            65 => KEY_F7,
            66 => KEY_F8,
            67 => KEY_F9,
            68 => KEY_F10,
            69 => KEY_NUMLOCK,
            70 => KEY_SCROLLLOCK,
            71 => KEY_KP7,
            72 => KEY_KP8,
            73 => KEY_KP9,
            74 => KEY_KPMINUS,
            75 => KEY_KP4,
            76 => KEY_KP5,
            77 => KEY_KP6,
            78 => KEY_KPPLUS,
            79 => KEY_KP1,
            80 => KEY_KP2,
            81 => KEY_KP3,
            82 => KEY_KP0,
            83 => KEY_KPDOT,

            85 => KEY_ZENKAKUHANKAKU,
            86 => KEY_102ND,
            87 => KEY_F11,
            88 => KEY_F12,
            89 => KEY_RO,
            90 => KEY_KATAKANA,
            91 => KEY_HIRAGANA,
            92 => KEY_HENKAN,
            93 => KEY_KATAKANAHIRAGANA,
            94 => KEY_MUHENKAN,
            95 => KEY_KPJPCOMMA,
            96 => KEY_KPENTER,
            97 => KEY_RIGHTCTRL,
            98 => KEY_KPSLASH,
            99 => KEY_SYSRQ,
            100 => KEY_RIGHTALT,
            101 => KEY_LINEFEED,
            102 => KEY_HOME,
            103 => KEY_UP,
            104 => KEY_PAGEUP,
            105 => KEY_LEFT,
            106 => KEY_RIGHT,
            107 => KEY_END,
            108 => KEY_DOWN,
            109 => KEY_PAGEDOWN,
            110 => KEY_INSERT,
            111 => KEY_DELETE,
            112 => KEY_MACRO,
            113 => KEY_MUTE,
            114 => KEY_VOLUMEDOWN,
            115 => KEY_VOLUMEUP,
            /* SC System Power Down */
            116 => KEY_POWER,
            117 => KEY_KPEQUAL,
            118 => KEY_KPPLUSMINUS,
            119 => KEY_PAUSE,
            /* AL Compiz Scale (Expose) */
            120 => KEY_SCALE,

            121 => KEY_KPCOMMA,
            122 => KEY_HANGEUL,
            // KEY_HANGEUL => // KEY_HANGUEL,
            123 => KEY_HANJA,
            124 => KEY_YEN,
            125 => KEY_LEFTMETA,
            126 => KEY_RIGHTMETA,
            127 => KEY_COMPOSE,

            /* AC Stop */
            128 => KEY_STOP,
            129 => KEY_AGAIN,
            /* AC Properties */
            130 => KEY_PROPS,
            /* AC Undo */
            131 => KEY_UNDO,
            132 => KEY_FRONT,
            /* AC Copy */
            133 => KEY_COPY,
            /* AC Open */
            134 => KEY_OPEN,
            /* AC Paste */
            135 => KEY_PASTE,
            /* AC Search */
            136 => KEY_FIND,
            /* AC Cut */
            137 => KEY_CUT,
            /* AL Integrated Help Center */
            138 => KEY_HELP,
            /* Menu (show menu) */
            139 => KEY_MENU,
            /* AL Calculator */
            140 => KEY_CALC,
            141 => KEY_SETUP,
            /* SC System Sleep */
            142 => KEY_SLEEP,
            /* System Wake Up */
            143 => KEY_WAKEUP,
            /* AL Local Machine Browser */
            144 => KEY_FILE,
            145 => KEY_SENDFILE,
            146 => KEY_DELETEFILE,
            147 => KEY_XFER,
            148 => KEY_PROG1,
            149 => KEY_PROG2,
            /* AL Internet Browser */
            150 => KEY_WWW,
            151 => KEY_MSDOS,
            /* AL Terminal Lock/Screensaver */
            // KEY_COFFEE => // KEY_SCREENLOCK,
            152 => KEY_SCREENLOCK,
            /* Display orientation for e.g. tablets */
            153 => KEY_ROTATE_DISPLAY,
            // KEY_ROTATE_DISPLAY => // KEY_DIRECTION,
            154 => KEY_CYCLEWINDOWS,
            155 => KEY_MAIL,
            /* AC Bookmarks */
            156 => KEY_BOOKMARKS,
            157 => KEY_COMPUTER,
            /* AC Back */
            158 => KEY_BACK,
            /* AC Forward */
            159 => KEY_FORWARD,
            160 => KEY_CLOSECD,
            161 => KEY_EJECTCD,
            162 => KEY_EJECTCLOSECD,
            163 => KEY_NEXTSONG,
            164 => KEY_PLAYPAUSE,
            165 => KEY_PREVIOUSSONG,
            166 => KEY_STOPCD,
            167 => KEY_RECORD,
            168 => KEY_REWIND,
            /* Media Select Telephone */
            169 => KEY_PHONE,
            170 => KEY_ISO,
            /* AL Consumer Control Configuration */
            171 => KEY_CONFIG,
            /* AC Home */
            172 => KEY_HOMEPAGE,
            /* AC Refresh */
            173 => KEY_REFRESH,
            /* AC Exit */
            174 => KEY_EXIT,
            175 => KEY_MOVE,
            176 => KEY_EDIT,
            177 => KEY_SCROLLUP,
            178 => KEY_SCROLLDOWN,
            179 => KEY_KPLEFTPAREN,
            180 => KEY_KPRIGHTPAREN,
            /* AC New */
            181 => KEY_NEW,
            /* AC Redo/Repeat */
            182 => KEY_REDO,

            183 => KEY_F13,
            184 => KEY_F14,
            185 => KEY_F15,
            186 => KEY_F16,
            187 => KEY_F17,
            188 => KEY_F18,
            189 => KEY_F19,
            190 => KEY_F20,
            191 => KEY_F21,
            192 => KEY_F22,
            193 => KEY_F23,
            194 => KEY_F24,

            200 => KEY_PLAYCD,
            201 => KEY_PAUSECD,
            202 => KEY_PROG3,
            203 => KEY_PROG4,
            /* AC Desktop Show All Applications */
            204 => KEY_ALL_APPLICATIONS,
            // KEY_ALL_APPLICATIONS => // KEY_DASHBOARD,
            205 => KEY_SUSPEND,
            /* AC Close */
            206 => KEY_CLOSE,
            207 => KEY_PLAY,
            208 => KEY_FASTFORWARD,
            209 => KEY_BASSBOOST,
            /* AC Print */
            210 => KEY_PRINT,
            211 => KEY_HP,
            212 => KEY_CAMERA,
            213 => KEY_SOUND,
            214 => KEY_QUESTION,
            215 => KEY_EMAIL,
            216 => KEY_CHAT,
            217 => KEY_SEARCH,
            218 => KEY_CONNECT,
            /* AL Checkbook/Finance */
            219 => KEY_FINANCE,
            220 => KEY_SPORT,
            221 => KEY_SHOP,
            222 => KEY_ALTERASE,
            /* AC Cancel */
            223 => KEY_CANCEL,
            224 => KEY_BRIGHTNESSDOWN,
            225 => KEY_BRIGHTNESSUP,
            226 => KEY_MEDIA,

            /* Cycle between available video outputs (Monitor/LCD/TV-out/etc) */
            227 => KEY_SWITCHVIDEOMODE,
            228 => KEY_KBDILLUMTOGGLE,
            229 => KEY_KBDILLUMDOWN,
            230 => KEY_KBDILLUMUP,

            /* AC Send */
            231 => KEY_SEND,
            /* AC Reply */
            232 => KEY_REPLY,
            /* AC Forward Msg */
            233 => KEY_FORWARDMAIL,
            /* AC Save */
            234 => KEY_SAVE,
            235 => KEY_DOCUMENTS,

            236 => KEY_BATTERY,

            237 => KEY_BLUETOOTH,
            238 => KEY_WLAN,
            239 => KEY_UWB,

            240 => KEY_UNKNOWN,

            /* drive next video source */
            241 => KEY_VIDEO_NEXT,
            /* drive previous video source */
            242 => KEY_VIDEO_PREV,
            /* brightness up, after max is min */
            243 => KEY_BRIGHTNESS_CYCLE,
            /* Set Auto Brightness: manual brightness control is off, rely on ambient */
            244 => KEY_BRIGHTNESS_AUTO,
            // KEY_BRIGHTNESS_AUTO => // KEY_BRIGHTNESS_ZERO,
            /* display device to off state */
            245 => KEY_DISPLAY_OFF,

            /* Wireless WAN (LTE, UMTS, GSM, etc.) */
            246 => KEY_WWAN,
            // KEY_WWAN => // KEY_WIMAX,
            /* Key that controls all radios */
            247 => KEY_RFKILL,

            /* Mute / unmute the microphone */
            248 => KEY_MICMUTE,
            /* Code 255 is reserved for special needs of AT keyboard driver */
            _ => return None,
        })
    }
}

const KEY_COFFEE: KeyCode = KeyCode::KEY_SCREENLOCK;
const KEY_DIRECTION: KeyCode = KeyCode::KEY_ROTATE_DISPLAY;
const KEY_DASHBOARD: KeyCode = KeyCode::KEY_ALL_APPLICATIONS;
const KEY_BRIGHTNESS_ZERO: KeyCode = KeyCode::KEY_BRIGHTNESS_AUTO;
const KEY_WIMAX: KeyCode = KeyCode::KEY_WWAN;

pub enum MouseButton {
    // BTN_MISC = 0x100,
    BTN_0 = 0x100,
    BTN_1 = 0x101,
    BTN_2 = 0x102,
    BTN_3 = 0x103,
    BTN_4 = 0x104,
    BTN_5 = 0x105,
    BTN_6 = 0x106,
    BTN_7 = 0x107,
    BTN_8 = 0x108,
    BTN_9 = 0x109,

    // BTN_MOUSE = 0x110,
    BTN_LEFT = 0x110,
    BTN_RIGHT = 0x111,
    BTN_MIDDLE = 0x112,
    BTN_SIDE = 0x113,
    BTN_EXTRA = 0x114,
    BTN_FORWARD = 0x115,
    BTN_BACK = 0x116,
    BTN_TASK = 0x117,

    // BTN_JOYSTICK = 0x120,
    BTN_TRIGGER = 0x120,
    BTN_THUMB = 0x121,
    BTN_THUMB2 = 0x122,
    BTN_TOP = 0x123,
    BTN_TOP2 = 0x124,
    BTN_PINKIE = 0x125,
    BTN_BASE = 0x126,
    BTN_BASE2 = 0x127,
    BTN_BASE3 = 0x128,
    BTN_BASE4 = 0x129,
    BTN_BASE5 = 0x12a,
    BTN_BASE6 = 0x12b,
    BTN_DEAD = 0x12f,

    // BTN_GAMEPAD = 0x130,
    BTN_SOUTH = 0x130,
    // BTN_A = BTN_SOUTH,
    BTN_EAST = 0x131,
    // BTN_B = BTN_EAST,
    BTN_C = 0x132,
    BTN_NORTH = 0x133,
    // BTN_X = BTN_NORTH,
    BTN_WEST = 0x134,
    // BTN_Y = BTN_WEST,
    BTN_Z = 0x135,
    BTN_TL = 0x136,
    BTN_TR = 0x137,
    BTN_TL2 = 0x138,
    BTN_TR2 = 0x139,
    BTN_SELECT = 0x13a,
    BTN_START = 0x13b,
    BTN_MODE = 0x13c,
    BTN_THUMBL = 0x13d,
    BTN_THUMBR = 0x13e,

    // BTN_DIGI = 0x140,
    BTN_TOOL_PEN = 0x140,
    BTN_TOOL_RUBBER = 0x141,
    BTN_TOOL_BRUSH = 0x142,
    BTN_TOOL_PENCIL = 0x143,
    BTN_TOOL_AIRBRUSH = 0x144,
    BTN_TOOL_FINGER = 0x145,
    BTN_TOOL_MOUSE = 0x146,
    BTN_TOOL_LENS = 0x147,
    /* Five fingers on trackpad */
    BTN_TOOL_QUINTTAP = 0x148,
    BTN_STYLUS3 = 0x149,
    BTN_TOUCH = 0x14a,
    BTN_STYLUS = 0x14b,
    BTN_STYLUS2 = 0x14c,
    BTN_TOOL_DOUBLETAP = 0x14d,
    BTN_TOOL_TRIPLETAP = 0x14e,
    /* Four fingers on trackpad */
    BTN_TOOL_QUADTAP = 0x14f,

    // BTN_WHEEL = 0x150,
    BTN_GEAR_DOWN = 0x150,
    BTN_GEAR_UP = 0x151,
}

pub fn chars_from_keycode(keycode: KeyCode) -> Option<(char, char)> {
    Some(match keycode {
        KeyCode::KEY_1 => ('1', '!'),
        KeyCode::KEY_2 => ('2', '@'),
        KeyCode::KEY_3 => ('3', '#'),
        KeyCode::KEY_4 => ('4', '$'),
        KeyCode::KEY_5 => ('5', '%'),
        KeyCode::KEY_6 => ('6', '^'),
        KeyCode::KEY_7 => ('7', '&'),
        KeyCode::KEY_8 => ('8', '*'),
        KeyCode::KEY_9 => ('9', '('),
        KeyCode::KEY_0 => ('0', ')'),
        KeyCode::KEY_A => ('a', 'A'),
        KeyCode::KEY_APOSTROPHE => ('\'', '"'),
        KeyCode::KEY_B => ('b', 'B'),
        KeyCode::KEY_BACKSLASH => ('\\', '|'),
        KeyCode::KEY_C => ('c', 'C'),
        KeyCode::KEY_COMMA => (',', '<'),
        KeyCode::KEY_D => ('d', 'D'),
        KeyCode::KEY_DOT => ('.', '>'),
        KeyCode::KEY_E => ('e', 'E'),
        KeyCode::KEY_EQUAL => ('=', '+'),
        KeyCode::KEY_F => ('f', 'F'),
        KeyCode::KEY_G => ('g', 'G'),
        KeyCode::KEY_GRAVE => ('`', '~'),
        KeyCode::KEY_H => ('h', 'H'),
        KeyCode::KEY_I => ('i', 'I'),
        KeyCode::KEY_J => ('j', 'J'),
        KeyCode::KEY_K => ('k', 'K'),
        KeyCode::KEY_KP1 => ('1', '!'),
        KeyCode::KEY_KP2 => ('2', '@'),
        KeyCode::KEY_KP3 => ('3', '#'),
        KeyCode::KEY_KP4 => ('4', '$'),
        KeyCode::KEY_KP5 => ('5', '%'),
        KeyCode::KEY_KP6 => ('6', '^'),
        KeyCode::KEY_KP7 => ('7', '&'),
        KeyCode::KEY_KP8 => ('8', '*'),
        KeyCode::KEY_KP9 => ('9', '('),
        KeyCode::KEY_KP0 => ('0', ')'),
        KeyCode::KEY_KPASTERISK => ('*', '*'), // TODO
        KeyCode::KEY_KPDOT => ('.', '>'),
        KeyCode::KEY_KPMINUS => ('-', '_'),
        KeyCode::KEY_KPPLUS => ('=', '+'), // TODO
        KeyCode::KEY_L => ('l', 'L'),
        KeyCode::KEY_LEFTBRACE => ('[', '{'),
        KeyCode::KEY_M => ('m', 'M'),
        KeyCode::KEY_MINUS => ('-', '_'),
        KeyCode::KEY_N => ('n', 'N'),
        KeyCode::KEY_O => ('o', 'O'),
        KeyCode::KEY_P => ('p', 'P'),
        KeyCode::KEY_Q => ('q', 'Q'),
        KeyCode::KEY_R => ('r', 'R'),
        KeyCode::KEY_RIGHTBRACE => (']', '}'),
        KeyCode::KEY_S => ('s', 'S'),
        KeyCode::KEY_SEMICOLON => (';', ':'),
        KeyCode::KEY_SLASH => ('/', '?'),
        KeyCode::KEY_SPACE => (' ', ' '),
        KeyCode::KEY_T => ('t', 'T'),
        KeyCode::KEY_TAB => ('\t', '\t'),
        KeyCode::KEY_U => ('u', 'U'),
        KeyCode::KEY_V => ('v', 'V'),
        KeyCode::KEY_W => ('w', 'W'),
        KeyCode::KEY_X => ('x', 'X'),
        KeyCode::KEY_Y => ('y', 'Y'),
        KeyCode::KEY_Z => ('z', 'Z'),
        _ => return None,
    })
}
