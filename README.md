# MinoriBot autochat-rs

Python 版 `services/autochat` 的行为兼容实现。配置、数据目录、RPC 契约都不变，关 py 开这个二进制即可接着跑。

预编译包在 [Releases](https://github.com/Phantasmic-cloud/MinoriBot-autochat-rs/releases)：

| 文件 | 平台 |
| --- | --- |
| `autochat-rs-windows-amd64.exe` | Windows x86_64 |
| `autochat-rs-linux-amd64` | Linux x86_64 |
| `autochat-rs-macos-arm64` | macOS Apple Silicon |

版本跟 git tag 走（`v0.1.0` 这种），和 `Cargo.toml` 的 `version` 对齐。打 `v*` tag 会编三个目标并挂到对应 Release。

## 运行

工作目录必须是 MinoriBot 仓库根（二进制会自己往上找 `config/chat/autochat.yaml`，也认 `MINORIBOT_ROOT`）。主程序要先起来，RPC 端口对上。

```bash
./autochat-rs-linux-amd64
```

从源码编：

```bash
cargo build --release
./target/release/autochat-rs
```
