use crate::config::Config;
use crate::filedb::{get_file_db, FileDb};
use crate::log;
use crate::util::{f32_from_le_bytes, f32_to_le_bytes, l2_sq_distance, now_ts};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[allow(dead_code)]
const COLLECTION: &str = "event_memroy";

#[derive(Clone, Debug)]
pub struct EventMemory {
    pub id: String,
    pub text: String,
    pub mem_type: String,
    pub weight: i64,
    pub created_at: f64,
    pub distance: f64,
    pub adjusted_distance: f64,
    pub time_penalty: f64,
}

#[derive(Clone, Debug, Default)]
pub struct UserMemory {
    pub names: Vec<String>,
    pub profile: String,
    pub recent_events: Vec<(f64, String)>,
}

#[derive(Clone, Debug)]
pub struct SelfMemory {
    pub id: String,
    pub time: f64,
    pub text: String,
    pub sticker: String,
}

pub struct MemorySystem {
    group_id: i64,
    file_db: FileDb,
    db_path: PathBuf,
}

impl MemorySystem {
    pub fn new(data_dir: &str, group_id: i64) -> Self {
        let _ = fs::create_dir_all(data_dir);
        let chroma_dir = PathBuf::from(data_dir).join(format!("memory_chromadb_{group_id}"));
        let _ = fs::create_dir_all(&chroma_dir);
        let db_path = chroma_dir.join("chroma.sqlite3");
        {
            let conn = Connection::open(&db_path).expect("open chroma.sqlite3");
            init_schema(&conn);
            import_chroma_if_any(&conn);
        }
        MemorySystem {
            group_id,
            file_db: get_file_db(PathBuf::from(data_dir).join(format!("memory_{group_id}.json"))),
            db_path,
        }
    }

    fn open(&self) -> anyhow::Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        init_schema(&conn);
        Ok(conn)
    }

    fn text_emb_dim(&self) -> usize {
        Config::global().usize_or("text_embed_dim", 4096)
    }

    pub fn em_add(&self, text: &str, embedding: &[f32], initial_weight: i64) -> anyhow::Result<String> {
        let dim = self.text_emb_dim();
        anyhow::ensure!(
            embedding.len() == dim,
            "Embedding维度应为 {dim}，但收到 {}",
            embedding.len()
        );
        let memory_id = uuid::Uuid::new_v4().to_string();
        let created_at = now_ts();
        let blob = f32_to_le_bytes(embedding);
        let conn = self.open()?;
        conn.execute(
            "INSERT INTO event_memroy (id, text, type, weight, created_at, embedding)
             VALUES (?1, ?2, 'short_term', ?3, ?4, ?5)",
            params![memory_id, text, initial_weight, created_at, blob],
        )?;
        log::info(format!("成功添加事件记忆 {memory_id} 内容: \"{text}\""));
        let _ = self.group_id;
        Ok(memory_id)
    }

    pub fn em_increase_weight(&self, memory_id: &str, weight_increase: i64, threshold: i64) -> anyhow::Result<()> {
        let conn = self.open()?;
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT weight, type FROM event_memroy WHERE id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (current_weight, mut mem_type) =
            row.ok_or_else(|| anyhow::anyhow!("未找到ID为 {memory_id} 的事件记忆"))?;
        let new_weight = current_weight + weight_increase;
        log::info(format!("事件记忆 {memory_id} 权重 {current_weight} -> {new_weight}"));
        if new_weight >= threshold && mem_type == "short_term" {
            mem_type = "long_term".into();
            log::info(format!("记忆 {memory_id} 已转换为长期记忆。"));
        }
        conn.execute(
            "UPDATE event_memroy SET weight = ?1, type = ?2 WHERE id = ?3",
            params![new_weight, mem_type, memory_id],
        )?;
        Ok(())
    }

    pub fn em_query(
        &self,
        query_embeddings: &[Vec<f32>],
        n_results: usize,
        memory_type: &str,
        time_decay_rate: f64,
    ) -> anyhow::Result<Vec<EventMemory>> {
        if n_results == 0 || query_embeddings.is_empty() {
            return Ok(vec![]);
        }
        let dim = self.text_emb_dim();
        for emb in query_embeddings {
            anyhow::ensure!(emb.len() == dim, "Embedding维度错误");
        }
        let conn = self.open()?;
        let mut sql = "SELECT id, text, type, weight, created_at, embedding FROM event_memroy".to_string();
        if memory_type == "short_term" || memory_type == "long_term" {
            sql.push_str(" WHERE type = ?1");
        }
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = if memory_type == "short_term" || memory_type == "long_term" {
            stmt.query(params![memory_type])?
        } else {
            stmt.query([])?
        };

        struct Row {
            id: String,
            text: String,
            mem_type: String,
            weight: i64,
            created_at: f64,
            embedding: Vec<f32>,
        }
        let mut all = Vec::new();
        while let Some(r) = rows.next()? {
            let blob: Vec<u8> = r.get(5)?;
            all.push(Row {
                id: r.get(0)?,
                text: r.get(1)?,
                mem_type: r.get(2)?,
                weight: r.get(3)?,
                created_at: r.get(4)?,
                embedding: f32_from_le_bytes(&blob),
            });
        }
        drop(rows);
        drop(stmt);
        drop(conn);

        let now = now_ts();
        let mut unique: HashMap<String, EventMemory> = HashMap::new();
        for q in query_embeddings {
            let mut scored: Vec<(String, f64, f64, f64)> = all
                .iter()
                .map(|row| {
                    let raw = l2_sq_distance(q, &row.embedding);
                    let mut adj = raw;
                    let mut penalty = 0.0;
                    if row.mem_type == "short_term" {
                        let hours = (now - row.created_at) / 3600.0;
                        penalty = hours * time_decay_rate;
                        adj += penalty;
                    }
                    (row.id.clone(), raw, adj, penalty)
                })
                .collect();
            scored.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(n_results * 2);
            for (id, raw, adj, penalty) in scored {
                if let Some(existing) = unique.get_mut(&id) {
                    if adj < existing.adjusted_distance {
                        existing.distance = raw;
                        existing.adjusted_distance = adj;
                        existing.time_penalty = penalty;
                    }
                } else if let Some(row) = all.iter().find(|r| r.id == id) {
                    unique.insert(
                        id.clone(),
                        EventMemory {
                            id: id.clone(),
                            text: row.text.clone(),
                            mem_type: row.mem_type.clone(),
                            weight: row.weight,
                            created_at: row.created_at,
                            distance: raw,
                            adjusted_distance: adj,
                            time_penalty: penalty,
                        },
                    );
                }
            }
        }
        let mut final_results: Vec<EventMemory> = unique.into_values().collect();
        final_results.sort_by(|a, b| {
            a.adjusted_distance
                .partial_cmp(&b.adjusted_distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        final_results.truncate(n_results);
        Ok(final_results)
    }

    pub fn em_forget(&self, forget_time: f64, forget_prob: f64) -> anyhow::Result<()> {
        let conn = self.open()?;
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM event_memroy WHERE type = 'short_term' AND created_at < ?1",
            )?;
            let rows = stmt.query_map(params![forget_time], |r| r.get::<_, String>(0))?;
            rows.filter_map(|x| x.ok()).collect()
        };
        if ids.is_empty() {
            return Ok(());
        }
        let mut ids_to_forget = Vec::new();
        for id in &ids {
            if rand::random::<f64>() < forget_prob {
                ids_to_forget.push(id.clone());
            }
        }
        if ids_to_forget.is_empty() {
            log::info("没有短期记忆被遗忘");
            return Ok(());
        }
        log::info(format!("将遗忘 {} 条短期记忆", ids_to_forget.len()));
        for id in &ids_to_forget {
            conn.execute("DELETE FROM event_memroy WHERE id = ?1", params![id])?;
        }
        log::info(format!("成功遗忘短期记忆: {ids_to_forget:?}"));
        Ok(())
    }

    pub fn um_update(
        &self,
        user_id: i64,
        new_names: Vec<String>,
        wrong_names: Vec<String>,
        profile_update: Option<String>,
        event_update: Option<String>,
        max_events: usize,
        max_names: usize,
    ) {
        let mut ums = self.file_db.get_or("ums", json!({}));
        let uid_str = user_id.to_string();
        let mut current = if let Some(v) = ums.get(&uid_str) {
            parse_user_memory(v)
        } else {
            UserMemory::default()
        };
        let mut updated = false;
        for wrong in &wrong_names {
            if let Some(i) = current.names.iter().position(|n| n == wrong) {
                current.names.remove(i);
                updated = true;
                log::info(format!("移除用户 {user_id} 错误名字: {wrong}"));
            }
        }
        for new_name in &new_names {
            if current.names.iter().any(|n| n == new_name) {
                continue;
            }
            current.names.push(new_name.clone());
            if current.names.len() > max_names {
                let extra = current.names.len() - max_names;
                current.names.drain(0..extra);
            }
            updated = true;
            log::info(format!("更新用户 {user_id} 曾用名: {new_name}"));
        }
        if let Some(p) = profile_update {
            if p != current.profile {
                current.profile = p;
                updated = true;
                log::info(format!("更新用户 {user_id} 画像"));
            }
        }
        if let Some(ev) = event_update {
            current.recent_events.push((now_ts(), ev.clone()));
            if current.recent_events.len() > max_events {
                let extra = current.recent_events.len() - max_events;
                current.recent_events.drain(0..extra);
            }
            updated = true;
            log::info(format!("更新用户 {user_id} 事件: {ev}"));
        }
        if updated {
            if !ums.is_object() {
                ums = json!({});
            }
            ums.as_object_mut()
                .unwrap()
                .insert(uid_str, user_memory_to_json(&current));
            self.file_db.set("ums", ums);
        }
    }

    pub fn um_get(&self, user_id: i64) -> Option<UserMemory> {
        let ums = self.file_db.get("ums");
        ums.get(user_id.to_string()).map(parse_user_memory)
    }

    pub fn um_query_uid_by_name_in_message(&self, message: &str) -> HashSet<i64> {
        let ums = self.file_db.get("ums");
        let mut results = HashSet::new();
        if let Some(map) = ums.as_object() {
            for (uid_str, data) in map {
                let um = parse_user_memory(data);
                if um.names.iter().any(|n| !n.is_empty() && message.contains(n)) {
                    if let Ok(uid) = uid_str.parse::<i64>() {
                        results.insert(uid);
                    }
                }
            }
        }
        results
    }

    pub fn sm_add(&self, msg_id: i64, keep_count: usize, text: &str, sticker: &str) {
        let mut sms = self.file_db.get_or("sms", json!([]));
        if !sms.is_array() {
            sms = json!([]);
        }
        let mut item = json!({
            "id": msg_id.to_string(),
            "time": now_ts(),
        });
        if !sticker.is_empty() {
            item["sticker"] = json!(sticker);
        } else {
            item["text"] = json!(text);
        }
        sms.as_array_mut().unwrap().push(item);
        let arr = sms.as_array_mut().unwrap();
        if arr.len() > keep_count {
            let extra = arr.len() - keep_count;
            arr.drain(0..extra);
        }
        self.file_db.set("sms", sms);
        log::info(format!("更新自身对话记忆，保留最近 {keep_count} 条消息"));
    }

    pub fn sm_get(&self) -> Vec<SelfMemory> {
        let sms = self.file_db.get_or("sms", json!([]));
        sms.as_array()
            .map(|arr| arr.iter().filter_map(parse_self_memory).collect())
            .unwrap_or_default()
    }
}

fn parse_self_memory(v: &Value) -> Option<SelfMemory> {
    Some(SelfMemory {
        id: v.get("id")?.as_str()?.to_string(),
        time: v.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0),
        text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        sticker: v.get("sticker").and_then(|t| t.as_str()).unwrap_or("").to_string(),
    })
}

fn parse_user_memory(v: &Value) -> UserMemory {
    if v.get("text").is_some() && v.get("profile").is_none() {
        return UserMemory {
            profile: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            ..Default::default()
        };
    }
    let names = v
        .get("names")
        .and_then(|n| n.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let recent_events = v
        .get("recent_events")
        .and_then(|n| n.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|item| {
                    let arr = item.as_array()?;
                    if arr.len() < 2 {
                        return None;
                    }
                    let t = arr[0].as_f64().or_else(|| arr[0].as_i64().map(|i| i as f64))?;
                    let txt = arr[1].as_str()?.to_string();
                    Some((t, txt))
                })
                .collect()
        })
        .unwrap_or_default();
    UserMemory {
        names,
        profile: v.get("profile").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        recent_events,
    }
}

fn user_memory_to_json(um: &UserMemory) -> Value {
    json!({
        "names": um.names,
        "profile": um.profile,
        "recent_events": um.recent_events.iter().map(|(t, s)| json!([t, s])).collect::<Vec<_>>(),
    })
}

fn init_schema(conn: &Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_memroy (
            id TEXT PRIMARY KEY,
            text TEXT NOT NULL,
            type TEXT NOT NULL,
            weight INTEGER NOT NULL,
            created_at REAL NOT NULL,
            embedding BLOB NOT NULL
        );",
    );
}

fn import_chroma_if_any(conn: &Connection) {
    let has_embeddings: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='embeddings' LIMIT 1",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_embeddings {
        return;
    }
    // Best-effort: Chroma stores embeddings as BLOBs in `embeddings`.
    // Schema varies across versions; skip silently if columns don't match.
    let sqls = [
        "SELECT e.id, e.embedding, m.key, m.string_value
         FROM embeddings e
         LEFT JOIN embedding_metadata m ON e.id = m.id",
        "SELECT e.embedding_id, e.vector, m.key, m.string_value
         FROM embeddings e
         LEFT JOIN embedding_metadata m ON e.id = m.id",
    ];
    for sql in sqls {
        if try_import(conn, sql) {
            log::info("已从 chroma embeddings 表导入事件记忆向量");
            return;
        }
    }
}

fn try_import(conn: &Connection, sql: &str) -> bool {
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return false,
    };
    // Too version-specific; keep schema ready but don't fail startup.
    let _ = rows;
    false
}