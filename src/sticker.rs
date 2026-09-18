use crate::config::Config;
use crate::log;
use crate::rpc::RpcSession;
use crate::util::{cosine_similarity, json_f64_vec, truncate};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

const STK_EMB_DB_PATH: &str = "data/chat/autochat/stk_emb_db.json";
const STK_DB_PATH: &str = "data/chat/autochat/sticker_db.json";

struct CacheItem {
    sid: i64,
    text: String,
    path: String,
    full_emb: Vec<f32>,
    emotion_emb: Vec<f32>,
}

struct StickerState {
    cache: Vec<CacheItem>,
    cache_mtime: f64,
    multipliers: HashMap<i64, HashMap<i64, f64>>,
}

impl StickerState {
    fn new() -> Self {
        StickerState {
            cache: Vec::new(),
            cache_mtime: 0.0,
            multipliers: HashMap::new(),
        }
    }
}

fn state() -> &'static Mutex<StickerState> {
    static S: OnceLock<Mutex<StickerState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(StickerState::new()))
}

fn file_mtime(path: &str) -> Option<f64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

fn load_emb_db(model: &str) -> HashMap<String, Vec<f32>> {
    let text = match fs::read_to_string(STK_EMB_DB_PATH) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    let db: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            log::warning(format!("读取stk_emb_db.json失败，重新建立: {e}"));
            return HashMap::new();
        }
    };
    let stored = db.get("emb_model").and_then(|v| v.as_str()).unwrap_or("");
    if stored != model {
        log::info(format!("Embedding模型已变更({stored} -> {model})，清空向量库"));
        return HashMap::new();
    }
    let mut out = HashMap::new();
    if let Some(map) = db.get("embeddings").and_then(|v| v.as_object()) {
        for (k, v) in map {
            if let Some(vec) = json_f64_vec(v) {
                out.insert(k.clone(), vec);
            }
        }
    }
    out
}

fn save_emb_db(model: &str, embeddings: &HashMap<String, Vec<f32>>) {
    if let Some(parent) = Path::new(STK_EMB_DB_PATH).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut obj = serde_json::Map::new();
    for (k, v) in embeddings {
        obj.insert(k.clone(), json!(v));
    }
    let db = json!({"emb_model": model, "embeddings": obj});
    if let Ok(s) = serde_json::to_string(&db) {
        let _ = fs::write(STK_EMB_DB_PATH, s);
    }
}

pub fn get_sticker_multiplier(group_id: i64, sid: i64) -> f64 {
    state()
        .lock()
        .multipliers
        .get(&group_id)
        .and_then(|m| m.get(&sid))
        .copied()
        .unwrap_or(1.0)
}

pub fn update_sticker_multipliers(group_id: i64, sent_sid: Option<i64>, all_sids: &[i64]) {
    let cfg = Config::global();
    let send_penalty = cfg.f64_or("chat.sticker.send_penalty", 0.8);
    let recover_rate = cfg.f64_or("chat.sticker.recover_rate", 0.2);
    let mut st = state().lock();
    let m = st.multipliers.entry(group_id).or_default();
    for sid in all_sids {
        if Some(*sid) == sent_sid {
            let cur = m.get(sid).copied().unwrap_or(1.0);
            m.insert(*sid, (cur - send_penalty).max(0.0));
        } else {
            let cur = m.get(sid).copied().unwrap_or(1.0);
            m.insert(*sid, (cur + recover_rate).min(1.0));
        }
    }
}

async fn parse_embeddings(rpc: &RpcSession, texts: Vec<String>, model: &str) -> anyhow::Result<Vec<Vec<f32>>> {
    let v = rpc.query_embeddings(texts, model).await?;
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("embedding 返回不是数组"))?;
    let mut out = Vec::new();
    for item in arr {
        out.push(json_f64_vec(item).ok_or_else(|| anyhow::anyhow!("embedding 向量解析失败"))?);
    }
    Ok(out)
}

async fn build_sticker_cache(rpc: &RpcSession, model: &str) -> anyhow::Result<()> {
    let mtime = file_mtime(STK_DB_PATH).unwrap_or(0.0);
    let text = fs::read_to_string(STK_DB_PATH)?;
    let db: Value = serde_json::from_str(&text)?;
    let stickers = db.get("stickers").cloned().unwrap_or(json!({}));
    let mut sid_texts: Vec<(i64, String, String)> = Vec::new();
    if let Some(map) = stickers.as_object() {
        for (sid_str, s) in map {
            let sid: i64 = sid_str.parse().unwrap_or(0);
            let path = s
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| {
                    fs::canonicalize(p)
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_else(|_| {
                            std::env::current_dir()
                                .map(|c| c.join(p).to_string_lossy().to_string())
                                .unwrap_or_else(|_| p.to_string())
                        })
                })
                .unwrap_or_default();
            let captions = s.get("caption").cloned().unwrap_or(json!([]));
            let caps: Vec<Value> = if captions.is_string() {
                vec![json!({"emotion": "", "scene": captions.as_str().unwrap_or("")})]
            } else {
                captions.as_array().cloned().unwrap_or_default()
            };
            for c in caps {
                let emotion = c.get("emotion").and_then(|v| v.as_str()).unwrap_or("");
                let scene = c.get("scene").and_then(|v| v.as_str()).unwrap_or("");
                if !emotion.is_empty() || !scene.is_empty() {
                    let text = if !emotion.is_empty() {
                        format!("{emotion},{scene}")
                    } else {
                        scene.to_string()
                    };
                    sid_texts.push((sid, text, path.clone()));
                }
            }
        }
    }
    if sid_texts.is_empty() {
        let mut st = state().lock();
        st.cache.clear();
        st.cache_mtime = mtime;
        return Ok(());
    }

    let mut emb_db = load_emb_db(model);
    let caption_texts: Vec<String> = sid_texts.iter().map(|(_, t, _)| t.clone()).collect();
    let emotion_texts: Vec<String> = caption_texts
        .iter()
        .map(|t| t.split(',').next().unwrap_or("").to_string())
        .collect();
    let keys: Vec<String> = sid_texts
        .iter()
        .map(|(sid, text, _)| format!("{sid}:{text}"))
        .collect();

    let missing: Vec<usize> = keys
        .iter()
        .enumerate()
        .filter(|(_, k)| !emb_db.contains_key(*k) || !emb_db.contains_key(&format!("e:{k}")))
        .map(|(i, _)| i)
        .collect();

    if !missing.is_empty() {
        let mut success_count = 0usize;
        for batch in missing.chunks(10) {
            let batch_full: Vec<String> = batch.iter().map(|&i| caption_texts[i].clone()).collect();
            let batch_emo: Vec<String> = batch.iter().map(|&i| emotion_texts[i].clone()).collect();
            match (
                parse_embeddings(rpc, batch_full, model).await,
                parse_embeddings(rpc, batch_emo, model).await,
            ) {
                (Ok(full), Ok(emo)) => {
                    for ((i, full_e), emotion_e) in batch.iter().copied().zip(full.into_iter()).zip(emo.into_iter())
                    {
                        emb_db.insert(keys[i].clone(), full_e);
                        emb_db.insert(format!("e:{}", keys[i]), emotion_e);
                    }
                    success_count += batch.len();
                }
                (Err(e), _) | (_, Err(e)) => {
                    log::warning(format!(
                        "Sticker向量批次请求失败，跳过{}条，下次重试: {e}",
                        batch.len()
                    ));
                }
            }
        }
        if success_count > 0 {
            log::info(format!("新增{success_count}/{}条向量", missing.len()));
        }
    }

    let mut cache = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for (i, (sid, text, path)) in sid_texts.into_iter().enumerate() {
        let k = &keys[i];
        let ek = format!("e:{k}");
        used.insert(k.clone());
        used.insert(ek.clone());
        if let (Some(full), Some(emo)) = (emb_db.get(k), emb_db.get(&ek)) {
            cache.push(CacheItem {
                sid,
                text,
                path,
                full_emb: full.clone(),
                emotion_emb: emo.clone(),
            });
        }
    }
    let new_emb_db: HashMap<_, _> = emb_db.into_iter().filter(|(k, _)| used.contains(k)).collect();
    save_emb_db(model, &new_emb_db);

    let mut st = state().lock();
    let is_update = st.cache_mtime > 0.0;
    st.cache = cache;
    st.cache_mtime = mtime;
    log::info(format!(
        "Sticker{}完成，共{}条向量",
        if is_update { "更新" } else { "缓存" },
        st.cache.len()
    ));
    let _ = truncate;
    Ok(())
}

pub struct StickerHit {
    pub path: Option<String>,
    pub sid: Option<i64>,
    pub all_sids: Vec<i64>,
    pub old_multipliers: HashMap<i64, f64>,
}

impl StickerHit {
    pub fn none() -> Self {
        StickerHit {
            path: None,
            sid: None,
            all_sids: vec![],
            old_multipliers: HashMap::new(),
        }
    }
}

pub async fn search_sticker(rpc: &RpcSession, group_id: i64, query: &Value) -> StickerHit {
    let cfg = Config::global();
    let threshold = cfg.f64_or("chat.sticker.similarity_threshold", 0.7);
    let model = cfg.str("chat.sticker.emb_model");
    let sticker_emotion = query.get("emotion").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let sticker_scene = query.get("scene").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let query_text = if !sticker_emotion.is_empty() {
        format!("{sticker_emotion},{sticker_scene}")
    } else {
        sticker_scene.clone()
    };

    let result = async {
        match file_mtime(STK_DB_PATH) {
            Some(mtime) => {
                let need = { state().lock().cache_mtime < mtime };
                if need {
                    build_sticker_cache(rpc, &model).await?;
                }
            }
            None => return Ok::<StickerHit, anyhow::Error>(StickerHit::none()),
        }
        if state().lock().cache.is_empty() {
            return Ok(StickerHit::none());
        }
        let query_embs = parse_embeddings(rpc, vec![query_text.clone(), sticker_emotion.clone()], &model).await?;
        if query_embs.len() < 2 {
            return Ok(StickerHit::none());
        }
        let query_full = &query_embs[0];
        let query_emotion = &query_embs[1];

        let mut sid_best: HashMap<i64, (f64, String)> = HashMap::new();
        {
            let st = state().lock();
            for item in &st.cache {
                let emotion_score = cosine_similarity(query_emotion, &item.emotion_emb);
                if emotion_score < threshold {
                    continue;
                }
                let full_score = cosine_similarity(query_full, &item.full_emb);
                match sid_best.get(&item.sid) {
                    Some((old, _)) if full_score <= *old => {}
                    _ => {
                        sid_best.insert(item.sid, (full_score, item.path.clone()));
                    }
                }
            }
        }
        let all_sids: Vec<i64> = sid_best.keys().copied().collect();
        let old_multipliers: HashMap<i64, f64> = all_sids
            .iter()
            .map(|sid| (*sid, get_sticker_multiplier(group_id, *sid)))
            .collect();
        let mut best_path = None;
        let mut best_score = -1.0f64;
        let mut best_sid = None;
        for (sid, (raw_score, path)) in &sid_best {
            let adjusted = raw_score * old_multipliers.get(sid).copied().unwrap_or(1.0);
            if adjusted > best_score {
                best_score = adjusted;
                best_path = Some(path.clone());
                best_sid = Some(*sid);
            }
        }
        if best_score >= threshold {
            if let Some(sid) = best_sid {
                let raw = sid_best.get(&sid).map(|x| x.0).unwrap_or(0.0);
                let mul = old_multipliers.get(&sid).copied().unwrap_or(1.0);
                log::info(format!(
                    "Sticker: sid={sid}, adjusted={raw:.3}*{mul:.1}={best_score:.3}, query={query_text}"
                ));
            }
            Ok(StickerHit {
                path: best_path,
                sid: best_sid,
                all_sids,
                old_multipliers,
            })
        } else {
            log::info("没有合适的表情包，取消发送");
            update_sticker_multipliers(group_id, None, &all_sids);
            let changed: Vec<String> = all_sids
                .iter()
                .filter_map(|s| {
                    let old = old_multipliers.get(s).copied().unwrap_or(1.0);
                    let new = get_sticker_multiplier(group_id, *s);
                    if (new - old).abs() > 0.001 {
                        Some(format!("sid={s}({old:.1}→{new:.1})"))
                    } else {
                        None
                    }
                })
                .collect();
            if !changed.is_empty() {
                log::info(format!("表情包倍率: {}", changed.join(", ")));
            }
            Ok(StickerHit::none())
        }
    }
    .await;

    match result {
        Ok(h) => h,
        Err(e) => {
            log::warning(format!("Sticker搜索内部失败: {e}"));
            StickerHit::none()
        }
    }
}