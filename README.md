[![CI](https://github.com/Laotree/multi-simu-plugin/actions/workflows/ci.yml/badge.svg)](https://github.com/Laotree/multi-simu-plugin/actions/workflows/ci.yml)

# multi-simu

**Terminal-native preview and control for iOS simulators, Android emulators,
and web pages — built for remote Macs, CI, and coding agents.**

Run the app you are working on in several places at once, see it in any
terminal, and let an agent drive it through MCP. iOS and Android do not need a
visible desktop window; the iOS path also works while the Mac screen is locked.

> **Unofficial and experimental.** The iOS adapter calls Apple's private
> CoreSimulator and SimulatorKit frameworks. Apple does not support this. It may
> break with Xcode updates. Use it only with simulators and devices you own or
> are authorized to control.

## Status

| Target | Adapter | Status | How it works |
|---|---|---|---|
| iOS simulator | `ios` | Working | CoreSimulator `IOSurface` frames + SimulatorKit Indigo HID. Headless; needs no Simulator.app window. |
| Android emulator | `android` | Working | emulator gRPC `streamScreenshot` + `adb input`. Headless. |
| Web page | `web` | Working | Headless Chrome + DevTools Protocol screencast and input. |
| macOS native window | `window` | Not implemented | Would need ScreenCaptureKit + CGEvent, an unlocked GUI session, and Screen Recording/Accessibility permission. |

Verified on macOS 27 with Xcode 27, Android emulator 35.x, and Chrome stable.

## Requirements

- Rust 1.88 or newer.
- iOS: macOS, Xcode 27 (other versions may work), at least one iOS simulator.
- Android: Android SDK, emulator, `adb`; the emulator must expose its gRPC
  bridge (`-grpc <port>`).
- Web: Google Chrome or Chromium, and `curl`.
- Terminal: Rex, kitty, Ghostty, or WezTerm for Kitty graphics; any 24-bit
  terminal with `--graphics unicode`.

## Install

```sh
git clone https://github.com/Laotree/multi-simu-plugin
cd multi-simu-plugin
cargo build --release
```

The binary is `target/release/multi-simu`. `cargo install --path .` also works.

The iOS adapter is enabled by default. To build without Apple's private
frameworks:

```sh
cargo build --release --no-default-features
```

## Quick start

```sh
cp examples/example.toml my.toml
# edit device names, URLs, and build commands

./target/release/multi-simu list --config my.toml
./target/release/multi-simu devices

./target/release/multi-simu run --config my.toml --target ios
./target/release/multi-simu run --config my.toml --target android
./target/release/multi-simu run --config my.toml --target web
```

Each invocation owns one terminal/pane. Inside Rex, `open-all` creates a new
window with one pane per enabled target:

```sh
./target/release/multi-simu open-all --config my.toml
```

Outside Rex it prints one command per target, ready for separate tabs or tmux
panes.

### Graphics mode

```sh
--graphics auto      # default; picks Kitty for known terminals
--graphics kitty     # force Kitty graphics
--graphics unicode   # half-block characters, works in any 24-bit terminal
```

With SSH or an unfamiliar terminal, start with `--graphics unicode`.

## MCP server

`multi-simu mcp` speaks MCP over stdio, so any MCP-capable agent can drive the
same targets without a terminal UI.

```sh
./target/release/multi-simu mcp --config /abs/path/my.toml
```

Claude Code:

```sh
claude mcp add multi-simu -- /abs/path/multi-simu mcp --config /abs/path/my.toml
```

Generic MCP configuration:

```json
{
  "mcpServers": {
    "multi-simu": {
      "command": "/abs/path/multi-simu",
      "args": ["mcp", "--config", "/abs/path/my.toml"]
    }
  }
}
```

Tools:

| Tool | Arguments | Result |
|---|---|---|
| `list_targets` | none | names, adapters, and running state |
| `start` | `target`, optional `build` | opens the adapter; `build:true` runs the configured build |
| `stop` | `target` | closes the adapter |
| `screenshot` | `target` | current screen as a PNG image |
| `tap` | `target`, `x`, `y` | tap at normalized 0..1 coordinates |
| `swipe` | `target`, `x1`, `y1`, `x2`, `y2` | swipe between normalized points |
| `type_text` | `target`, `text` | type into the focused field |
| `key` | `target`, `key` | enter, escape, backspace, tab, delete, arrows, home, end, pageup, pagedown |
| `action` | `target`, `action` | home, switcher, back, lock, center, notify, volume_up, volume_down |

`start` assumes the app is already installed and launched. Use the `build`
argument, or run the configured build yourself, to install and launch it.

## Configuration

```toml
[project]
root = "."

[targets.ios]
adapter = "ios"
device = "iPhone 17"          # or a simulator UDID
build = "your iOS build and install command"

[targets.android]
adapter = "android"
device = "avd:Your_AVD"       # emulator -list-avds
build = "your Android build, install, and launch command"

[targets.web]
adapter = "web"
url = "http://127.0.0.1:8080"
command = "python3 -m http.server 8080 --directory examples/web"
```

Fields:

- `adapter`: `ios`, `android`, or `web`.
- `device`: iOS simulator name or UDID; Android `avd:<name>`.
- `url`: web page address.
- `command`: optional web development server command. It runs with the target's
  working directory.
- `build`: command run before opening the target. A string runs through
  `/bin/sh -lc`; an array runs directly.
- `cwd`: working directory relative to `project.root`.
- `enabled`: set to `false` to skip a target.

`build` may be a string or an argv array:

```toml
build = ["make", "app"]
```

If the iOS simulator is not running, `run` boots it by name. The Android
adapter starts the emulator with `-grpc 8554` if no matching emulator is
running.

## Terminal controls

The bottom bar is clickable:

```
[Home] [Switcher] [Back] [Lock] [Center] [Notify] [Vol-] [Vol+]   [Exit]
```

Narrow panes keep `[Home]`, `[Switcher]`, `[Back]`, and `[Exit]`. The other
actions stay on the keyboard:

| Key | Action |
|---|---|
| `F1` | Home |
| `F2` | App switcher / recent apps |
| `F3` | Back |
| `F4` | Lock |
| `F5` | Control Center / notification shade |
| `F6` | Notification Center |
| `F7` / `F8` | Volume down / up |
| `F10`, `Ctrl+Q`, `Ctrl+C` | Exit the viewer. The simulator keeps running. |

Clicking the screen sends a tap. Drag to swipe; scroll to scroll. A red `+`
marks each click and the click is appended to `/tmp/multi-simu.log`:

```
click col=74 row=7 x=0.895 y=0.113
```

## How it works

- **iOS**: loads CoreSimulator and SimulatorKit at runtime, scans all
  `com.apple.framebuffer.display` ports for a live `IOSurface`, and samples it.
  Touch, keyboard, and hardware buttons go through
  `SimDeviceLegacyHIDClient` and the Indigo HID message constructors.
- **Android**: streams PNG frames from the emulator's gRPC controller. Input
  uses `adb shell input tap/swipe/keyevent`, which is more reliable than the
  gRPC key path.
- **Web**: launches headless Chrome with a temporary profile, connects to the
  DevTools Protocol page target, and uses `Page.startScreencast`,
  `Input.dispatchMouseEvent`, `Input.insertText`, and `Input.dispatchKeyEvent`.

Rendering happens on a separate thread. If the terminal cannot keep up, frames
are dropped without blocking input.

## Limitations

- macOS native windows are not supported yet. They need an unlocked GUI
  session plus Screen Recording and Accessibility permissions.
- The iOS adapter uses private APIs and is tied to the installed Xcode. Expect
  work when Apple changes those interfaces.
- Physical iOS/Android devices are out of scope.
- The terminal renderer implements Kitty graphics and a Unicode fallback.
  iTerm2 inline images and Sixel are not implemented.
- Mouse reporting uses SGR cell coordinates, not pixel reporting. Mapping is
  approximate when the terminal does not report a cell size.
- A newly created pane with no client owner or desired size can make the server
  use 80x24. Click the pane once, or call the terminal's `resize` method, to
  apply the real pane size. `open-all` sets the size for its panes.
- The Android emulator can show GPU corruption after a sleep/wake cycle.
  Reboot the emulator if that happens.

## Security and privacy

multi-simu runs locally. It does not send screen contents anywhere. It reads
the simulators, emulators, and browsers on the same machine and injects input
into them. Capture and input permissions belong to the application that runs
the binary; grant Screen Recording and Accessibility only to a terminal or host
you trust.

The iOS path is unofficial. Do not use it to bypass device security, and do not
run it against devices you do not own.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
cargo build --no-default-features
```

The Android gRPC bindings are generated from `proto/emulator_controller.proto`
and need `protoc`. The `tools/ios-probe` crate is a standalone diagnostic for
the private iOS interfaces and is not part of the release binary.

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

The Android emulator protocol definitions are derived from the Android Open
Source Project under Apache-2.0; see [NOTICE](NOTICE). Apple's frameworks are
not distributed with this project and are loaded at runtime from the installed
Xcode.
