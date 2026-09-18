use crate::config::Config;
use crate::log;
use crate::rpc::RpcSession;
use crate::util::{cosine_similarity, json_f64_vec};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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

fn job_flag() -> &'static AtomicBool {
    static F: OnceLock<AtomicBool> = OnceLock::new();
    F.get_or_init(|| AtomicBool::new(false))
}

fn sticker_job_running() -> bool {
    job_flag().load(Ordering::SeqCst)
}

fn spawn_sticker_job<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if job_flag().swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        fut.await;
        job_flag().store(false, Ordering::SeqCst);
    });
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

fn abspath(p: &str) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_string_lossy().to_string()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p).to_string_lossy().to_string())
            .unwrap_or_else(|_| p.to_string())
    }
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

fn read_stk_emb_file() -> Option<EmbDbFile> {
    let file = match fs::File::open(STK_EMB_DB_PATH) {
        Ok(f) => f,
        Err(_) => return None,
    };
    match serde_json::from_reader(BufReader::new(file)) {
        Ok(db) => Some(db),
        Err(e) => {
            log::warning(format!("读取stk_emb_db.json失败，重新建立: {e}"));
            None
        }
    }
}

fn emb_db_usable(model: &str) -> (bool, HashMap<String, Vec<f32>>) {
    match read_stk_emb_file() {
        None => (false, HashMap::new()),
        Some(db) if db.emb_model != model => {
            log::info(format!(
                "Embedding模型已变更({} -> {model})，清空向量库",
                db.emb_model
            ));
            (false, HashMap::new())
        }
        Some(db) => (true, db.embeddings),
    }
}

fn save_emb_db(model: &str, embeddings: &HashMap<String, Vec<f32>>) {
    if let Some(parent) = Path::new(STK_EMB_DB_PATH).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let refs: HashMap<String, &Vec<f32>> = embeddings.iter().map(|(k, v)| (k.clone(), v)).collect();
    let tmp = format!("{STK_EMB_DB_PATH}.tmp");
    let ok = (|| -> anyhow::Result<()> {
        let file = fs::File::create(&tmp)?;
        let mut w = BufWriter::new(file);
        serde_json::to_writer(
            &mut w,
            &EmbDbSave {
                emb_model: model,
                embeddings: refs,
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

fn collect_sid_texts() -> anyhow::Result<(Vec<(i64, String, String)>, f64)> {
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
                .map(abspath)
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
    Ok((sid_texts, mtime))
}

fn apply_sticker_cache(
    sid_texts: &[(i64, String, String)],
    emb_db: &HashMap<String, Vec<f32>>,
    mtime: f64,
) {
    let mut cache = Vec::new();
    for (sid, text, path) in sid_texts {
        let k = format!("{sid}:{text}");
        let ek = format!("e:{k}");
        if let (Some(full), Some(emo)) = (emb_db.get(&k), emb_db.get(&ek)) {
            cache.push(CacheItem {
                sid: *sid,
                text: text.clone(),
                path: path.clone(),
                full_emb: full.clone(),
                emotion_emb: emo.clone(),
            });
        }
    }
    let mut st = state().lock();
    st.cache = cache;
    st.cache_mtime = mtime;
}

fn missing_indices(sid_texts: &[(i64, String, String)], emb_db: &HashMap<String, Vec<f32>>) -> Vec<usize> {
    sid_texts
        .iter()
        .enumerate()
        .filter(|(_, (sid, text, _))| {
            let k = format!("{sid}:{text}");
            !emb_db.contains_key(&k) || !emb_db.contains_key(&format!("e:{k}"))
        })
        .map(|(i, _)| i)
        .collect()
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

async fn fill_missing_embeddings(
    rpc: &RpcSession,
    model: &str,
    sid_texts: &[(i64, String, String)],
    emb_db: &mut HashMap<String, Vec<f32>>,
) {
    let caption_texts: Vec<String> = sid_texts.iter().map(|(_, t, _)| t.clone()).collect();
    let emotion_texts: Vec<String> = caption_texts
        .iter()
        .map(|t| t.split(',').next().unwrap_or("").to_string())
        .collect();
    let keys: Vec<String> = sid_texts
        .iter()
        .map(|(sid, text, _)| format!("{sid}:{text}"))
        .collect();
    let missing = missing_indices(sid_texts, emb_db);
    if missing.is_empty() {
        return;
    }
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

fn commit_sticker_cache(
    model: &str,
    sid_texts: &[(i64, String, String)],
    emb_db: HashMap<String, Vec<f32>>,
    mtime: f64,
) {
    let used: HashSet<String> = sid_texts
        .iter()
        .flat_map(|(sid, text, _)| {
            let k = format!("{sid}:{text}");
            [k.clone(), format!("e:{k}")]
        })
        .collect();
    let new_emb_db: HashMap<_, _> = emb_db.into_iter().filter(|(k, _)| used.contains(k)).collect();
    apply_sticker_cache(sid_texts, &new_emb_db, mtime);
    save_emb_db(model, &new_emb_db);
    let n = state().lock().cache.len();
    log::info(format!("Sticker缓存完成，共{n}条向量"));
}

async fn sticker_job_rebuild(rpc: RpcSession, model: String) {
    log::info("开始后台重建表情包向量库");
    let (sid_texts, mtime) = match collect_sid_texts() {
        Ok(v) => v,
        Err(e) => {
            log::warning(format!("表情包向量库重建失败: {e}"));
            return;
        }
    };
    if sid_texts.is_empty() {
        apply_sticker_cache(&[], &HashMap::new(), mtime);
        return;
    }
    let mut emb_db = HashMap::new();
    fill_missing_embeddings(&rpc, &model, &sid_texts, &mut emb_db).await;
    commit_sticker_cache(&model, &sid_texts, emb_db, mtime);
}

async fn sticker_job_fill(rpc: RpcSession, model: String) {
    let (sid_texts, mtime) = match collect_sid_texts() {
        Ok(v) => v,
        Err(e) => {
            log::warning(format!("表情包向量补全失败: {e}"));
            return;
        }
    };
    let (ok, mut emb_db) = emb_db_usable(&model);
    if !ok {
        emb_db = HashMap::new();
    }
    if sid_texts.is_empty() {
        apply_sticker_cache(&[], &HashMap::new(), mtime);
        return;
    }
    if missing_indices(&sid_texts, &emb_db).is_empty() {
        return;
    }
    log::info("开始后台补全缺失表情包向量");
    fill_missing_embeddings(&rpc, &model, &sid_texts, &mut emb_db).await;
    commit_sticker_cache(&model, &sid_texts, emb_db, mtime);
}

fn load_memory_from_disk(model: &str) -> bool {
    let (ok, emb_db) = emb_db_usable(model);
    if !ok {
        return false;
    }
    if !state().lock().cache.is_empty() {
        return true;
    }
    let (sid_texts, mtime) = match collect_sid_texts() {
        Ok(v) => v,
        Err(_) => return false,
    };
    apply_sticker_cache(&sid_texts, &emb_db, mtime);
    log::info(format!(
        "已从磁盘加载表情包向量，共{}条",
        state().lock().cache.len()
    ));
    true
}

pub async fn prepare_sticker_search(rpc: &RpcSession) -> bool {
    if sticker_job_running() {
        log::info("表情包向量任务进行中，本轮跳过发送");
        return false;
    }
    let model = Config::global().str("chat.sticker.emb_model");
    if !Path::new(STK_DB_PATH).exists() {
        return false;
    }
    if load_memory_from_disk(&model) {
        return true;
    }
    log::info("表情包向量库缺失或模型不匹配，后台重建，本轮跳过发送");
    let rpc = rpc.clone();
    spawn_sticker_job(async move {
        sticker_job_rebuild(rpc, model).await;
    });
    false
}

pub fn schedule_sticker_fill(rpc: RpcSession) {
    if sticker_job_running() {
        return;
    }
    let model = Config::global().str("chat.sticker.emb_model");
    let (ok, emb_db) = emb_db_usable(&model);
    if !ok {
        return;
    }
    let sid_texts = match collect_sid_texts() {
        Ok((v, _)) => v,
        Err(_) => return,
    };
    if missing_indices(&sid_texts, &emb_db).is_empty() {
        return;
    }
    spawn_sticker_job(async move {
        sticker_job_fill(rpc, model).await;
    });
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
        if state().lock().cache.is_empty() {
            return Ok::<StickerHit, anyhow::Error>(StickerHit::none());
        }
        let query_embs =
            parse_embeddings(rpc, vec![query_text.clone(), sticker_emotion.clone()], &model).await?;
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
