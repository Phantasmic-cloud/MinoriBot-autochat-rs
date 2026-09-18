use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug)]
pub struct Message {
    pub msg_id: i64,
    pub time: f64,
    pub user_id: i64,
    pub group_id: i64,
    pub nickname: String,
    pub msg: Vec<Value>,
}

fn json_i64(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn json_f64(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}

impl Message {
    pub fn from_json(v: &Value, fallback_group: Option<i64>) -> Option<Self> {
        let obj = v.as_object()?;
        let group_id = obj
            .get("group_id")
            .map(json_i64)
            .or(fallback_group)
            .unwrap_or(0);
        let msg = obj
            .get("msg")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Some(Message {
            msg_id: obj.get("msg_id").map(json_i64).unwrap_or(0),
            time: obj.get("time").map(json_f64).unwrap_or(0.0),
            user_id: obj.get("user_id").map(json_i64).unwrap_or(0),
            group_id,
            nickname: obj
                .get("nickname")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
            msg,
        })
    }

    pub fn is_poke(&self) -> bool {
        self.msg.iter().any(|seg| seg.get("type").and_then(|t| t.as_str()) == Some("poke"))
    }

    pub fn poke_target_id(&self) -> i64 {
        for seg in &self.msg {
            if seg.get("type").and_then(|t| t.as_str()) == Some("poke") {
                if let Some(data) = seg.get("data") {
                    return json_i64(data.get("target_id").unwrap_or(&Value::Null));
                }
            }
        }
        0
    }

    pub fn poke_key(&self) -> (i64, i64, i64) {
        (self.time as i64, self.user_id, self.poke_target_id())
    }

    pub fn plain_text(&self) -> String {
        let mut ret = String::new();
        for seg in &self.msg {
            if seg.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = seg.get("data").and_then(|d| d.get("text")).and_then(|t| t.as_str()) {
                    ret.push_str(t);
                }
            }
        }
        ret.trim().to_string()
    }
}

const POKE_KEEP: usize = 10;

static GROUP_POKES: OnceLock<Mutex<HashMap<i64, Vec<Message>>>> = OnceLock::new();

fn pokes() -> &'static Mutex<HashMap<i64, Vec<Message>>> {
    GROUP_POKES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn remember_poke(msg: &Message) {
    let mut map = pokes().lock();
    let lst = map.entry(msg.group_id).or_default();
    let key = msg.poke_key();
    if lst.iter().any(|p| p.poke_key() == key) {
        return;
    }
    lst.push(msg.clone());
    let extra = lst.len().saturating_sub(POKE_KEEP);
    if extra > 0 {
        lst.drain(0..extra);
    }
}

pub fn collect_recent_pokes(group_id: i64, since: f64) -> Vec<Message> {
    pokes()
        .lock()
        .get(&group_id)
        .map(|lst| lst.iter().filter(|p| p.time >= since).cloned().collect())
        .unwrap_or_default()
}

pub fn poke_person_label(uid: i64, name: &str, self_id: i64) -> String {
    if uid == self_id {
        "你".into()
    } else if name.is_empty() {
        uid.to_string()
    } else {
        format!("{name}({uid})")
    }
}

#[derive(Clone)]
pub struct AppState {
    pub rpc: crate::rpc::RpcSession,
    pub file_db: crate::filedb::FileDb,
    pub image_caption_db: crate::filedb::FileDb,
    pub memories: Arc<Mutex<HashMap<i64, Arc<crate::memory::MemorySystem>>>>,
    pub self_infos: Arc<Mutex<HashMap<i64, (i64, String)>>>,
}

impl AppState {
    pub fn new(rpc: crate::rpc::RpcSession) -> Self {
        AppState {
            rpc,
            file_db: crate::filedb::get_file_db("data/chat/autochat/db.json"),
            image_caption_db: crate::filedb::get_file_db("data/chat/autochat/image_captions.json"),
            memories: Arc::new(Mutex::new(HashMap::new())),
            self_infos: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn memory(&self, group_id: i64) -> Arc<crate::memory::MemorySystem> {
        let mut map = self.memories.lock();
        map.entry(group_id)
            .or_insert_with(|| Arc::new(crate::memory::MemorySystem::new("data/chat/autochat", group_id)))
            .clone()
    }
}