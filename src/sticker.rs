use crate::config::Config;
use crate::log;
use crate::rpc::RpcSession;
use crate::util::{cosine_similarity, json_f64_vec};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter, Write};
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

#[derive(Deserialize)]
struct EmbDbFile {
    #[serde(default)]
    emb_model: String,
    #[serde(default)]
    embeddings: HashMap<String, Vec<f32>>,
}

#[derive(Serialize)]
struct EmbDbSave<'a> {
    emb_model: &'a str,
    embeddings: HashMap<String, &'a Vec<f32>>,
}

fn load_emb_db(model: &str) -> HashMap<String, Vec<f32>> {
    let file = match fs::File::open(STK_EMB_DB_PATH) {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    let db: EmbDbFile = match serde_json::from_reader(BufReader::new(file)) {
        Ok(v) => v,
        Err(e) => {
            log::warning(format!("读取stk_emb_db.json失败，重新建立: {e}"));
            return HashMap::new();
        }
    };
    if db.emb_model != model {
        log::info(format!(
            "Embedding模型已变更({} -> {model})，清空向量库",
            db.emb_model
        ));
        return HashMap::new();
    }
    db.embeddings
}

fn save_emb_db(model: &str, cache: &[CacheItem]) {
    if let Some(parent) = Path::new(STK_EMB_DB_PATH).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut embeddings = HashMap::with_capacity(cache.len() * 2);
    for item in cache {
        let k = format!("{}:{}", item.sid, item.text);
        embeddings.insert(format!("e:{k}"), &item.emotion_emb);
        embeddings.insert(k, &item.full_emb);
    }
    let tmp = format!("{STK_EMB_DB_PATH}.tmp");
    let ok = (|| -> anyhow::Result<()> {
        let file = fs::File::create(&tmp)?;
        let mut w = BufWriter::new(file);
        serde_json::to_writer(
            &mut w,
            &EmbDbSave {
                emb_model: model,
                embeddings,
            },
        )?;
        w.flush()?;
        fs::rename(&tmp, STK_EMB_DB_PATH)?;
        Ok(())
    })();
    if let Err(e) = ok {
        log::warning(format!("保存stk_emb_db.json失败: {e}"));
        let _ = fs::remove_file(&tmp);
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

    let mut cache = Vec::with_capacity(sid_texts.len());
    for (i, (sid, text, path)) in sid_texts.into_iter().enumerate() {
        let k = &keys[i];
        let ek = format!("e:{k}");
        match (emb_db.remove(k), emb_db.remove(&ek)) {
            (Some(full), Some(emo)) => {
                cache.push(CacheItem {
                    sid,
                    text,
                    path,
                    full_emb: full,
                    emotion_emb: emo,
                });
            }
            _ => {}
        }
    }
    drop(emb_db);
    save_emb_db(model, &cache);

    let mut st = state().lock();
    let is_update = st.cache_mtime > 0.0;
    let n = cache.len();
    st.cache = cache;
    st.cache_mtime = mtime;
    log::info(format!(
        "Sticker{}完成，共{}条向量",
        if is_update { "更新" } else { "缓存" },
        n
    ));
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