//! Android emulator frames and input through the emulator's gRPC controller.
//!
//! The emulator must expose its gRPC bridge (`-grpc <port>`). Frames come from
//! `streamScreenshot`; touches and keys go through `sendTouch` and `sendKey`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use crate::target::{Action, SpecialKey, Target};

pub mod pb {
    tonic::include_proto!("android.emulation.control");
}

use pb::emulator_controller_client::EmulatorControllerClient;
use pb::ImageFormat;

pub struct Android {
    #[allow(dead_code)]
    rt: tokio::runtime::Runtime,
    serial: String,
    size: (u32, u32),
    frames: Receiver<Vec<u8>>,
    down: Option<(i32, i32)>,
    last: (i32, i32),
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn adb(serial: &str, args: &[&str]) -> Result<String, String> {
    run(Command::new("adb").arg("-s").arg(serial).args(args))
}

fn find_serial(avd: &str) -> Option<String> {
    let out = run(Command::new("adb").arg("devices")).ok()?;
    for line in out.lines() {
        let Some(serial) = line.split_whitespace().next() else {
            continue;
        };
        if !serial.starts_with("emulator-") {
            continue;
        }
        if let Ok(name) = adb(serial, &["emu", "avd", "name"]) {
            if name.lines().next().unwrap_or("").trim() == avd {
                return Some(serial.to_string());
            }
        }
    }
    None
}

fn boot_emulator(avd: &str) -> Result<String, String> {
    let sdk = std::env::var("ANDROID_HOME")
        .or_else(|_| std::env::var("ANDROID_SDK_ROOT"))
        .unwrap_or_else(|_| {
            format!(
                "{}/Library/Android/sdk",
                std::env::var("HOME").unwrap_or_default()
            )
        });
    let emulator = format!("{sdk}/emulator/emulator");
    Command::new(&emulator)
        .args(["-avd", avd, "-no-window", "-no-audio", "-grpc", "8554"])
        .spawn()
        .map_err(|e| format!("cannot start {emulator}: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if let Some(serial) = find_serial(avd) {
            if adb(&serial, &["shell", "getprop", "sys.boot_completed"])
                .map(|s| s.trim() == "1")
                .unwrap_or(false)
            {
                return Ok(serial);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("{avd} did not finish booting"))
}

fn grpc_port(avd: &str) -> Result<Option<u16>, String> {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Library/Caches/TemporaryItems/avd/running");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|e| e.to_str()) != Some("ini") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let mut id = None;
                let mut port = None;
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("avd.id=") {
                        id = Some(v.trim());
                    }
                    if let Some(v) = line.strip_prefix("grpc.port=") {
                        port = v.trim().parse::<u16>().ok();
                    }
                }
                if id == Some(avd) {
                    return Ok(port);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err(format!(
        "no gRPC port for {avd}; start it with -grpc <port>"
    ))
}

fn device_size(serial: &str) -> (u32, u32) {
    let out = adb(serial, &["shell", "wm", "size"]).unwrap_or_default();
    let mut size = (1080, 1920);
    for line in out.lines() {
        if let Some(pos) = line.find(':') {
            let value = line[pos + 1..].trim();
            if let Some((w, h)) = value.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                    size = (w, h);
                    if line.starts_with("Override") {
                        break;
                    }
                }
            }
        }
    }
    size
}

fn special_code(key: SpecialKey) -> i32 {
    match key {
        SpecialKey::Enter => 66,
        SpecialKey::Esc => 111,
        SpecialKey::Backspace => 67,
        SpecialKey::Tab => 61,
        SpecialKey::Delete => 112,
        SpecialKey::Up => 19,
        SpecialKey::Down => 20,
        SpecialKey::Left => 21,
        SpecialKey::Right => 22,
        SpecialKey::Home => 3,
        SpecialKey::End => 123,
        SpecialKey::PageUp => 92,
        SpecialKey::PageDown => 93,
    }
}

impl Android {
    pub fn open(avd: &str) -> Result<Android, String> {
        let serial = match find_serial(avd) {
            Some(s) => s,
            None => boot_emulator(avd)?,
        };
        let size = device_size(&serial);
        let Some(port) = grpc_port(avd)? else {
            return Err(format!(
                "{avd} has no gRPC port; start it with -grpc <port>"
            ));
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let endpoint = format!("http://127.0.0.1:{port}");
        let mut last_err = String::new();
        let mut client = None;
        for _ in 0..30 {
            match rt.block_on(EmulatorControllerClient::connect(endpoint.clone())) {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
        let client =
            client.ok_or_else(|| format!("cannot connect to emulator gRPC: {last_err}"))?;

        let (tx, frames) = channel();
        let mut stream_client = client.clone();
        rt.spawn(async move {
            let format = ImageFormat {
                format: 0,
                width: 540,
                height: 0,
                display: 0,
            };
            match stream_client
                .stream_screenshot(tonic::Request::new(format))
                .await
            {
                Ok(response) => {
                    let mut stream = response.into_inner();
                    loop {
                        match stream.message().await {
                            Ok(Some(image)) => {
                                if tx.send(image.image).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }
                Err(e) => eprintln!("android screenshot stream: {e}"),
            }
        });

        Ok(Android {
            rt,
            serial,
            size,
            frames,
            down: None,
            last: (0, 0),
        })
    }

    fn shell(&self, args: &[&str]) {
        let _ = Command::new("adb")
            .arg("-s")
            .arg(&self.serial)
            .arg("shell")
            .args(args)
            .status();
    }

    fn keyevent(&self, code: i32) {
        self.shell(&["input", "keyevent", &code.to_string()]);
    }

    fn text(&self, text: &str) {
        // `input text` passes its argument through the device shell, where a
        // space splits words; %s is the documented escape for a space.
        let escaped = text.replace(' ', "%s");
        self.shell(&["input", "text", &escaped]);
    }
}

impl Target for Android {
    fn size(&self) -> (usize, usize) {
        (self.size.0 as usize, self.size.1 as usize)
    }

    fn frame_rgba(&mut self, width: usize, height: usize) -> Option<Vec<u8>> {
        let mut latest = None;
        while let Ok(png) = self.frames.try_recv() {
            latest = Some(png);
        }
        let png = latest?;
        let image = image::load_from_memory(&png).ok()?;
        let resized = image::imageops::resize(
            &image,
            width as u32,
            height as u32,
            image::imageops::FilterType::Triangle,
        );
        Some(resized.into_raw())
    }

    fn action(&mut self, action: Action) {
        match action {
            Action::Home => self.keyevent(3),
            Action::Switcher => self.keyevent(187),
            Action::Back => self.keyevent(4),
            Action::Lock => self.keyevent(26),
            Action::Center => self.shell(&["cmd", "statusbar", "expand-settings"]),
            Action::Notify => self.shell(&["cmd", "statusbar", "expand-notifications"]),
            Action::VolDown => self.keyevent(25),
            Action::VolUp => self.keyevent(24),
        }
    }

    fn touch(&mut self, x: f64, y: f64, down: bool) {
        let px = (x.clamp(0.0, 1.0) * self.size.0 as f64) as i32;
        let py = (y.clamp(0.0, 1.0) * self.size.1 as f64) as i32;
        if down {
            if self.down.is_none() {
                self.down = Some((px, py));
            }
            self.last = (px, py);
        } else if let Some((sx, sy)) = self.down.take() {
            if (sx - px).abs() < 4 && (sy - py).abs() < 4 {
                self.shell(&["input", "tap", &sx.to_string(), &sy.to_string()]);
            } else {
                self.shell(&[
                    "input",
                    "swipe",
                    &sx.to_string(),
                    &sy.to_string(),
                    &px.to_string(),
                    &py.to_string(),
                    "250",
                ]);
            }
        }
    }

    fn swipe(&mut self, x1: f64, y1: f64, x2: f64, y2: f64) {
        self.touch(x1, y1, true);
        self.touch(x2, y2, false);
    }

    fn char(&mut self, c: char, _shift: bool, _ctrl: bool) {
        self.text(&c.to_string());
    }

    fn special(&mut self, key: SpecialKey) {
        self.keyevent(special_code(key));
    }
}
