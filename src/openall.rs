//! Open every enabled target in one multi-pane window.
//!
//! Inside Rex this creates a new window with a horizontal split and sets each
//! pane's terminal size. In any other terminal it prints one command per target,
//! ready to run in separate tabs or panes.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::config::Config;

pub fn open_all(config_path: PathBuf, no_build: bool) -> Result<(), String> {
    let (cfg, root) = Config::load(&config_path)?;
    let targets: Vec<(String, crate::config::Target)> = cfg
        .targets
        .iter()
        .filter(|(_, target)| target.enabled)
        .map(|(name, target)| (name.clone(), target.clone()))
        .collect();
    if targets.is_empty() {
        return Err("the config has no enabled targets".into());
    }
    let config_abs = std::fs::canonicalize(&config_path).unwrap_or(config_path);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    let commands: Vec<Vec<String>> = targets
        .iter()
        .map(|(name, _)| {
            let mut command = vec![
                exe.display().to_string(),
                "run".into(),
                "--config".into(),
                config_abs.display().to_string(),
                "--target".into(),
                name.clone(),
                "--no-focus".into(),
            ];
            if no_build {
                command.push("--no-build".into());
            }
            command
        })
        .collect();

    let Ok(session) = std::env::var("REX_SESSION") else {
        println!("Run each target in its own terminal, tab, or pane:");
        for command in &commands {
            println!("  {}", shell_join(command));
        }
        return Ok(());
    };

    let block = std::env::var("REX_BLOCK").unwrap_or_default();
    let size = rex(
        &["block", "call", "-s", &session, "-b", &block, "size"],
        None,
    )?;
    let size: Value = serde_json::from_str(&size).map_err(|e| e.to_string())?;
    let cols = size["columns"].as_u64().unwrap_or(80) as u16;
    let rows = size["rows"].as_u64().unwrap_or(24) as u16;
    let cell_w = size["cell_width_px"].as_u64().unwrap_or(8);
    let cell_h = size["cell_height_px"].as_u64().unwrap_or(16);

    let mut path_prepend = vec![
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
    ];
    if let Ok(home) = std::env::var("HOME") {
        path_prepend.push(format!("{home}/Library/Android/sdk/platform-tools"));
    }

    let nodes: Vec<Value> = targets
        .iter()
        .zip(commands.iter())
        .map(|((name, _), command)| {
            json!({
                "block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": format!("multi-simu {name}"),
                    "options": {
                        "shell": "none",
                        "command": command,
                        "cwd": root.display().to_string(),
                        "path_prepend": path_prepend
                    }
                }
            })
        })
        .collect();

    let document = json!({
        "session_id": session,
        "window_label": "multi-simu",
        "focus": true,
        "layout": layout_tree(nodes)
    });
    let output = rex(
        &["api", "call", "-s", &session, "session.new_window", "-"],
        Some(&document.to_string()),
    )?;
    let output: Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
    let block_ids = output["block_ids"].as_array().cloned().unwrap_or_default();

    let pane_cols = ((cols as usize) / targets.len()).max(1) as u16;
    for id in &block_ids {
        let Some(id) = id.as_str() else { continue };
        let args = json!({
            "columns": pane_cols,
            "rows": rows,
            "cell_width_px": cell_w,
            "cell_height_px": cell_h
        });
        let _ = rex(
            &["block", "call", "-s", &session, "-b", id, "resize", "-"],
            Some(&args.to_string()),
        );
    }

    println!(
        "opened {} panes in window {}",
        block_ids.len(),
        output["window_id"].as_str().unwrap_or("?")
    );
    Ok(())
}

fn rex(args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("rex");
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run rex: {e}"))?;
    if let Some(input) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(input.as_bytes())
                .map_err(|e| e.to_string())?;
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn layout_tree(nodes: Vec<Value>) -> Value {
    let mut node = nodes.last().cloned().unwrap_or_else(|| json!({}));
    for i in (0..nodes.len().saturating_sub(1)).rev() {
        let ratio = 1.0 / (nodes.len() - i) as f64;
        node = json!({
            "split": {
                "direction": "horizontal",
                "ratio": ratio,
                "before": nodes[i],
                "after": node
            }
        });
    }
    node
}

fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|part| {
            if part.contains(' ') {
                format!("'{part}'")
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
