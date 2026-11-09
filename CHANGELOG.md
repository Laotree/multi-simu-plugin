# Changelog

## 0.1.1

- Web adapter: apply the device metrics before navigation, enable focus
  emulation, and start the screencast only after the load event. This fixes
  NSH web apps that failed to load their browser facade.
- Web adapter: launch Chrome with `--disable-gpu`, a desktop-sized 1280x800
  viewport, and a fixed window for the pane.

## 0.1.0

- iOS adapter: headless CoreSimulator IOSurface frames and Indigo HID input.
- Android adapter: emulator gRPC screenshot stream and adb input.
- Web adapter: headless Chrome DevTools screencast and input.
- Terminal UI: Kitty graphics, Unicode fallback, toolbar, keyboard shortcuts,
  click markers, and a local click log.
- Configuration: per-project TOML with build commands and target settings.
