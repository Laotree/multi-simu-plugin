mod android;
mod config;
#[cfg(feature = "ios")]
mod ios;
mod mcp;
mod openall;
mod target;
mod terminal;
mod web;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};

use config::Config;
use target::Target;
use terminal::Graphics;

#[derive(Parser)]
#[command(
    name = "multi-simu",
    version,
    about = "Show and drive client targets inside any terminal"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a target and show it in the current terminal
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "auto")]
        graphics: String,
        #[arg(long, default_value_t = 10)]
        fps: u32,
        #[arg(long)]
        no_build: bool,
        #[arg(long)]
        no_focus: bool,
    },
    /// List the targets in a config
    List {
        #[arg(long)]
        config: PathBuf,
    },
    /// List iOS simulators on this Mac
    Devices,
    /// Run an MCP server on stdio for coding agents
    Mcp {
        #[arg(long)]
        config: PathBuf,
    },
    /// Open every enabled target in one multi-pane window
    OpenAll {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        no_build: bool,
    },
}

fn graphics_from(name: &str) -> Result<Graphics, String> {
    match name {
        "kitty" => Ok(Graphics::Kitty),
        "unicode" => Ok(Graphics::Unicode),
        "auto" => {
            let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
            let term = std::env::var("TERM").unwrap_or_default();
            let kitty = matches!(term_program.as_str(), "rex" | "ghostty" | "WezTerm")
                || term.contains("kitty")
                || term.contains("ghostty")
                || term.contains("wezterm");
            Ok(if kitty {
                Graphics::Kitty
            } else {
                Graphics::Unicode
            })
        }
        other => Err(format!(
            "unknown graphics mode {other}; use auto, kitty or unicode"
        )),
    }
}

fn simctl_devices() -> Result<Vec<(String, String, String)>, String> {
    let out = Command::new("xcrun")
        .args(["simctl", "list", "devices", "available", "-j"])
        .output()
        .map_err(|e| format!("cannot run simctl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("cannot parse simctl output: {e}"))?;
    let mut devices = Vec::new();
    if let Some(runtimes) = value.get("devices").and_then(|v| v.as_object()) {
        for (runtime, list) in runtimes {
            if !runtime.contains("iOS") {
                continue;
            }
            if let Some(list) = list.as_array() {
                for d in list {
                    let name = d
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let udid = d
                        .get("udid")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let state = d
                        .get("state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    devices.push((name, udid, state));
                }
            }
        }
    }
    Ok(devices)
}

#[cfg(feature = "ios")]
fn resolve_device(wanted: Option<&str>) -> Result<(String, String), String> {
    let devices = simctl_devices()?;
    let matches = |name: &str, udid: &str| match wanted {
        Some(w) => name == w || udid == w,
        None => name.starts_with("iPhone") || name.starts_with("iPad"),
    };
    let booted = devices
        .iter()
        .find(|(name, udid, state)| matches(name, udid) && state == "Booted");
    if let Some((name, udid, _)) = booted {
        return Ok((name.clone(), udid.clone()));
    }
    let candidate = devices
        .iter()
        .find(|(name, udid, _)| matches(name, udid))
        .ok_or_else(|| match wanted {
            Some(w) => format!("no iOS simulator matches {w}"),
            None => "no iOS simulator is available".to_string(),
        })?;
    let (name, udid, _) = candidate;
    eprintln!("booting {name} ({udid})…");
    let _ = Command::new("xcrun")
        .args(["simctl", "boot", udid])
        .status();
    let status = Command::new("xcrun")
        .args(["simctl", "bootstatus", udid, "-b"])
        .status()
        .map_err(|e| format!("cannot run simctl bootstatus: {e}"))?;
    if !status.success() {
        return Err(format!("{name} did not finish booting"));
    }
    Ok((name.clone(), udid.clone()))
}

fn main() {
    let cli = Cli::parse();
    let result: Result<(), String> = match cli.command {
        Cmd::Run {
            config,
            target,
            graphics,
            fps,
            no_build,
            no_focus,
        } => run(config, target, graphics, fps, no_build, no_focus),
        Cmd::List { config } => list(config),
        Cmd::Devices => match simctl_devices() {
            Ok(devices) => {
                for (name, udid, state) in devices {
                    println!("{name}\t{udid}\t{state}");
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        Cmd::Mcp { config } => mcp::serve(config),
        Cmd::OpenAll { config, no_build } => openall::open_all(config, no_build),
    };
    if let Err(e) = result {
        eprintln!("multi-simu: {e}");
        std::process::exit(1);
    }
}

fn list(path: PathBuf) -> Result<(), String> {
    let (cfg, root) = Config::load(&path)?;
    for (name, target) in cfg.targets.iter().filter(|(_, t)| t.enabled) {
        println!(
            "{name}\t{}\t{}\t{}",
            target.adapter,
            target.device.clone().unwrap_or_default(),
            target.cwd(&root).display()
        );
    }
    Ok(())
}

fn run(
    path: PathBuf,
    wanted: Option<String>,
    graphics: String,
    fps: u32,
    no_build: bool,
    no_focus: bool,
) -> Result<(), String> {
    let graphics = graphics_from(&graphics)?;
    let (cfg, root) = Config::load(&path)?;
    let (name, target) = match wanted {
        Some(name) => {
            let target = cfg
                .targets
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no target named {name}"))?;
            (name, target)
        }
        None => cfg
            .targets
            .iter()
            .find(|(_, t)| t.enabled)
            .map(|(n, t)| (n.clone(), t.clone()))
            .ok_or_else(|| "the config has no enabled target".to_string())?,
    };
    let (mut device, label) = make_target(&name, &target, &root, !no_build)?;
    if !no_focus {
        focus_self();
    }
    terminal::run(&mut *device, &label, graphics, fps)
}

pub(crate) fn run_build(name: &str, target: &config::Target, root: &Path) -> Result<(), String> {
    if let Some((program, args)) = target.build_command() {
        let cwd = target.cwd(root);
        eprintln!("building {name} in {}", cwd.display());
        let output = Command::new(&program)
            .args(&args)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run the build: {e}"))?;
        // The MCP server owns stdout, so build output goes to stderr.
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            return Err(format!(
                "the build failed with exit code {}",
                output.status.code().unwrap_or(-1)
            ));
        }
    }
    Ok(())
}

pub(crate) fn make_target(
    name: &str,
    target: &config::Target,
    root: &Path,
    do_build: bool,
) -> Result<(Box<dyn Target>, String), String> {
    if do_build {
        run_build(name, target, root)?;
    }
    match target.adapter.as_str() {
        "ios" => {
            #[cfg(feature = "ios")]
            {
                let (device_name, udid) = resolve_device(target.device.as_deref())?;
                eprintln!("opening {device_name} ({udid})");
                let sim = ios::Sim::open(&udid)?;
                Ok((Box::new(sim), format!("{name} · {device_name}")))
            }
            #[cfg(not(feature = "ios"))]
            {
                Err(format!(
                    "the {name} target needs the ios feature; build with --features ios"
                ))
            }
        }
        "android" => {
            let device = target
                .device
                .as_deref()
                .ok_or_else(|| format!("the {name} target needs a device"))?;
            let avd = device.strip_prefix("avd:").unwrap_or(device);
            eprintln!("opening Android emulator {avd}");
            let android = android::Android::open(avd)?;
            Ok((Box::new(android), format!("{name} · {avd}")))
        }
        "web" => {
            let url = target
                .url
                .as_deref()
                .ok_or_else(|| format!("the {name} target needs a url"))?;
            let cwd = target.cwd(root);
            eprintln!("opening web target {url}");
            let adapter = web::Web::open(url, target.command.as_deref(), &cwd)?;
            Ok((Box::new(adapter), format!("{name} · {url}")))
        }
        other => Err(format!(
            "the {name} target uses the {other} adapter; this build supports ios, android and web"
        )),
    }
}

fn focus_self() {
    let Ok(session) = std::env::var("REX_SESSION") else {
        return;
    };
    let Ok(block) = std::env::var("REX_BLOCK") else {
        return;
    };
    let _ = Command::new("rex")
        .args(["block", "focus", "-s", &session, &block])
        .status();
}
