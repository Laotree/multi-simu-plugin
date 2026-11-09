# ios-probe

A diagnostic binary for the private CoreSimulator/SimulatorKit interfaces that
the iOS adapter uses: framebuffer lookup, IOSurface capture, and HID injection.

```sh
cargo run -- <simulator-udid>
```

It is not part of the `multi-simu` release binary. It also uses unofficial
Apple interfaces, and Apple does not support it.
