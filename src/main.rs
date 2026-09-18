mod actions;
mod chat;
mod config;
mod filedb;
mod format;
mod log;
mod memory;
mod rpc;
mod sticker;
mod types;
mod util;

use crate::config::Config;
use crate::rpc::RpcSession;
use crate::types::AppState;
use std::env;
use std::path::{Path, PathBuf};

fn find_root() -> PathBuf {
    if let Ok(p) = env::var("MINORIBOT_ROOT") {
        return PathBuf::from(p);
    }
    let mut cur = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for _ in 0..8 {
        if cur.join("config/chat/autochat.yaml").is_file() {
            return cur;
        }
        if !cur.pop() {
            break;
        }
    }
    // binary lives in services/autochat-rs/target/...
    if let Ok(exe) = env::current_exe() {
        let mut p = exe;
        for _ in 0..10 {
            if p.join("config/chat/autochat.yaml").is_file() {
                return p;
            }
            if !p.pop() {
                break;
            }
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[tokio::main]
async fn main() {
    let root = find_root();
    if let Err(e) = env::set_current_dir(&root) {
        eprintln!("无法切换到仓库根目录 {}: {e}", root.display());
        std::process::exit(1);
    }
    let cfg_path = Path::new("config/chat/autochat.yaml");
    if !cfg_path.exists() {
        eprintln!("找不到配置文件 {}", cfg_path.display());
        std::process::exit(1);
    }
    Config::load(cfg_path.to_path_buf()).set_global();

    let rpc = RpcSession::new();
    let state = AppState::new(rpc.clone());
    tokio::spawn(rpc.run());
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    chat::run_loop(state).await;
}