# MinoriBot autochat-rs

[English](./README.md) | 中文

[MinoriBot](https://github.com/Phantasmic-cloud/MinoriBot) 自动聊天微服务的 Rust 实现，功能与 Python 版无异，性能更强、内存占用更少，两者可无缝切换

## 环境

- 已部署并可运行的 [MinoriBot](https://github.com/Phantasmic-cloud/MinoriBot) 主程序

## 快速开始

从 [Releases](https://github.com/Phantasmic-cloud/MinoriBot-autochat-rs/releases) 下载对应平台的预编译文件：

| 文件 | 平台 |
| --- | --- |
| `autochat-rs-windows-amd64.exe` | Windows x86_64 |
| `autochat-rs-linux-amd64` | Linux x86_64 |
| `autochat-rs-macos-arm64` | macOS Apple Silicon |

将文件放到 `./services/`。切勿同时运行 Rust 与 Python 版。主程序需先启动。

Linux / macOS：

```bash
cd /path/to/MinoriBot
chmod +x services/autochat-rs-linux-amd64
./services/autochat-rs-linux-amd64
```

Windows：直接双击即可。


## 从源码编译

需要 [Rust](https://rustup.rs/) 工具链。

```bash
git clone https://github.com/Phantasmic-cloud/MinoriBot-autochat-rs.git
cd MinoriBot-autochat-rs
cargo build --release
```

## 许可证

[MIT](./LICENSE)
