# Contributing

Thanks for helping improve multi-simu.

## Development

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The iOS adapter needs Xcode and macOS. The web and Android adapters compile on
other platforms, and the core builds without the iOS feature:

```sh
cargo build --no-default-features
```

## Rules

- Keep the iOS private-API code isolated behind the `ios` feature.
- Do not add Apple binaries, Xcode assets, credentials, or personal paths.
- Add a test for protocol parsing, layout math, or configuration changes.
- Keep commits focused; describe what changed and why.
- Never claim support for an Xcode, emulator, or browser version you did not
  test.

## License

By contributing, you agree that your contributions are licensed under the
project's MIT OR Apache-2.0 terms.
