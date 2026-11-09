//! A small MCP server over stdio for coding agents.
//!
//! It exposes the configured targets as tools: start/stop, screenshot, tap,
//! swipe, type, key, and platform actions. The server keeps adapters alive
//! between calls, so an agent can drive a target without a terminal UI.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::config::Config;
use crate::make_target;
use crate::target::{Action, SpecialKey, Target};

pub fn serve(config_path: PathBuf) -> Result<(), String> {
    let (cfg, root) = Config::load(&config_path)?;
    let mut server = Server {
        cfg,
        root,
        adapters: HashMap::new(),
        last: HashMap::new(),
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = request.get("id").cloned();
        let method = request["method"].as_str().unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match method {
            "initialize" => Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "multi-simu", "version": env!("CARGO_PKG_VERSION")}
            })),
            "notifications/initialized" | "notifications/cancelled" => None,
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({ "tools": tools() })),
            "tools/call" => Some(server.call(params)),
            _ => Some(json!({
                "error": {"code": -32601, "message": format!("unknown method {method}")}
            })),
        };
        if let Some(result) = result {
            let Some(id) = id else { continue };
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(stdout, "{response}").map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

struct Server {
    cfg: Config,
    root: PathBuf,
    adapters: HashMap<String, Box<dyn Target>>,
    last: HashMap<String, (Vec<u8>, usize, usize)>,
}

impl Server {
    fn call(&mut self, params: Value) -> Value {
        let name = params["name"].as_str().unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match self.run(name, &args) {
            Ok(content) => json!({ "content": content }),
            Err(message) => json!({
                "content": [{"type": "text", "text": message}],
                "isError": true
            }),
        }
    }

    fn run(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "list_targets" => Ok(text(self.list_targets())),
            "start" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let build = args["build"].as_bool().unwrap_or(false);
                self.start(target, build)?;
                Ok(text(format!("started {target}")))
            }
            "stop" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                self.adapters.remove(target);
                Ok(text(format!("stopped {target}")))
            }
            "screenshot" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let (rgba, w, h) = self.capture(target)?;
                let image = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
                    .ok_or("invalid image size")?;
                let mut png = Vec::new();
                image
                    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .map_err(|e| e.to_string())?;
                let data = base64::engine::general_purpose::STANDARD.encode(png);
                Ok(json!([{"type": "image", "data": data, "mimeType": "image/png"}]))
            }
            "tap" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let x = number(args, "x")?;
                let y = number(args, "y")?;
                let adapter = self.adapter(target)?;
                adapter.touch(x, y, true);
                std::thread::sleep(Duration::from_millis(60));
                adapter.touch(x, y, false);
                Ok(text(format!("tapped {x:.3},{y:.3} on {target}")))
            }
            "swipe" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let x1 = number(args, "x1")?;
                let y1 = number(args, "y1")?;
                let x2 = number(args, "x2")?;
                let y2 = number(args, "y2")?;
                self.adapter(target)?.swipe(x1, y1, x2, y2);
                Ok(text(format!("swiped on {target}")))
            }
            "type_text" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let input = args["text"].as_str().ok_or("text is required")?;
                let adapter = self.adapter(target)?;
                for c in input.chars() {
                    adapter.char(c, false, false);
                    std::thread::sleep(Duration::from_millis(12));
                }
                Ok(text(format!("typed on {target}")))
            }
            "key" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let key = parse_key(args["key"].as_str().ok_or("key is required")?)?;
                self.adapter(target)?.special(key);
                Ok(text(format!("pressed {key:?} on {target}")))
            }
            "action" => {
                let target = args["target"].as_str().ok_or("target is required")?;
                let action = parse_action(args["action"].as_str().ok_or("action is required")?)?;
                self.adapter(target)?.action(action);
                Ok(text(format!("{action:?} on {target}")))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }

    fn list_targets(&self) -> String {
        let mut lines = Vec::new();
        for (name, target) in &self.cfg.targets {
            let running = self.adapters.contains_key(name);
            lines.push(format!(
                "{name}\t{}\t{}\t{}",
                target.adapter,
                target.device.clone().unwrap_or_default(),
                if running { "running" } else { "stopped" }
            ));
        }
        lines.join("\n")
    }

    fn start(&mut self, name: &str, build: bool) -> Result<(), String> {
        if self.adapters.contains_key(name) {
            return Ok(());
        }
        let target = self
            .cfg
            .targets
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no target named {name}"))?;
        let (adapter, _label) = make_target(name, &target, &self.root, build)?;
        self.adapters.insert(name.to_string(), adapter);
        Ok(())
    }

    fn adapter(&mut self, name: &str) -> Result<&mut Box<dyn Target>, String> {
        self.adapters
            .get_mut(name)
            .ok_or_else(|| format!("{name} is not running; call start first"))
    }

    fn capture(&mut self, name: &str) -> Result<(Vec<u8>, usize, usize), String> {
        let (sw, sh) = {
            let adapter = self.adapter(name)?;
            adapter.size()
        };
        if sw == 0 || sh == 0 {
            return Err(format!("{name} has no size"));
        }
        let width = sw.clamp(1, 1280);
        let height = ((sh as f64 * width as f64 / sw as f64).round() as usize).clamp(1, 1600);
        let mut fresh = None;
        {
            let adapter = self.adapter(name)?;
            for _ in 0..20 {
                if let Some(rgba) = adapter.frame_rgba(width, height) {
                    fresh = Some(rgba);
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        if let Some(rgba) = fresh {
            self.last.insert(name.to_string(), (rgba, width, height));
        }
        self.last
            .get(name)
            .cloned()
            .ok_or_else(|| format!("{name} has no frame yet"))
    }
}

fn number(args: &Value, key: &str) -> Result<f64, String> {
    args[key]
        .as_f64()
        .ok_or_else(|| format!("{key} must be a number"))
}

fn text(value: String) -> Value {
    json!([{"type": "text", "text": value}])
}

fn parse_key(key: &str) -> Result<SpecialKey, String> {
    Ok(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => SpecialKey::Enter,
        "escape" | "esc" => SpecialKey::Esc,
        "backspace" => SpecialKey::Backspace,
        "tab" => SpecialKey::Tab,
        "delete" => SpecialKey::Delete,
        "up" | "arrowup" => SpecialKey::Up,
        "down" | "arrowdown" => SpecialKey::Down,
        "left" | "arrowleft" => SpecialKey::Left,
        "right" | "arrowright" => SpecialKey::Right,
        "home" => SpecialKey::Home,
        "end" => SpecialKey::End,
        "pageup" => SpecialKey::PageUp,
        "pagedown" => SpecialKey::PageDown,
        other => return Err(format!("unknown key {other}")),
    })
}

fn parse_action(action: &str) -> Result<Action, String> {
    Ok(match action.to_ascii_lowercase().as_str() {
        "home" => Action::Home,
        "switcher" | "app_switcher" => Action::Switcher,
        "back" => Action::Back,
        "lock" => Action::Lock,
        "center" | "control_center" => Action::Center,
        "notify" | "notification_center" => Action::Notify,
        "volume_up" => Action::VolUp,
        "volume_down" => Action::VolDown,
        other => return Err(format!("unknown action {other}")),
    })
}

fn tools() -> Vec<Value> {
    let target = json!({"type": "string", "description": "Target name from the config"});
    vec![
        json!({
            "name": "list_targets",
            "description": "List configured targets and whether each is running.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "start",
            "description": "Open a target. The app should already be installed; set build to run the configured build command.",
            "inputSchema": {"type": "object", "properties": {"target": target, "build": {"type": "boolean", "default": false}}, "required": ["target"]}
        }),
        json!({
            "name": "stop",
            "description": "Close a target.",
            "inputSchema": {"type": "object", "properties": {"target": target}, "required": ["target"]}
        }),
        json!({
            "name": "screenshot",
            "description": "Return the current screen as a PNG image.",
            "inputSchema": {"type": "object", "properties": {"target": target}, "required": ["target"]}
        }),
        json!({
            "name": "tap",
            "description": "Tap at normalized coordinates (0..1).",
            "inputSchema": {"type": "object", "properties": {"target": target, "x": {"type": "number"}, "y": {"type": "number"}}, "required": ["target", "x", "y"]}
        }),
        json!({
            "name": "swipe",
            "description": "Swipe between normalized coordinates.",
            "inputSchema": {"type": "object", "properties": {"target": target, "x1": {"type": "number"}, "y1": {"type": "number"}, "x2": {"type": "number"}, "y2": {"type": "number"}}, "required": ["target", "x1", "y1", "x2", "y2"]}
        }),
        json!({
            "name": "type_text",
            "description": "Type text into the focused field.",
            "inputSchema": {"type": "object", "properties": {"target": target, "text": {"type": "string"}}, "required": ["target", "text"]}
        }),
        json!({
            "name": "key",
            "description": "Press a special key: enter, escape, backspace, tab, delete, up, down, left, right, home, end, pageup, pagedown.",
            "inputSchema": {"type": "object", "properties": {"target": target, "key": {"type": "string"}}, "required": ["target", "key"]}
        }),
        json!({
            "name": "action",
            "description": "Run a platform action: home, switcher, back, lock, center, notify, volume_up, volume_down.",
            "inputSchema": {"type": "object", "properties": {"target": target, "action": {"type": "string"}}, "required": ["target", "action"]}
        }),
    ]
}
