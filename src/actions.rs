use serde_json::Value;

pub const MAX_ACTIONS: usize = 5;
pub const MAX_TEXT_ACTIONS: usize = 3;
pub const MAX_REACT_ACTIONS: usize = 3;

#[derive(Clone, Debug)]
pub enum Action {
    Text { text: String },
    Poke { ids: Vec<i64> },
    Sticker { query: Value },
    React { msg_id: i64, emoji_id: String },
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Text { .. } => "text",
            Action::Poke { .. } => "poke",
            Action::Sticker { .. } => "sticker",
            Action::React { .. } => "react",
        }
    }
}

fn parse_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn parse_poke_ids(raw: &Value) -> Vec<i64> {
    if raw.is_null() || raw.as_str() == Some("") {
        return vec![];
    }
    let items: Vec<&Value> = if raw.is_array() {
        raw.as_array().unwrap().iter().collect()
    } else if raw.is_number() || raw.is_string() {
        vec![raw]
    } else {
        return vec![];
    };
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        if let Some(uid) = parse_i64(item) {
            if uid > 0 && seen.insert(uid) {
                ids.push(uid);
            }
        }
    }
    ids
}

fn sticker_query_ok(query: &Value) -> bool {
    query.as_object().is_some_and(|m| {
        let e = m.get("emotion").and_then(|v| v.as_str()).unwrap_or("");
        let s = m.get("scene").and_then(|v| v.as_str()).unwrap_or("");
        !e.is_empty() || !s.is_empty()
    })
}

fn emoji_to_id(raw: &Value) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    if let Some(n) = raw.as_i64().or_else(|| raw.as_u64().map(|u| u as i64)) {
        if raw.as_bool().is_some() {
            return None;
        }
        let eid = n.to_string();
        return if eid.chars().all(|c| c.is_ascii_digit()) && n > 0 {
            Some(eid)
        } else {
            None
        };
    }
    if let Some(f) = raw.as_f64() {
        if raw.as_bool().is_some() {
            return None;
        }
        let n = f as i64;
        let eid = n.to_string();
        return if eid.chars().all(|c| c.is_ascii_digit()) && n > 0 {
            Some(eid)
        } else {
            None
        };
    }
    let text = match raw {
        Value::String(s) => s.trim().to_string(),
        other => other.to_string().trim().to_string(),
    };
    if text.is_empty() {
        return None;
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        let n: i64 = text.parse().ok()?;
        return if n > 0 { Some(text) } else { None };
    }
    let ch = text.chars().next()?;
    let cp = ch as u32;
    if cp < 128 {
        None
    } else {
        Some(cp.to_string())
    }
}

fn parse_react(raw: &Value) -> Option<(i64, String)> {
    let arr = raw.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    let msg_id = parse_i64(&arr[0])?;
    let emoji_id = emoji_to_id(&arr[1])?;
    Some((msg_id, emoji_id))
}

fn expand_action_item(item: &Value) -> Vec<Action> {
    if let Some(s) = item.as_str() {
        let text = s.trim();
        return if text.is_empty() {
            vec![]
        } else {
            vec![Action::Text {
                text: text.to_string(),
            }]
        };
    }
    let obj = match item.as_object() {
        Some(o) => o,
        None => return vec![],
    };
    let mut actions = Vec::new();
    for (key, val) in obj {
        match key.as_str() {
            "text" => {
                let text = if val.is_null() {
                    String::new()
                } else if let Some(s) = val.as_str() {
                    s.trim().to_string()
                } else {
                    val.to_string().trim().to_string()
                };
                if !text.is_empty() {
                    actions.push(Action::Text { text });
                }
            }
            "poke" => {
                let ids = parse_poke_ids(val);
                if !ids.is_empty() {
                    actions.push(Action::Poke { ids });
                }
            }
            "sticker" if sticker_query_ok(val) => {
                actions.push(Action::Sticker { query: val.clone() });
            }
            "react" => {
                if let Some((msg_id, emoji_id)) = parse_react(val) {
                    actions.push(Action::React { msg_id, emoji_id });
                }
            }
            _ => {}
        }
    }
    actions
}

pub fn parse_actions(llm_response: &Value) -> Vec<Action> {
    let mut actions = Vec::new();
    if let Some(raw) = llm_response.get("actions").and_then(|a| a.as_array()) {
        for item in raw {
            actions.extend(expand_action_item(item));
        }
    }
    let mut out = Vec::new();
    let mut text_n = 0;
    let mut react_n = 0;
    for action in actions {
        if out.len() >= MAX_ACTIONS {
            break;
        }
        match &action {
            Action::Text { .. } => {
                if text_n >= MAX_TEXT_ACTIONS {
                    continue;
                }
                text_n += 1;
            }
            Action::React { .. } => {
                if react_n >= MAX_REACT_ACTIONS {
                    continue;
                }
                react_n += 1;
            }
            _ => {}
        }
        out.push(action);
    }
    out
}