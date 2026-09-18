use crate::config::{python_format, Config};
use crate::log;
use crate::rpc::cfg_model;
use crate::types::{poke_person_label, AppState, Message};
use crate::util::{get_readable_datetime, truncate};
use rand::Rng;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;

pub fn debug_mode() -> bool {
    Config::global().str_or("log_level", "INFO").eq_ignore_ascii_case("DEBUG")
}

fn json_msg_to_readable_text(data: &Value) -> String {
    if let Some(raw) = data.get("data").and_then(|d| d.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            if let Some(detail) = parsed.pointer("/meta/detail_1") {
                let title = detail.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let desc = truncate(detail.get("desc").and_then(|v| v.as_str()).unwrap_or(""), 32);
                return format!("[{title}分享:{desc}]");
            }
        }
    }
    if let Some(prompt) = data.get("prompt").and_then(|v| v.as_str()) {
        return format!("[转发消息:{prompt}]");
    }
    "[转发消息]".into()
}

fn json_i64(v: Option<&Value>) -> i64 {
    v.and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_u64().map(|u| u as i64))
            .or_else(|| x.as_f64().map(|f| f as i64))
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    })
    .unwrap_or(0)
}

pub async fn get_image_caption(state: &AppState, data: &Value, use_llm: bool) -> String {
    let summary = data.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let url = data.get("url").cloned().unwrap_or(Value::Null);
    let file_unique = data.get("file_unique").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let sub_type_n = json_i64(data.get("sub_type"));
    let sub_type = if sub_type_n == 0 { "图片" } else { "表情" };
    let fallback = if summary.is_empty() {
        format!("[{sub_type}]")
    } else {
        format!("[{sub_type}:{summary}]")
    };

    log::info(format!(
        "尝试获取图片总结: file_unique={file_unique} subtype={sub_type} url={url} summary={summary}"
    ));
    if !file_unique.is_empty() {
        let cache = state.image_caption_db.get(&file_unique);
        if let Some(c) = cache.as_str() {
            log::info(format!("图片总结命中缓存: {c}"));
            return format!("[{sub_type}:{c}]");
        }
    }
    if !use_llm {
        return fallback;
    }
    let cfg = Config::global();
    let prompt_tpl = cfg.str("image_caption.prompt");
    let mut vars = HashMap::new();
    vars.insert("sub_type", sub_type.to_string());
    let prompt = match python_format(&prompt_tpl, &vars) {
        Ok(p) => p,
        Err(_) => prompt_tpl.replace("{sub_type}", sub_type),
    };
    let images = if url.is_null() { vec![] } else { vec![url.clone()] };
    let options = json!({
        "timeout": cfg.f64_or("image_caption.timeout", 60.0),
        "max_tokens": cfg.i64_or("image_caption.max_tokens", 2048),
    });
    match state
        .rpc
        .query_llm(cfg_model("image_caption.model"), &prompt, images, options)
        .await
    {
        Ok(caption) => {
            let caption = match caption {
                Value::String(s) => s,
                other => other.as_str().unwrap_or("").to_string(),
            };
            if caption.is_empty() {
                log::warning(format!("总结图片 url={url} 失败: 图片总结为空"));
                return fallback;
            }
            log::info(format!("图片总结成功: {caption}"));
            if !file_unique.is_empty() {
                state.image_caption_db.set(&file_unique, json!(caption.clone()));
            }
            format!("[{sub_type}:{caption}]")
        }
        Err(e) => {
            log::warning(format!("总结图片 url={url} 失败: {e}"));
            fallback
        }
    }
}

pub async fn format_msgs(
    state: &AppState,
    msgs: &[Message],
    image_caption_limit: f64,
    image_caption_prob: f64,
    emotion_caption_limit: f64,
    emotion_caption_prob: f64,
    self_id: i64,
) -> String {
    let mut msgs: Vec<Message> = msgs.to_vec();
    msgs.sort_by(|a, b| b.time.partial_cmp(&a.time).unwrap_or(std::cmp::Ordering::Equal));
    let mut texts = Vec::new();
    let mut captioned_images = 0.0f64;
    let mut captioned_emotions = 0.0f64;
    let mut rng = rand::thread_rng();
    for msg in &msgs {
        let mut text = format!(
            "{} [{}] {}({}):\n",
            get_readable_datetime(msg.time, true),
            msg.msg_id,
            msg.nickname,
            msg.user_id
        );
        for seg in &msg.msg {
            let stype = seg.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let sdata = seg.get("data").cloned().unwrap_or(json!({}));
            match stype {
                "poke" => {
                    let from_label = poke_person_label(msg.user_id, &msg.nickname, self_id);
                    let tid = json_i64(sdata.get("target_id"));
                    let tname = sdata
                        .get("target_name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| tid.to_string());
                    let to_label = poke_person_label(tid, &tname, self_id);
                    text = format!(
                        "{} {from_label} 戳了戳 {to_label}",
                        get_readable_datetime(msg.time, true)
                    );
                }
                "text" => {
                    if let Some(t) = sdata.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                "face" => text.push_str("[表情]"),
                "video" => text.push_str("[视频]"),
                "audio" => text.push_str("[音频]"),
                "file" => text.push_str("[文件]"),
                "at" => {
                    let qq = sdata
                        .get("qq")
                        .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
                        .unwrap_or_default();
                    text.push_str(&format!("[@{qq}]"));
                }
                "reply" => {
                    let id = sdata
                        .get("id")
                        .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
                        .unwrap_or_default();
                    text.push_str(&format!("[reply={id}]"));
                }
                "forward" => text.push_str("[转发聊天记录]"),
                "json" => text.push_str(&json_msg_to_readable_text(&sdata)),
                "image" => {
                    let sub = json_i64(sdata.get("sub_type"));
                    if sub == 0 {
                        let use_llm =
                            captioned_images < image_caption_limit && rng.gen::<f64>() < image_caption_prob;
                        text.push_str(&get_image_caption(state, &sdata, use_llm).await);
                        captioned_images += 1.0;
                    } else {
                        let use_llm = captioned_emotions < emotion_caption_limit
                            && rng.gen::<f64>() < emotion_caption_prob;
                        text.push_str(&get_image_caption(state, &sdata, use_llm).await);
                        captioned_emotions += 1.0;
                    }
                }
                _ => {}
            }
        }
        texts.push(text.trim().to_string());
    }
    texts.reverse();
    texts.join("\n")
}

pub async fn generate_summary(state: &AppState, text: &str) -> String {
    log::info(format!("开始生成文本摘要: {}", truncate(text, 20)));
    let cfg = Config::global();
    let tpl = cfg.str("summary.prompt");
    let mut vars = HashMap::new();
    vars.insert("text", text.to_string());
    let prompt = match python_format(&tpl, &vars) {
        Ok(p) => p,
        Err(e) => {
            log::error(format!("生成摘要失败: {e}"));
            return String::new();
        }
    };
    let options = json!({
        "timeout": cfg.f64_or("summary.timeout", 60.0),
        "max_tokens": cfg.i64_or("summary.max_tokens", 10240),
    });
    match state
        .rpc
        .query_llm(cfg_model("summary.model"), &prompt, vec![], options)
        .await
    {
        Ok(summary) => {
            let summary = match summary {
                Value::String(s) => s,
                other => other.as_str().unwrap_or("").to_string(),
            };
            log::info(format!("生成文本摘要成功: {summary}"));
            summary
        }
        Err(e) => {
            log::error(format!("生成摘要失败: {e}"));
            String::new()
        }
    }
}

pub fn maybe_dump_prompt(full_prompt: &str) {
    if !debug_mode() {
        return;
    }
    let path = "sandbox/autochat_prompt.txt";
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, full_prompt);
}