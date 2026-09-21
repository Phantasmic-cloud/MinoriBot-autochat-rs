use crate::config::{yaml_to_json, Config};
use crate::log;
use crate::rpc::RpcSession;
use crate::util::truncate;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

const VOICE_DB_PATH: &str = "data/chat/autochat/voice_db.json";

#[derive(Clone, Debug)]
struct VoiceClip {
    vid: i64,
    path: String,
    tag: String,
}

struct VoiceState {
    clips: Vec<VoiceClip>,
    mtime: Option<f64>,
}

fn state() -> &'static Mutex<VoiceState> {
    static S: OnceLock<Mutex<VoiceState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(VoiceState {
            clips: Vec::new(),
            mtime: None,
        })
    })
}

#[derive(Clone, Debug)]
pub enum VoiceHit {
    Skip,
    File(String),
    Text(String),
}

fn file_mtime(path: &str) -> Option<f64> {
    let meta = fs::metadata(path).ok()?;
    let dur = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_secs() as f64 + f64::from(dur.subsec_nanos()) / 1e9)
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\u{4e00}-\u{9fff}A-Za-z0-9]+").unwrap())
}

pub fn plain_tts_text(text: &str) -> String {
    let at_re = Regex::new(r"\[@\d+\]").unwrap();
    let reply_re = Regex::new(r"\[reply=-?\d+\]").unwrap();
    let mut out = at_re.replace_all(text, "").into_owned();
    out = reply_re.replace_all(&out, "").into_owned();
    out = out.replace('\r', " ").replace('\n', " ");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn match_chars(text: &str) -> String {
    token_re()
        .find_iter(text)
        .map(|m| m.as_str())
        .collect::<String>()
}

fn jaccard(a: &str, b: &str) -> f64 {
    let sa: std::collections::HashSet<char> = a.chars().collect();
    let sb: std::collections::HashSet<char> = b.chars().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn len_ratio(a: &str, b: &str) -> f64 {
    let la = a.chars().count();
    let lb = b.chars().count();
    if la == 0 || lb == 0 {
        0.0
    } else {
        la.min(lb) as f64 / la.max(lb) as f64
    }
}

fn load_voice_clips() -> Vec<VoiceClip> {
    let mut st = state().lock();
    let mtime = match file_mtime(VOICE_DB_PATH) {
        Some(t) => t,
        None => {
            if Path::new(VOICE_DB_PATH).exists() {
                log::warning("读取语音库失败: 无法获取 mtime");
                return st.clips.clone();
            }
            st.clips.clear();
            st.mtime = None;
            return Vec::new();
        }
    };
    if st.mtime == Some(mtime) {
        return st.clips.clone();
    }
    let text = match fs::read_to_string(VOICE_DB_PATH) {
        Ok(t) => t,
        Err(e) => {
            log::warning(format!("读取语音库失败: {e}"));
            return st.clips.clone();
        }
    };
    let db: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            log::warning(format!("读取语音库失败: {e}"));
            return st.clips.clone();
        }
    };
    let mut clips = Vec::new();
    if let Some(map) = db.get("voices").and_then(|v| v.as_object()) {
        for v in map.values() {
            let path = v
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let tag = v
                .get("tag")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if tag.is_empty() || path.is_empty() {
                continue;
            }
            let abs = match fs::canonicalize(&path) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(_) => {
                    if Path::new(&path).is_file() {
                        path
                    } else {
                        continue;
                    }
                }
            };
            if !Path::new(&abs).is_file() {
                continue;
            }
            let vid = v
                .get("vid")
                .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64)))
                .unwrap_or(0);
            clips.push(VoiceClip {
                vid,
                path: abs,
                tag,
            });
        }
    }
    log::info(format!("已加载语音库 {} 条", clips.len()));
    st.clips = clips.clone();
    st.mtime = Some(mtime);
    clips
}

fn coarse_voice_candidates(text: &str, clips: &[VoiceClip]) -> Vec<VoiceClip> {
    let query = match_chars(text);
    if query.is_empty() {
        return Vec::new();
    }
    let cfg = Config::global();
    let limit = cfg.usize_or("chat.voice.candidate_limit", 40).max(1);
    let min_jaccard = cfg.f64_or("chat.voice.min_jaccard", 0.15);
    let min_len_ratio = cfg.f64_or("chat.voice.min_len_ratio", 0.5);
    let mut scored: Vec<(f64, f64, VoiceClip)> = Vec::new();
    for clip in clips {
        let tag_chars = match_chars(&clip.tag);
        let jac = jaccard(&query, &tag_chars);
        let ratio = len_ratio(&query, &tag_chars);
        if jac < min_jaccard && ratio < min_len_ratio {
            continue;
        }
        scored.push((jac, ratio, clip.clone()));
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    scored.into_iter().take(limit).map(|(_, _, c)| c).collect()
}

fn parse_match_index(raw: &str, n: usize) -> usize {
    let re = Regex::new(r"-?\d+").unwrap();
    let m = match re.find(raw) {
        Some(m) => m.as_str(),
        None => return 0,
    };
    match m.parse::<i64>() {
        Ok(idx) if idx >= 1 && (idx as usize) <= n => idx as usize,
        _ => 0,
    }
}

fn tts_model_configured() -> bool {
    !Config::global().str("chat.voice.tts_model").trim().is_empty()
}

async fn llm_pick_voice(rpc: &RpcSession, text: &str, candidates: &[VoiceClip]) -> Option<VoiceClip> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        let query = match_chars(text);
        let tag_chars = match_chars(&candidates[0].tag);
        if jaccard(&query, &tag_chars) >= 0.8 {
            return Some(candidates[0].clone());
        }
    }
    let cfg = Config::global();
    let models = yaml_to_json(&cfg.get("chat.voice.model"));
    let prompt_tpl = cfg.str("chat.voice.prompt");
    if models.is_null()
        || models.as_array().is_some_and(|a| a.is_empty())
        || prompt_tpl.trim().is_empty()
    {
        return None;
    }
    let lines = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {}", i + 1, c.tag))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = prompt_tpl.replace("{text}", text).replace("{candidates}", &lines);
    let timeout = cfg.f64_or("chat.voice.match_timeout", 8.0);
    let max_tokens = cfg.i64_or("chat.voice.max_tokens", 64);
    let options = json!({
        "timeout": timeout,
        "max_tokens": max_tokens,
    });
    match rpc.query_llm(models, &prompt, vec![], options).await {
        Ok(resp) => {
            let raw = match &resp {
                Value::String(s) => s.clone(),
                other => other.as_str().unwrap_or(&other.to_string()).to_string(),
            };
            let raw = raw.trim();
            let idx = parse_match_index(raw, candidates.len());
            if idx > 0 {
                let hit = candidates[idx - 1].clone();
                log::info(format!(
                    "语音匹配选中 vid={} tag={} raw={}",
                    hit.vid,
                    hit.tag,
                    truncate(raw, 32)
                ));
                Some(hit)
            } else {
                log::info(format!("语音匹配无合适条目 raw={}", truncate(raw, 32)));
                None
            }
        }
        Err(e) => {
            log::warning(format!("语音匹配小模型失败: {e}"));
            None
        }
    }
}

pub async fn resolve_voice_action(rpc: &RpcSession, text: &str) -> VoiceHit {
    let text = plain_tts_text(text);
    if text.is_empty() {
        return VoiceHit::Skip;
    }
    let clips = load_voice_clips();
    if let Some(exact) = clips.iter().find(|c| c.tag == text) {
        return VoiceHit::File(exact.path.clone());
    }
    let candidates = coarse_voice_candidates(&text, &clips);
    if let Some(pick) = llm_pick_voice(rpc, &text, &candidates).await {
        return VoiceHit::File(pick.path);
    }
    if !tts_model_configured() {
        return VoiceHit::Text(text);
    }
    let timeout = Config::global().f64_or("chat.voice.timeout", 15.0) + 5.0;
    match rpc.synth_tts(&text, timeout).await {
        Ok(ret) => {
            let path = ret
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if path.is_empty() || !Path::new(&path).is_file() {
                log::warning("合成语音未返回文件");
                VoiceHit::Text(text)
            } else {
                VoiceHit::File(path)
            }
        }
        Err(e) => {
            log::warning(format!("合成语音失败: {e}"));
            VoiceHit::Text(text)
        }
    }
}

pub async fn prefetch_voice(rpc: &RpcSession, text: &str) -> VoiceHit {
    let timeout = Config::global().f64_or("chat.voice.timeout", 15.0);
    let match_timeout = Config::global().f64_or("chat.voice.match_timeout", 8.0);
    let wait = timeout + match_timeout + 5.0;
    match tokio::time::timeout(
        Duration::from_secs_f64(wait.max(0.1)),
        resolve_voice_action(rpc, text),
    )
    .await
    {
        Ok(hit) => hit,
        Err(_) => {
            log::warning("语音预取失败，回退文字: 超时");
            let fallback = plain_tts_text(text);
            if fallback.is_empty() {
                VoiceHit::Skip
            } else {
                VoiceHit::Text(fallback)
            }
        }
    }
}
