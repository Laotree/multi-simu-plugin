//! Probe: headless iOS simulator capture and touch injection through
//! CoreSimulator / SimulatorKit private frameworks.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use block2::{ManualBlockEncoding, RcBlock};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{msg_send, sel};
use objc2_foundation::NSString;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

struct Frame {
    width: usize,
    height: usize,
    bytes_per_row: usize,
    pixel_format: u32,
    data: Vec<u8>,
}

struct VoidEncoding;
// SAFETY: A block with no arguments and no return value.
unsafe impl ManualBlockEncoding for VoidEncoding {
    type Arguments = ();
    type Return = ();
    const ENCODING_CSTR: &'static std::ffi::CStr = c"v8@?0";
}

static FRAME: Mutex<Option<Frame>> = Mutex::new(None);

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *mut c_void) -> u32;
}

extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
    fn NSSetUncaughtExceptionHandler(f: extern "C" fn(*mut AnyObject));
}

extern "C" fn exc_handler(exc: *mut AnyObject) {
    unsafe {
        if exc.is_null() { eprintln!("EXC: <null>"); return; }
        let name: *mut AnyObject = msg_send![&*exc, name];
        let reason: *mut AnyObject = msg_send![&*exc, reason];
        eprintln!("EXC name={:?} reason={:?}", nsstring(name), nsstring(reason));
    }
}

fn load(path: &str) -> bool {
    let c = CString::new(path).unwrap();
    unsafe {
        let h = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if h.is_null() {
            let e = libc::dlerror();
            let msg = if e.is_null() {
                "unknown".into()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            };
            eprintln!("dlopen {path}: {msg}");
            false
        } else {
            true
        }
    }
}

fn load_frameworks() {
    let dev = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "/Applications/Xcode.app/Contents/Developer".into());
    let xcode = dev.trim_end_matches("/Contents/Developer");
    let paths = [
        "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator".to_string(),
        format!("{dev}/Library/PrivateFrameworks/CoreSimulator.framework/CoreSimulator"),
        format!("{xcode}/Contents/SharedFrameworks/SimulatorKit.framework/SimulatorKit"),
        format!("{dev}/Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit"),
    ];
    for p in &paths {
        load(p);
    }
}

unsafe fn nsstring(ptr: *mut AnyObject) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s: &NSString = &*(ptr as *const NSString);
    Some(s.to_string())
}

unsafe fn kvc(obj: *mut AnyObject, key: &str) -> *mut AnyObject {
    if obj.is_null() {
        return ptr::null_mut();
    }
    let k = NSString::from_str(key);
    msg_send![&*obj, valueForKey: &*k]
}

unsafe fn find_device(udid: &str) -> Option<*mut AnyObject> {
    let cls = AnyClass::get(c"SimServiceContext")?;
    let dev_dir = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())?;
    let dev_ns = NSString::from_str(&dev_dir);
    let mut err: *mut AnyObject = ptr::null_mut();
    let ctx: *mut AnyObject =
        msg_send![cls, sharedServiceContextForDeveloperDir: &*dev_ns, error: &mut err];
    if ctx.is_null() {
        eprintln!("no SimServiceContext");
        return None;
    }
    let mut err: *mut AnyObject = ptr::null_mut();
    let set: *mut AnyObject = msg_send![&*ctx, defaultDeviceSetWithError: &mut err];
    if set.is_null() {
        eprintln!("no default device set");
        return None;
    }
    let devices = kvc(set, "devices");
    if devices.is_null() {
        eprintln!("no devices");
        return None;
    }
    let count: usize = msg_send![&*devices, count];
    for i in 0..count {
        let dev: *mut AnyObject = msg_send![&*devices, objectAtIndex: i];
        let uuid_obj = kvc(dev, "UDID");
        let uuid_str: *mut AnyObject = msg_send![&*uuid_obj, UUIDString];
        if nsstring(uuid_str).as_deref() == Some(udid) {
            return Some(dev);
        }
    }
    None
}

unsafe fn capture_surface(desc: *mut AnyObject) {
    let surf: *mut AnyObject = msg_send![&*desc, performSelector: sel!(framebufferSurface)];
    if surf.is_null() {
        return;
    }
    let surf_c = surf as *mut c_void;
    let mut seed = 0u32;
    if IOSurfaceLock(surf_c, 1, &mut seed) != 0 {
        return;
    }
    let width = IOSurfaceGetWidth(surf_c);
    let height = IOSurfaceGetHeight(surf_c);
    let bpr = IOSurfaceGetBytesPerRow(surf_c);
    let fmt = IOSurfaceGetPixelFormat(surf_c);
    let base = IOSurfaceGetBaseAddress(surf_c) as *const u8;
    if !base.is_null() && width > 0 && height > 0 {
        let len = bpr * height;
        let data = std::slice::from_raw_parts(base, len).to_vec();
        *FRAME.lock().unwrap() = Some(Frame {
            width,
            height,
            bytes_per_row: bpr,
            pixel_format: fmt,
            data,
        });
    }
    IOSurfaceUnlock(surf_c, 1, &mut seed);
}

unsafe fn register_capture(desc: *mut AnyObject) -> bool {
    let sel_reg = sel!(registerScreenCallbacksWithUUID:callbackQueue:frameCallback:surfacesChangedCallback:propertiesChangedCallback:);
    let responds: Bool = msg_send![&*desc, respondsToSelector: sel_reg];
    if !responds.as_bool() {
        eprintln!("descriptor does not respond to registerScreenCallbacks");
        return false;
    }
    let desc_addr = desc as usize;
    let frame = RcBlock::with_encoding::<(), (), _, VoidEncoding>(move || {
        let d = desc_addr as *mut AnyObject;
        unsafe { capture_surface(d) };
    });
    // A block that captures nothing becomes a global block, which has no
    // signature field; ROCK rejects those. Capture a dummy to keep it a
    // heap block with a generated signature.
    let dummy = 0usize;
    let noop1 = RcBlock::with_encoding::<(), (), _, VoidEncoding>(move || { let _ = dummy; });
    let noop2 = RcBlock::with_encoding::<(), (), _, VoidEncoding>(move || { let _ = dummy; });
    let frame_ptr = RcBlock::as_ptr(&frame) as *mut AnyObject;
    let noop1_ptr = RcBlock::as_ptr(&noop1) as *mut AnyObject;
    let noop2_ptr = RcBlock::as_ptr(&noop2) as *mut AnyObject;
    let label = CString::new("multi-simu.frames").unwrap();
    let queue = dispatch_queue_create(label.as_ptr(), ptr::null_mut());
    let uuid_cls = AnyClass::get(c"NSUUID").unwrap();
    let uuid: *mut AnyObject = msg_send![uuid_cls, UUID];
    type RegFn = unsafe extern "C" fn(
        *mut AnyObject,
        objc2::runtime::Sel,
        *mut AnyObject,
        *mut c_void,
        *mut AnyObject,
        *mut AnyObject,
        *mut AnyObject,
    );
    let f: RegFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
    match objc2::exception::catch(std::panic::AssertUnwindSafe(|| f(desc, sel_reg, uuid, queue, frame_ptr, noop1_ptr, noop2_ptr))) {
        Ok(()) => eprintln!("register call returned"),
        Err(e) => eprintln!("EXC caught: {:?}", e),
    }
    // The descriptor keeps the blocks for as long as the callbacks are registered.
    std::mem::forget(frame);
    std::mem::forget(noop1);
    std::mem::forget(noop2);
    true
}

unsafe fn responds(obj: *mut AnyObject, sel: objc2::runtime::Sel) -> bool {
    if obj.is_null() { return false; }
    let b: Bool = msg_send![&*obj, respondsToSelector: sel];
    b.as_bool()
}

unsafe fn class_name(obj: *mut AnyObject) -> String {
    if obj.is_null() { return "<nil>".into(); }
    let c = (&*obj).class();
    c.name().to_string_lossy().into_owned()
}

unsafe fn find_framebuffer(device: *mut AnyObject) -> Option<*mut AnyObject> {
    let io: *mut AnyObject = msg_send![&*device, io];
    if io.is_null() {
        eprintln!("device has no io");
        return None;
    }
    eprintln!("io class={}", class_name(io));
    eprintln!("io responds updateIOPorts={}", responds(io, sel!(updateIOPorts)));
    if responds(io, sel!(updateIOPorts)) {
        let _: () = msg_send![&*io, updateIOPorts];
    }
    let ports = if responds(io, sel!(deviceIOPorts)) {
        msg_send![&*io, deviceIOPorts]
    } else {
        kvc(io, "deviceIOPorts")
    };
    if ports.is_null() {
        eprintln!("no deviceIOPorts");
        return None;
    }
    eprintln!("ports class={}", class_name(ports));
    let count: usize = msg_send![&*ports, count];
    eprintln!("ports count={count}");
    let mut first: Option<*mut AnyObject> = None;
    let mut chosen: Option<*mut AnyObject> = None;
    for i in 0..count {
        let port: *mut AnyObject = msg_send![&*ports, objectAtIndex: i];
        let has_pid = responds(port, sel!(portIdentifier));
        let pid = if has_pid {
            msg_send![&*port, performSelector: sel!(portIdentifier)]
        } else {
            kvc(port, "portIdentifier")
        };
        let pid_s = nsstring(pid).unwrap_or_else(|| "<nil>".into());
        if !pid_s.contains("framebuffer") && !pid_s.contains("display") {
            continue;
        }
        let desc = if responds(port, sel!(descriptor)) {
            msg_send![&*port, performSelector: sel!(descriptor)]
        } else {
            kvc(port, "descriptor")
        };
        if desc.is_null() {
            continue;
        }
        let surf: *mut AnyObject = if responds(desc, sel!(framebufferSurface)) {
            msg_send![&*desc, performSelector: sel!(framebufferSurface)]
        } else {
            ptr::null_mut()
        };
        let (w, h) = if surf.is_null() {
            (0usize, 0usize)
        } else {
            (
                IOSurfaceGetWidth(surf as *mut c_void),
                IOSurfaceGetHeight(surf as *mut c_void),
            )
        };
        eprintln!("candidate {pid_s} desc={} surface={w}x{h}", class_name(desc));
        if first.is_none() {
            first = Some(desc);
        }
        if !surf.is_null() && w > 0 && h > 0 {
            chosen = Some(desc);
        }
    }
    chosen.or(first)
}

unsafe fn send_touch(client: *mut AnyObject, x: f64, y: f64, down: bool, edge: u32) {
    let sym = libc::dlsym(libc::RTLD_DEFAULT, c"IndigoHIDMessageForMouseNSEvent".as_ptr());
    if sym.is_null() {
        eprintln!("IndigoHIDMessageForMouseNSEvent not found");
        return;
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
    let event_type = if down { 1 } else { 2 };
    let msg = f(&p, ptr::null(), 0x32, event_type, 1.0, 1.0, edge);
    if msg.is_null() {
        eprintln!("touch message nil");
        return;
    }
    type SendFn = unsafe extern "C" fn(
        *mut AnyObject,
        objc2::runtime::Sel,
        *mut c_void,
        i8,
        *mut c_void,
        *mut c_void,
    );
    let send: SendFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
    send(
        client,
        sel!(sendWithMessage:freeWhenDone:completionQueue:completion:),
        msg,
        1,
        ptr::null_mut(),
        ptr::null_mut(),
    );
}

unsafe fn make_hid_client(device: *mut AnyObject) -> Option<*mut AnyObject> {
    let cls = AnyClass::get(c"_TtC12SimulatorKit24SimDeviceLegacyHIDClient")?;
    let alloc: *mut AnyObject = msg_send![cls, alloc];
    let mut err: *mut AnyObject = ptr::null_mut();
    let client: *mut AnyObject = msg_send![&*alloc, initWithDevice: &*device, error: &mut err];
    if client.is_null() {
        eprintln!("HID client init failed");
        return None;
    }
    Some(client)
}

fn save_png(path: &str) -> bool {
    let guard = FRAME.lock().unwrap();
    let Some(f) = guard.as_ref() else {
        eprintln!("no frame captured");
        return false;
    };
    eprintln!(
        "frame {}x{} bpr={} fmt=0x{:08x} len={}",
        f.width,
        f.height,
        f.bytes_per_row,
        f.pixel_format,
        f.data.len()
    );
    let w = f.width;
    if f.data.len() < f.bytes_per_row * f.height {
        eprintln!("short frame");
        return false;
    }
    let mut rgba = vec![0u8; w * f.height * 4];
    for y in 0..f.height {
        let row = &f.data[y * f.bytes_per_row..y * f.bytes_per_row + w * 4];
        for x in 0..w {
            let i = x * 4;
            let o = (y * w + x) * 4;
            let fmt = f.pixel_format;
            if fmt == u32::from_be_bytes(*b"BGRA") || fmt == u32::from_be_bytes(*b"BGRX") {
                rgba[o] = row[i + 2];
                rgba[o + 1] = row[i + 1];
                rgba[o + 2] = row[i];
                rgba[o + 3] = if fmt == u32::from_be_bytes(*b"BGRX") { 255 } else { row[i + 3] };
            } else if fmt == u32::from_be_bytes(*b"RGBA") || fmt == u32::from_be_bytes(*b"RGBX") {
                rgba[o] = row[i];
                rgba[o + 1] = row[i + 1];
                rgba[o + 2] = row[i + 2];
                rgba[o + 3] = if fmt == u32::from_be_bytes(*b"RGBX") { 255 } else { row[i + 3] };
            } else {
                rgba[o] = row[i + 2];
                rgba[o + 1] = row[i + 1];
                rgba[o + 2] = row[i];
                rgba[o + 3] = 255;
            }
        }
    }
    let img = image::RgbaImage::from_raw(w as u32, f.height as u32, rgba).unwrap();
    img.save(path).is_ok()
}

fn main() {
    load_frameworks();
    unsafe {
        let Some(udid) = std::env::args().nth(1) else {
            eprintln!("usage: ios-probe <simulator-udid>");
            return;
        };
        let Some(device) = find_device(&udid) else {
            eprintln!("device {udid} not found");
            return;
        };
        eprintln!("device found");
        let state = nsstring(kvc(device, "stateString")).unwrap_or_default();
        eprintln!("state={state}");

        let Some(desc) = find_framebuffer(device) else {
            eprintln!("no framebuffer descriptor");
            return;
        };
        eprintln!("framebuffer descriptor found");
        NSSetUncaughtExceptionHandler(exc_handler);
        let _ = register_capture(desc);
        std::thread::sleep(Duration::from_millis(800));
        // Poll the surface directly as a fallback.
        for _ in 0..5 {
            capture_surface(desc);
            if FRAME.lock().unwrap().is_some() { break; }
            std::thread::sleep(Duration::from_millis(300));
        }
        if save_png("/tmp/multi-simu-probe.png") {
            eprintln!("saved /tmp/multi-simu-probe.png");
        }

        let Some(client) = make_hid_client(device) else {
            eprintln!("no HID client");
            return;
        };
        eprintln!("HID client ready; swiping up from the bottom edge");
        send_touch(client, 0.5, 0.995, true, 3);
        for i in 1..=10 {
            let y = 0.995 - 0.045 * i as f64;
            send_touch(client, 0.5, y, true, 3);
            std::thread::sleep(Duration::from_millis(16));
        }
        send_touch(client, 0.5, 0.545, false, 3);
        eprintln!("swipe sent");
        std::thread::sleep(Duration::from_millis(700));
        *FRAME.lock().unwrap() = None;
        for _ in 0..5 {
            capture_surface(desc);
            if FRAME.lock().unwrap().is_some() { break; }
            std::thread::sleep(Duration::from_millis(200));
        }
        if save_png("/tmp/multi-simu-probe-after.png") {
            eprintln!("saved /tmp/multi-simu-probe-after.png");
        }
    }
    std::thread::sleep(Duration::from_millis(500));
}
