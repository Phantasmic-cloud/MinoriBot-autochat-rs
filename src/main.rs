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
mod voice;

use crate::config::Config;
use crate::rpc::RpcSession;
use crate::types::AppState;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const AUTOCHAT_LOCK_PATH: &str = "data/chat/autochat/autochat.lock";

fn acquire_autochat_lock() -> File {
    if let Some(parent) = Path::new(AUTOCHAT_LOCK_PATH).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(AUTOCHAT_LOCK_PATH)
        .unwrap_or_else(|e| {
            eprintln!("无法打开 autochat 锁文件 {AUTOCHAT_LOCK_PATH}: {e}");
            std::process::exit(1);
        });
    lock_exclusive_nonblock(&file).unwrap_or_else(|_| {
        eprintln!("已有 autochat 微服务在运行（Python 或 Rust），请先关掉再启动");
        std::process::exit(1);
    });
    let _ = file.set_len(0);
    let mut f = &file;
    let _ = write!(f, "{}", std::process::id());
    file
}

#[cfg(windows)]
fn lock_exclusive_nonblock(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn LockFileEx(
            hfile: *mut core::ffi::c_void,
            dwflags: u32,
            dwreserved: u32,
            nnumberoflocklow: u32,
            nnumberoflockhigh: u32,
            lpoverlapped: *mut u8,
        ) -> i32;
    }
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x00000002;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x00000001;
    let mut overlapped = [0u8; 32];
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as *mut core::ffi::c_void,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            overlapped.as_mut_ptr(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn lock_exclusive_nonblock(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc_flock(fd, 2 | 4) }; // LOCK_EX | LOCK_NB
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, operation: i32) -> i32;
}

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
    let _lock = acquire_autochat_lock();

    let rpc = RpcSession::new();
    let state = AppState::new(rpc.clone());
    tokio::spawn(rpc.run());
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    chat::run_loop(state).await;
}