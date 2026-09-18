# MinoriBot autochat-rs

English | [中文](./README_zh.md)

A Rust port of the [MinoriBot](https://github.com/Phantasmic-cloud/MinoriBot) autochat microservice. Same features as the Python version, with better performance and lower memory use. The two can be swapped without changing config or data.

## Requirements

- A running [MinoriBot](https://github.com/Phantasmic-cloud/MinoriBot) main process

## Quick start

Download the binary for your platform from [Releases](https://github.com/Phantasmic-cloud/MinoriBot-autochat-rs/releases):

| File | Platform |
| --- | --- |
| `autochat-rs-windows-amd64.exe` | Windows x86_64 |
| `autochat-rs-linux-amd64` | Linux x86_64 |
| `autochat-rs-macos-arm64` | macOS Apple Silicon |

Place it in `./services/`. Do not run the Rust and Python versions at the same time. Start the main process first.

Linux / macOS:

```bash
cd /path/to/MinoriBot
chmod +x services/autochat-rs-linux-amd64
./services/autochat-rs-linux-amd64
```

Windows: double-click the `.exe`.

## Build from source

Requires a [Rust](https://rustup.rs/) toolchain.

```bash
git clone https://github.com/Phantasmic-cloud/MinoriBot-autochat-rs.git
cd MinoriBot-autochat-rs
cargo build --release
```

## License

[MIT](./LICENSE)