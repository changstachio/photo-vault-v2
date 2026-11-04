#![allow(non_camel_case_types)]
use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int, c_uint, c_void};

#[repr(C)]
struct libraw_data_t {
    _p: [u8; 0],
}

#[repr(C)]
struct libraw_processed_image_t {
    kind: c_int,
    height: u16,
    width: u16,
    colors: u16,
    bits: u16,
    data_size: c_uint,
    data: [u8; 1],
}

extern "C" {
    fn libraw_init(flags: c_uint) -> *mut libraw_data_t;
    fn libraw_close(p: *mut libraw_data_t);
    fn libraw_strerror(code: c_int) -> *const c_char;
    fn libraw_open_buffer(p: *mut libraw_data_t, buf: *const c_void, size: usize) -> c_int;
    fn libraw_unpack(p: *mut libraw_data_t) -> c_int;
    fn libraw_dcraw_process(p: *mut libraw_data_t) -> c_int;
    fn libraw_dcraw_make_mem_image(p: *mut libraw_data_t, err: *mut c_int) -> *mut libraw_processed_image_t;
    fn libraw_dcraw_clear_mem(p: *mut libraw_processed_image_t);
    fn libraw_set_demosaic(p: *mut libraw_data_t, v: c_int);
    fn libraw_set_output_color(p: *mut libraw_data_t, v: c_int);
    fn libraw_set_output_bps(p: *mut libraw_data_t, v: c_int);
    fn libraw_set_gamma(p: *mut libraw_data_t, i: c_int, v: f32);
    fn libraw_set_no_auto_bright(p: *mut libraw_data_t, v: c_int);
    fn pv_set_camera_wb(p: *mut libraw_data_t, v: c_int);
    fn pv_exif_colorspace(p: *mut libraw_data_t) -> c_int;
    fn pv_set_render(p: *mut libraw_data_t, use_auto_wb: c_int, exp_shift: f32, exp_preser: f32, bright: f32, no_auto_bright: c_int, auto_bright_thr: f32, highlight: c_int, threshold: f32, fbdd_noiserd: c_int, med_passes: c_int);
}

/// Processing options beyond colour space, gamma and demosaic.
#[derive(Clone, Copy, Debug)]
pub struct RenderParams {
    pub auto_wb: bool,
    /// Linear exposure multiplier (0.25..=8; 1 = off).
    pub exp_shift: f32,
    pub exp_preser: f32,
    pub bright: f32,
    pub auto_bright: bool,
    /// Fraction of pixels auto-brightness may clip (e.g. 0.01).
    pub auto_bright_thr: f32,
    pub highlight: i32,
    pub threshold: f32,
    pub fbdd_noiserd: i32,
    pub med_passes: i32,
}

fn check(code: c_int) -> Result<(), String> {
    if code == 0 {
        return Ok(());
    }
    let s = unsafe { CStr::from_ptr(libraw_strerror(code)) };
    Err(format!("libraw error {code}: {}", s.to_string_lossy()))
}

pub struct Raw<'a> {
    p: *mut libraw_data_t,
    _buf: PhantomData<&'a [u8]>,
}

unsafe impl Send for Raw<'_> {}

impl<'a> Raw<'a> {
    pub fn open(buf: &'a [u8]) -> Result<Self, String> {
        let p = unsafe { libraw_init(0) };
        if p.is_null() {
            return Err("libraw_init failed".into());
        }
        let r = Raw { p, _buf: PhantomData };
        check(unsafe { libraw_open_buffer(p, buf.as_ptr().cast(), buf.len()) })?;
        Ok(r)
    }
    pub fn exif_colorspace(&self) -> i32 {
        unsafe { pv_exif_colorspace(self.p) }
    }
    pub fn set_demosaic(&mut self, v: i32) {
        unsafe { libraw_set_demosaic(self.p, v) }
    }
    pub fn set_output_color(&mut self, v: i32) {
        unsafe { libraw_set_output_color(self.p, v) }
    }
    pub fn set_gamma(&mut self, power: f32, toe: f32) {
        unsafe {
            libraw_set_gamma(self.p, 0, power);
            libraw_set_gamma(self.p, 1, toe);
        }
    }
    pub fn set_camera_wb(&mut self, on: bool) {
        unsafe { pv_set_camera_wb(self.p, on as c_int) }
    }
    pub fn set_no_auto_bright(&mut self, on: bool) {
        unsafe { libraw_set_no_auto_bright(self.p, on as c_int) }
    }
    pub fn set_render(&mut self, r: &RenderParams) {
        unsafe {
            pv_set_render(self.p, r.auto_wb as c_int, r.exp_shift, r.exp_preser, r.bright, !r.auto_bright as c_int, r.auto_bright_thr, r.highlight, r.threshold, r.fbdd_noiserd, r.med_passes)
        }
    }
    pub fn unpack(&mut self) -> Result<(), String> {
        check(unsafe { libraw_unpack(self.p) })
    }
    pub fn process_rgb8(&mut self) -> Result<Image, String> {
        unsafe {
            libraw_set_output_bps(self.p, 8);
            check(libraw_dcraw_process(self.p))?;
            let mut e = 0;
            let m = libraw_dcraw_make_mem_image(self.p, &mut e);
            check(e)?;
            if m.is_null() {
                return Err("no image".into());
            }
            let img = Image { m };
            if (*m).kind != 2 || (*m).colors != 3 || (*m).bits != 8 || (*m).data_size as usize != (*m).width as usize * (*m).height as usize * 3 {
                return Err("unexpected image layout".into());
            }
            Ok(img)
        }
    }
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        unsafe { libraw_close(self.p) }
    }
}

pub struct Image {
    m: *mut libraw_processed_image_t,
}

impl Image {
    pub fn width(&self) -> u32 {
        unsafe { (*self.m).width as u32 }
    }
    pub fn height(&self) -> u32 {
        unsafe { (*self.m).height as u32 }
    }
    pub fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts((*self.m).data.as_ptr(), (*self.m).data_size as usize) }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { libraw_dcraw_clear_mem(self.m) }
    }
}
