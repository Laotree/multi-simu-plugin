//! Web target: headless Chrome frames and input through the DevTools Protocol.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::target::{Action, SpecialKey, Target};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum WebInput {
    Mouse { x: f64, y: f64, down: bool },
    Text(String),
    Key { key: String, code: String, vk: i64 },
    Navigate(String),
    Eval(String),
    Viewport { width: usize, height: usize },
}

pub struct Web {
    #[allow(dead_code)]
    rt: tokio::runtime::Runtime,
    input: UnboundedSender<WebInput>,
    frames: Receiver<Vec<u8>>,
    size: (usize, usize),
    chrome: Child,
    server: Option<Child>,
    profile: PathBuf,
    url: String,
}

fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http URLs are supported: {url}"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (hostport.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

fn http_get(host: &str, port: u16, path: &str) -> Result<String, String> {
    // curl handles chunked responses and connection details that the DevTools
    // endpoint uses; the request is always to loopback.
    let url = format!("http://{host}:{port}{path}");
    let out = Command::new("curl")
        .args(["-s", "--max-time", "3", &url])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn wait_for_url(url: &str, timeout: Duration) -> Result<(), String> {
    let (host, port, path) = parse_url(url)?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if http_get(&host, port, &path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err(format!("timed out waiting for {url}"))
}

fn chrome_path() -> Result<String, String> {
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    for path in candidates {
        if std::path::Path::new(path).exists() {
            return Ok(path.to_string());
        }
    }
    for name in ["google-chrome", "chromium", "chromium-browser"] {
        if Command::new("which")
            .arg(name)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(name.to_string());
        }
    }
    Err("Chrome or Chromium is required".into())
}

fn launch_chrome(chrome: &str, profile: &Path, size: (usize, usize)) -> Result<Child, String> {
    let child = Command::new(chrome)
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-extensions",
            "--disable-sync",
            "--mute-audio",
            "--hide-scrollbars",
            "--force-device-scale-factor=1",
            "--window-size=1280,800",
            "about:blank",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start Chrome: {e}"))?;
    let _ = size;
    Ok(child)
}

fn page_ws_url(debug_port: u16) -> Result<String, String> {
    let body = http_get("127.0.0.1", debug_port, "/json/list")?;
    let targets: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("cannot parse DevTools targets: {e}"))?;
    let list = targets.as_array().ok_or("DevTools returned no targets")?;
    for target in list {
        if target["type"] == "page" {
            if let Some(url) = target["webSocketDebuggerUrl"].as_str() {
                return Ok(url.to_string());
            }
        }
    }
    Err("no page target in Chrome".into())
}

fn devtools_port(profile: &Path) -> Result<u16, String> {
    let file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Some(line) = text.lines().next() {
                if let Ok(port) = line.trim().parse() {
                    return Ok(port);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err("Chrome did not open its DevTools port".into())
}

fn key_params(key: SpecialKey) -> (String, String, i64) {
    let (key, code, vk) = match key {
        SpecialKey::Enter => ("Enter", "Enter", 13),
        SpecialKey::Esc => ("Escape", "Escape", 27),
        SpecialKey::Backspace => ("Backspace", "Backspace", 8),
        SpecialKey::Tab => ("Tab", "Tab", 9),
        SpecialKey::Delete => ("Delete", "Delete", 46),
        SpecialKey::Up => ("ArrowUp", "ArrowUp", 38),
        SpecialKey::Down => ("ArrowDown", "ArrowDown", 40),
        SpecialKey::Left => ("ArrowLeft", "ArrowLeft", 37),
        SpecialKey::Right => ("ArrowRight", "ArrowRight", 39),
        SpecialKey::Home => ("Home", "Home", 36),
        SpecialKey::End => ("End", "End", 35),
        SpecialKey::PageUp => ("PageUp", "PageUp", 33),
        SpecialKey::PageDown => ("PageDown", "PageDown", 34),
    };
    (key.to_string(), code.to_string(), vk)
}

fn spawn_ws(
    rt: &tokio::runtime::Runtime,
    ws: Ws,
    mut input: tokio::sync::mpsc::UnboundedReceiver<WebInput>,
    frames: std::sync::mpsc::Sender<Vec<u8>>,
    size: (usize, usize),
    url: String,
) {
    rt.spawn(async move {
        let (mut write, mut read) = ws.split();
        let mut id = 0i64;
        macro_rules! send {
            ($method:expr, $params:expr) => {{
                id += 1;
                let msg = serde_json::json!({"id": id, "method": $method, "params": $params});
                let _ = write.send(Message::Text(msg.to_string().into())).await;
            }};
        }
        send!("Page.enable", serde_json::json!({}));
        send!(
            "Emulation.setDeviceMetricsOverride",
            serde_json::json!({"width": size.0, "height": size.1, "deviceScaleFactor": 1, "mobile": false})
        );
        send!("Page.navigate", serde_json::json!({"url": url}));
        send!(
            "Page.startScreencast",
            serde_json::json!({"format": "jpeg", "quality": 75, "maxWidth": size.0, "maxHeight": size.1, "everyNthFrame": 1})
        );

        let mut pressed = false;
        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                if v["method"] == "Page.screencastFrame" {
                                    let data = v["params"]["data"].as_str().unwrap_or("");
                                    if let Some(session) = v["params"]["sessionId"].as_i64() {
                                        id += 1;
                                        let ack = serde_json::json!({"id": id, "method": "Page.screencastFrameAck", "params": {"sessionId": session}});
                                        let _ = write.send(Message::Text(ack.to_string().into())).await;
                                    }
                                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                                        let _ = frames.send(bytes);
                                    }
                                }
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
                item = input.recv() => {
                    let Some(item) = item else { break };
                    match item {
                        WebInput::Mouse { x, y, down } => {
                            let (kind, buttons) = if down {
                                if pressed { ("mouseMoved", 1) } else { ("mousePressed", 1) }
                            } else {
                                ("mouseReleased", 0)
                            };
                            pressed = down;
                            send!("Input.dispatchMouseEvent", serde_json::json!({
                                "type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1
                            }));
                        }
                        WebInput::Text(text) => {
                            send!("Input.insertText", serde_json::json!({"text": text}));
                        }
                        WebInput::Key { key, code, vk } => {
                            let text = if key == "Enter" { "\r" } else { "" };
                            send!("Input.dispatchKeyEvent", serde_json::json!({
                                "type": "keyDown", "key": key, "code": code,
                                "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "text": text
                            }));
                            send!("Input.dispatchKeyEvent", serde_json::json!({
                                "type": "keyUp", "key": key, "code": code,
                                "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk
                            }));
                        }
                        WebInput::Navigate(url) => {
                            send!("Page.navigate", serde_json::json!({"url": url}));
                        }
                        WebInput::Eval(expression) => {
                            send!("Runtime.evaluate", serde_json::json!({"expression": expression}));
                        }
                        WebInput::Viewport { width, height } => {
                            send!("Page.stopScreencast", serde_json::json!({}));
                            send!(
                                "Emulation.setDeviceMetricsOverride",
                                serde_json::json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false})
                            );
                            send!(
                                "Page.startScreencast",
                                serde_json::json!({"format": "jpeg", "quality": 75, "maxWidth": width, "maxHeight": height, "everyNthFrame": 1})
                            );
                        }
                    }
                }
            }
        }
    });
}

impl Web {
    pub fn open(url: &str, command: Option<&str>, cwd: &std::path::Path) -> Result<Web, String> {
        let mut server = None;
        if let Some(command) = command {
            let log =
                std::fs::File::create("/tmp/multi-simu-web.log").map_err(|e| e.to_string())?;
            let log_err = log.try_clone().map_err(|e| e.to_string())?;
            server = Some(
                Command::new("/bin/sh")
                    .arg("-lc")
                    .arg(command)
                    .current_dir(cwd)
                    .stdout(Stdio::from(log))
                    .stderr(Stdio::from(log_err))
                    .spawn()
                    .map_err(|e| format!("cannot start the web command: {e}"))?,
            );
            wait_for_url(url, Duration::from_secs(90))?;
        }

        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let (cw, ch) = crate::terminal::rex_cell_size().unwrap_or((8, 16));
        let area_w = (cols as usize * cw).max(1);
        let area_h = (rows.saturating_sub(1) as usize * ch).max(1);
        let width = area_w.clamp(320, 1280);
        let height =
            ((width as f64 * area_h as f64 / area_w as f64).round() as usize).clamp(240, 1600);
        let size = (width, height);

        let profile = PathBuf::from(format!("/tmp/multi-simu-chrome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
        let chrome = launch_chrome(&chrome_path()?, &profile, size)?;
        let port = devtools_port(&profile)?;
        let ws_url = page_ws_url(port)?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let (ws, _) = rt
            .block_on(connect_async(ws_url.clone()))
            .map_err(|e| format!("cannot connect to Chrome DevTools: {e}"))?;
        let (tx, rx) = channel();
        let (input_tx, input_rx) = unbounded_channel();
        spawn_ws(&rt, ws, input_rx, tx, size, url.to_string());

        Ok(Web {
            rt,
            input: input_tx,
            frames: rx,
            size,
            chrome,
            server,
            profile,
            url: url.to_string(),
        })
    }

    fn send(&self, item: WebInput) {
        let _ = self.input.send(item);
    }

    fn cleanup(&mut self) {
        let _ = self.chrome.kill();
        let _ = self.chrome.wait();
        if let Some(server) = &mut self.server {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Target for Web {
    fn size(&self) -> (usize, usize) {
        self.size
    }

    fn frame_rgba(&mut self, width: usize, height: usize) -> Option<Vec<u8>> {
        let mut latest = None;
        while let Ok(bytes) = self.frames.try_recv() {
            latest = Some(bytes);
        }
        let bytes = latest?;
        let image = image::load_from_memory(&bytes).ok()?;
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
            Action::Home => self.send(WebInput::Navigate(self.url.clone())),
            Action::Back => self.send(WebInput::Eval("history.back()".into())),
            _ => {}
        }
    }

    fn touch(&mut self, x: f64, y: f64, down: bool) {
        self.send(WebInput::Mouse {
            x: x.clamp(0.0, 1.0) * self.size.0 as f64,
            y: y.clamp(0.0, 1.0) * self.size.1 as f64,
            down,
        });
    }

    fn swipe(&mut self, x1: f64, y1: f64, x2: f64, y2: f64) {
        self.touch(x1, y1, true);
        for i in 1..=10 {
            let t = i as f64 / 10.0;
            self.touch(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t, true);
            std::thread::sleep(Duration::from_millis(12));
        }
        self.touch(x2, y2, false);
    }

    fn char(&mut self, c: char, shift: bool, _ctrl: bool) {
        let text = if shift { c.to_ascii_uppercase() } else { c };
        self.send(WebInput::Text(text.to_string()));
    }

    fn special(&mut self, key: SpecialKey) {
        let (key, code, vk) = key_params(key);
        self.send(WebInput::Key { key, code, vk });
    }

    fn resize(&mut self, width: usize, height: usize) {
        if width == 0 || height == 0 {
            return;
        }
        let vw = width.clamp(320, 1280);
        let vh = ((height as f64 * vw as f64 / width as f64).round() as usize).clamp(240, 1600);
        if (vw, vh) != self.size {
            self.size = (vw, vh);
            self.send(WebInput::Viewport {
                width: vw,
                height: vh,
            });
        }
    }

    fn shutdown(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::parse_url;

    #[test]
    fn parses_http_url() {
        assert_eq!(
            parse_url("http://127.0.0.1:8080/path").expect("url"),
            ("127.0.0.1".to_string(), 8080, "/path".to_string())
        );
        assert_eq!(
            parse_url("http://example.com").expect("url"),
            ("example.com".to_string(), 80, "/".to_string())
        );
    }
}
