//! Headless iOS Simulator access through CoreSimulator and SimulatorKit.
//!
//! The simulator exposes its framebuffer as an IOSurface on a private IO port.
//! Touch and keyboard events go through SimulatorKit's legacy HID client and
//! the Indigo message constructors. Both paths work while the macOS session is
//! locked, so a terminal on another machine can drive the device.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::Once;

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::NSString;

#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *mut c_void) -> u32;
    fn IOSurfaceGetSeed(surface: *mut c_void) -> u32;
}

extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
}

static LOAD: Once = Once::new();

fn load_once() {
    LOAD.call_once(|| {
        let dev = developer_dir();
        let xcode = dev.trim_end_matches("/Contents/Developer").to_string();
        let paths = [
            "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator"
                .to_string(),
            format!("{dev}/Library/PrivateFrameworks/CoreSimulator.framework/CoreSimulator"),
            format!("{xcode}/Contents/SharedFrameworks/SimulatorKit.framework/SimulatorKit"),
            format!("{dev}/Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit"),
        ];
        for path in &paths {
            if let Ok(c) = CString::new(path.as_str()) {
                unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            }
        }
    });
}

pub fn developer_dir() -> String {
    std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "/Applications/Xcode.app/Contents/Developer".to_string())
}

unsafe fn responds(obj: *mut AnyObject, sel: Sel) -> bool {
    if obj.is_null() {
        return false;
    }
    let b: Bool = msg_send![&*obj, respondsToSelector: sel];
    b.as_bool()
}

unsafe fn nsstring(ptr: *mut AnyObject) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s: &NSString = &*(ptr as *const NSString);
    Some(s.to_string())
}

unsafe fn perform(obj: *mut AnyObject, sel: Sel) -> *mut AnyObject {
    if obj.is_null() {
        return ptr::null_mut();
    }
    msg_send![&*obj, performSelector: sel]
}

unsafe fn kvc(obj: *mut AnyObject, key: &str) -> *mut AnyObject {
    if obj.is_null() {
        return ptr::null_mut();
    }
    let k = NSString::from_str(key);
    msg_send![&*obj, valueForKey: &*k]
}

fn find_device(udid: &str) -> Result<*mut AnyObject, String> {
    load_once();
    unsafe {
        let cls = AnyClass::get(c"SimServiceContext")
            .ok_or_else(|| "SimServiceContext is unavailable; is Xcode installed?".to_string())?;
        let dir = NSString::from_str(&developer_dir());
        let mut err: *mut AnyObject = ptr::null_mut();
        let ctx: *mut AnyObject =
            msg_send![cls, sharedServiceContextForDeveloperDir: &*dir, error: &mut err];
        if ctx.is_null() {
            return Err("CoreSimulator did not return a service context".into());
        }
        let mut err: *mut AnyObject = ptr::null_mut();
        let set: *mut AnyObject = msg_send![&*ctx, defaultDeviceSetWithError: &mut err];
        if set.is_null() {
            return Err("CoreSimulator did not return the default device set".into());
        }
        let devices = kvc(set, "devices");
        if devices.is_null() {
            return Err("the device set has no devices".into());
        }
        let count: usize = msg_send![&*devices, count];
        for i in 0..count {
            let dev: *mut AnyObject = msg_send![&*devices, objectAtIndex: i];
            let uuid_obj = kvc(dev, "UDID");
            let uuid_str: *mut AnyObject = msg_send![&*uuid_obj, UUIDString];
            if nsstring(uuid_str).as_deref() == Some(udid) {
                let state = nsstring(kvc(dev, "stateString")).unwrap_or_default();
                if state != "Booted" {
                    return Err(format!("device {udid} is {state}; boot it first"));
                }
                return Ok(dev);
            }
        }
        Err(format!("device {udid} not found"))
    }
}

unsafe fn surface_size(desc: *mut AnyObject) -> (usize, usize) {
    if desc.is_null() || !responds(desc, sel!(framebufferSurface)) {
        return (0, 0);
    }
    let surf: *mut AnyObject = msg_send![&*desc, performSelector: sel!(framebufferSurface)];
    if surf.is_null() {
        return (0, 0);
    }
    let s = surf as *mut c_void;
    (IOSurfaceGetWidth(s), IOSurfaceGetHeight(s))
}

fn find_framebuffer(device: *mut AnyObject) -> Result<*mut AnyObject, String> {
    unsafe {
        let io: *mut AnyObject = msg_send![&*device, io];
        if io.is_null() {
            return Err("the device has no IO client".into());
        }
        let _: () = msg_send![&*io, updateIOPorts];
        let ports: *mut AnyObject = msg_send![&*io, deviceIOPorts];
        if ports.is_null() {
            return Err("the device has no IO ports".into());
        }
        let count: usize = msg_send![&*ports, count];
        let mut first: Option<*mut AnyObject> = None;
        let mut live: Option<(*mut AnyObject, usize, usize)> = None;
        for i in 0..count {
            let port: *mut AnyObject = msg_send![&*ports, objectAtIndex: i];
            if !responds(port, sel!(portIdentifier)) {
                continue;
            }
            let pid: *mut AnyObject = perform(port, sel!(portIdentifier));
            let Some(pid) = nsstring(pid) else { continue };
            if !pid.contains("framebuffer") && !pid.contains("display") {
                continue;
            }
            if !responds(port, sel!(descriptor)) {
                continue;
            }
            let desc: *mut AnyObject = perform(port, sel!(descriptor));
            if desc.is_null() {
                continue;
            }
            if first.is_none() && pid == "com.apple.framebuffer.display" {
                first = Some(desc);
            }
            let (w, h) = surface_size(desc);
            if w > 0 && h > 0 {
                let area = w * h;
                if live.map(|(_, lw, lh)| area > lw * lh).unwrap_or(true) {
                    live = Some((desc, w, h));
                }
            }
        }
        if let Some((desc, _, _)) = live {
            return Ok(desc);
        }
        first.ok_or_else(|| "no framebuffer display port found".into())
    }
}

fn make_hid(device: *mut AnyObject) -> Result<*mut AnyObject, String> {
    unsafe {
        let cls = AnyClass::get(c"_TtC12SimulatorKit24SimDeviceLegacyHIDClient")
            .ok_or_else(|| "SimulatorKit HID client is unavailable".to_string())?;
        let alloc: *mut AnyObject = msg_send![cls, alloc];
        let mut err: *mut AnyObject = ptr::null_mut();
        let client: *mut AnyObject = msg_send![&*alloc, initWithDevice: &*device, error: &mut err];
        if client.is_null() {
            let msg = nsstring(err).unwrap_or_else(|| "unknown error".into());
            return Err(format!("cannot create the HID client: {msg}"));
        }
        Ok(client)
    }
}

pub struct Sim {
    descriptor: *mut AnyObject,
    hid: *mut AnyObject,
    width: usize,
    height: usize,
    last_seed: u32,
}

impl Sim {
    pub fn open(udid: &str) -> Result<Sim, String> {
        let device = find_device(udid)?;
        let mut descriptor = ptr::null_mut();
        let mut width = 0;
        let mut height = 0;
        // A freshly booted device can take a moment to expose a live surface.
        for _ in 0..25 {
            descriptor = find_framebuffer(device)?;
            let (w, h) = unsafe { surface_size(descriptor) };
            if w > 0 && h > 0 {
                width = w;
                height = h;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if width == 0 || height == 0 {
            return Err(
                "the framebuffer surface is not ready; is the device showing a screen?".into(),
            );
        }
        let hid = make_hid(device)?;
        Ok(Sim {
            descriptor,
            hid,
            width,
            height,
            last_seed: u32::MAX,
        })
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Sample the current framebuffer into an RGBA image of `dst_w` by `dst_h`.
    /// Returns None while the screen did not change since the last call.
    pub fn frame_rgba(&mut self, dst_w: usize, dst_h: usize) -> Option<Vec<u8>> {
        if dst_w == 0 || dst_h == 0 {
            return None;
        }
        unsafe {
            let surf: *mut AnyObject = perform(self.descriptor, sel!(framebufferSurface));
            if surf.is_null() {
                return None;
            }
            let surf_c = surf as *mut c_void;
            let seed = IOSurfaceGetSeed(surf_c);
            if seed == self.last_seed {
                return None;
            }
            let mut lock_seed = 0u32;
            if IOSurfaceLock(surf_c, 1, &mut lock_seed) != 0 {
                return None;
            }
            let w = IOSurfaceGetWidth(surf_c);
            let h = IOSurfaceGetHeight(surf_c);
            let bpr = IOSurfaceGetBytesPerRow(surf_c);
            let fmt = IOSurfaceGetPixelFormat(surf_c);
            let base = IOSurfaceGetBaseAddress(surf_c) as *const u8;
            let mut out = None;
            if !base.is_null() && w > 0 && h > 0 && bpr >= w * 4 {
                let src = std::slice::from_raw_parts(base, bpr * h);
                let mut rgba = vec![0u8; dst_w * dst_h * 4];
                let bgra =
                    fmt == u32::from_be_bytes(*b"BGRA") || fmt == u32::from_be_bytes(*b"BGRX");
                for dy in 0..dst_h {
                    let sy = dy * h / dst_h;
                    let row = &src[sy * bpr..];
                    for dx in 0..dst_w {
                        let sx = dx * w / dst_w;
                        let i = sx * 4;
                        let o = (dy * dst_w + dx) * 4;
                        if bgra {
                            rgba[o] = row[i + 2];
                            rgba[o + 1] = row[i + 1];
                            rgba[o + 2] = row[i];
                        } else {
                            rgba[o] = row[i];
                            rgba[o + 1] = row[i + 1];
                            rgba[o + 2] = row[i + 2];
                        }
                        rgba[o + 3] = 255;
                    }
                }
                self.last_seed = seed;
                out = Some(rgba);
            }
            IOSurfaceUnlock(surf_c, 1, &mut lock_seed);
            out
        }
    }

    fn send_message(&self, msg: *mut c_void) {
        if msg.is_null() {
            return;
        }
        unsafe {
            type SendFn = unsafe extern "C" fn(
                *mut AnyObject,
                Sel,
                *mut c_void,
                i8,
                *mut c_void,
                *mut c_void,
            );
            let send: SendFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
            send(
                self.hid,
                sel!(sendWithMessage:freeWhenDone:completionQueue:completion:),
                msg,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
    }

    fn mouse_message(&self, x: f64, y: f64, down: bool, edge: u32) -> *mut c_void {
        unsafe {
            let sym = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"IndigoHIDMessageForMouseNSEvent".as_ptr(),
            );
            if sym.is_null() {
                return ptr::null_mut();
            }
            type F = unsafe extern "C" fn(
                *const CGPoint,
                *const CGPoint,
                u32,
                i32,
                f64,
                f64,
                u32,
            ) -> *mut c_void;
            let f: F = std::mem::transmute(sym);
            let p = CGPoint { x, y };
            f(
                &p,
                ptr::null(),
                0x32,
                if down { 1 } else { 2 },
                1.0,
                1.0,
                edge,
            )
        }
    }

    pub fn touch(&self, x: f64, y: f64, down: bool, edge: u32) {
        let msg = self.mouse_message(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0), down, edge);
        self.send_message(msg);
    }

    pub fn swipe(&self, x1: f64, y1: f64, x2: f64, y2: f64, edge: u32, steps: usize) {
        let steps = steps.max(1);
        self.touch(x1, y1, true, edge);
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            self.touch(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t, true, edge);
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        self.touch(x2, y2, false, edge);
    }

    pub fn home(&self) {
        // A hardware Home press dismisses the app switcher and system sheets,
        // which the bottom-edge swipe alone can leave open. Current Xcode may
        // drop the press, so the swipe still runs afterwards.
        self.press_button(0x0);
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.swipe(0.5, 0.995, 0.5, 0.5, 3, 12);
    }

    pub fn press_button(&self, source: i32) {
        unsafe {
            let sym = libc::dlsym(libc::RTLD_DEFAULT, c"IndigoHIDMessageForButton".as_ptr());
            if sym.is_null() {
                return;
            }
            type F = unsafe extern "C" fn(i32, i32, i32) -> *mut c_void;
            let f: F = std::mem::transmute(sym);
            let down = f(source, 1, 0x33);
            self.send_message(down);
            std::thread::sleep(std::time::Duration::from_millis(60));
            let up = f(source, 2, 0x33);
            self.send_message(up);
        }
    }

    pub fn lock(&self) {
        self.press_button(0x1);
    }

    pub fn app_switcher(&self) {
        self.press_button(0x0);
        std::thread::sleep(std::time::Duration::from_millis(180));
        self.press_button(0x0);
    }

    pub fn back(&self) {
        self.swipe(0.002, 0.5, 0.55, 0.5, 1, 10);
    }

    pub fn control_center(&self) {
        self.swipe(0.95, 0.002, 0.95, 0.55, 2, 12);
    }

    pub fn notification_center(&self) {
        self.swipe(0.12, 0.002, 0.12, 0.75, 2, 12);
    }

    pub fn volume(&self, up: bool) {
        unsafe {
            let sym = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"IndigoHIDMessageForHIDArbitrary".as_ptr(),
            );
            if sym.is_null() {
                return;
            }
            type F = unsafe extern "C" fn(u32, u32, u32, u32) -> *mut c_void;
            let f: F = std::mem::transmute(sym);
            let usage = if up { 233 } else { 234 };
            let down = f(0x32, 0x0C, usage, 1);
            self.send_message(down);
            std::thread::sleep(std::time::Duration::from_millis(60));
            let up = f(0x32, 0x0C, usage, 2);
            self.send_message(up);
        }
    }

    pub fn key(&self, usage: u32, down: bool) {
        unsafe {
            let sym = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"IndigoHIDMessageForKeyboardArbitrary".as_ptr(),
            );
            if sym.is_null() {
                return;
            }
            type F = unsafe extern "C" fn(u32, u32) -> *mut c_void;
            let f: F = std::mem::transmute(sym);
            let msg = f(usage, if down { 1 } else { 2 });
            self.send_message(msg);
        }
    }

    pub fn type_key(&self, usage: u32, shift: bool, ctrl: bool) {
        let modifiers = [(0xE1, shift), (0xE0, ctrl)];
        for (m, on) in modifiers {
            if on {
                self.key(m, true);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        self.key(usage, true);
        std::thread::sleep(std::time::Duration::from_millis(8));
        self.key(usage, false);
        for (m, on) in modifiers.iter().rev() {
            if *on {
                std::thread::sleep(std::time::Duration::from_millis(5));
                self.key(*m, false);
            }
        }
    }
}

#[allow(dead_code)]
fn _keep_dispatch() {
    // Kept for the callback path used by future frame streaming.
    let label = CString::new("multi-simu").unwrap();
    unsafe {
        let _ = dispatch_queue_create(label.as_ptr(), ptr::null_mut());
    }
}

#[allow(dead_code)]
fn _keep_cstr(s: &CStr) -> &CStr {
    s
}

// ---- Target implementation -------------------------------------------------

use crate::target::{Action, SpecialKey, Target};

fn usage_for_char(c: char) -> Option<(u32, bool)> {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some((0x04 + (lower as u32 - 'a' as u32), c.is_ascii_uppercase()));
    }
    let shifted = match c {
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '~' => ('`', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        _ => (c, false),
    };
    let (base, shift) = shifted;
    let usage = match base {
        '1'..='9' => 0x1E + (base as u32 - '1' as u32),
        '0' => 0x27,
        '-' => 0x2D,
        '=' => 0x2E,
        '[' => 0x2F,
        ']' => 0x30,
        '\\' => 0x31,
        ';' => 0x33,
        '\'' => 0x34,
        '`' => 0x35,
        ',' => 0x36,
        '.' => 0x37,
        '/' => 0x38,
        ' ' => 0x2C,
        _ => return None,
    };
    Some((usage, shift))
}

fn special_usage(key: SpecialKey) -> u32 {
    match key {
        SpecialKey::Enter => 0x28,
        SpecialKey::Esc => 0x29,
        SpecialKey::Backspace => 0x2A,
        SpecialKey::Tab => 0x2B,
        SpecialKey::Delete => 0x4C,
        SpecialKey::Right => 0x4F,
        SpecialKey::Left => 0x50,
        SpecialKey::Down => 0x51,
        SpecialKey::Up => 0x52,
        SpecialKey::Home => 0x4A,
        SpecialKey::End => 0x4D,
        SpecialKey::PageUp => 0x4B,
        SpecialKey::PageDown => 0x4E,
    }
}

impl Target for Sim {
    fn size(&self) -> (usize, usize) {
        Sim::size(self)
    }

    fn frame_rgba(&mut self, width: usize, height: usize) -> Option<Vec<u8>> {
        Sim::frame_rgba(self, width, height)
    }

    fn action(&mut self, action: Action) {
        match action {
            Action::Home => self.home(),
            Action::Switcher => self.app_switcher(),
            Action::Back => self.back(),
            Action::Lock => self.lock(),
            Action::Center => self.control_center(),
            Action::Notify => self.notification_center(),
            Action::VolDown => self.volume(false),
            Action::VolUp => self.volume(true),
        }
    }

    fn touch(&mut self, x: f64, y: f64, down: bool) {
        Sim::touch(self, x, y, down, 0);
    }

    fn swipe(&mut self, x1: f64, y1: f64, x2: f64, y2: f64) {
        Sim::swipe(self, x1, y1, x2, y2, 0, 10);
    }

    fn char(&mut self, c: char, shift: bool, ctrl: bool) {
        if let Some((usage, shifted)) = usage_for_char(c) {
            self.type_key(usage, shift || shifted, ctrl);
        }
    }

    fn special(&mut self, key: SpecialKey) {
        let usage = special_usage(key);
        self.key(usage, true);
        std::thread::sleep(std::time::Duration::from_millis(8));
        self.key(usage, false);
    }
}
