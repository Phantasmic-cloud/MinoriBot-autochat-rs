use crate::config::Config;
use chrono::Local;

const LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARNING", "ERROR"];

fn enabled(level: &str) -> bool {
    let cur = Config::global().str_or("log_level", "INFO").to_ascii_uppercase();
    let cur_i = LEVELS.iter().position(|l| *l == cur.as_str()).unwrap_or(1);
    let want_i = LEVELS.iter().position(|l| *l == level).unwrap_or(1);
    cur_i <= want_i
}

fn emit(level: &str, msg: &str) {
    if !enabled(level) {
        return;
    }
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    println!("[{ts}][{level}] {msg}");
}

pub fn debug(msg: impl AsRef<str>) {
    emit("DEBUG", msg.as_ref());
}

pub fn info(msg: impl AsRef<str>) {
    emit("INFO", msg.as_ref());
}

pub fn warning(msg: impl AsRef<str>) {
    emit("WARNING", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    emit("ERROR", msg.as_ref());
}

pub fn error_with_trace(msg: impl AsRef<str>, err: &impl std::fmt::Display) {
    emit("ERROR", &format!("{}: {}", msg.as_ref(), err));
}

pub fn exc_desc(err: &anyhow::Error) -> String {
    let s = format!("{err:#}");
    let et = err
        .downcast_ref::<crate::rpc::RpcError>()
        .map(|e| e.to_string())
        .unwrap_or_else(|| s.clone());
    if et.is_empty() {
        s
    } else {
        et
    }
}

pub fn any_desc(err: &impl std::fmt::Display) -> String {
    err.to_string()
}