use anyhow::{bail, Context};
use parking_lot::Mutex;
use serde_yaml::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Clone)]
pub struct Config {
    path: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    mtime: Option<u64>,
    data: Value,
}

impl Config {
    pub fn load(path: PathBuf) -> Self {
        let cfg = Config {
            path,
            inner: Arc::new(Mutex::new(Inner {
                mtime: None,
                data: Value::Null,
            })),
        };
        cfg.refresh();
        cfg
    }

    pub fn set_global(self) -> &'static Config {
        CONFIG.set(self).ok();
        CONFIG.get().expect("config")
    }

    pub fn global() -> &'static Config {
        CONFIG.get().expect("config not initialized")
    }

    fn file_mtime(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    }

    fn refresh(&self) {
        if !self.path.exists() {
            eprintln!("[WARNING] 找不到配置文件 {}", self.path.display());
            return;
        }
        let mtime = Self::file_mtime(&self.path);
        let mut inner = self.inner.lock();
        if inner.mtime == mtime && !inner.data.is_null() {
            return;
        }
        match fs::read_to_string(&self.path) {
            Ok(text) => match serde_yaml::from_str::<Value>(&text) {
                Ok(data) => {
                    inner.mtime = mtime;
                    inner.data = data;
                }
                Err(e) => {
                    eprintln!("[WARNING] 读取配置文件 {} 失败: {}", self.path.display(), e);
                    inner.mtime = mtime;
                }
            },
            Err(e) => {
                eprintln!("[WARNING] 读取配置文件 {} 失败: {}", self.path.display(), e);
                inner.mtime = mtime;
            }
        }
    }

    pub fn get(&self, key: &str) -> Value {
        self.try_get(key).unwrap_or(Value::Null)
    }

    pub fn try_get(&self, key: &str) -> Option<Value> {
        self.refresh();
        let inner = self.inner.lock();
        let mut cur = &inner.data;
        for part in key.split('.') {
            cur = mapping_get(cur, part)?;
        }
        Some(cur.clone())
    }

    pub fn require(&self, key: &str) -> anyhow::Result<Value> {
        self.try_get(key)
            .with_context(|| format!("配置 chat.autochat 中不存在 {key}"))
    }

    pub fn get_or(&self, key: &str, default: Value) -> Value {
        self.try_get(key).unwrap_or(default)
    }

    pub fn str(&self, key: &str) -> String {
        value_to_string(&self.get(key))
    }

    pub fn str_or(&self, key: &str, default: &str) -> String {
        match self.try_get(key) {
            Some(v) if !v.is_null() => value_to_string(&v),
            _ => default.to_string(),
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.try_get(key) {
            Some(Value::Bool(b)) => b,
            Some(Value::String(s)) => matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
            _ => default,
        }
    }

    pub fn i64_or(&self, key: &str, default: i64) -> i64 {
        match self.try_get(key) {
            Some(v) => value_to_i64(&v).unwrap_or(default),
            None => default,
        }
    }

    pub fn usize_or(&self, key: &str, default: usize) -> usize {
        self.i64_or(key, default as i64).max(0) as usize
    }

    pub fn f64_or(&self, key: &str, default: f64) -> f64 {
        match self.try_get(key) {
            Some(v) => value_to_f64(&v).unwrap_or(default),
            None => default,
        }
    }

    pub fn mapping(&self, key: &str) -> HashMap<String, Value> {
        match self.try_get(key) {
            Some(Value::Mapping(m)) => m
                .iter()
                .map(|(k, v)| (value_to_string(k), v.clone()))
                .collect(),
            _ => HashMap::new(),
        }
    }
}

pub fn mapping_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let map = v.as_mapping()?;
    if let Some(val) = map.get(&Value::String(key.to_string())) {
        return Some(val);
    }
    if let Ok(i) = key.parse::<i64>() {
        if let Some(val) = map.get(&Value::Number(i.into())) {
            return Some(val);
        }
    }
    if let Ok(f) = key.parse::<f64>() {
        let n = serde_yaml::Number::from(f);
        if let Some(val) = map.get(&Value::Number(n)) {
            return Some(val);
        }
    }
    None
}

pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

pub fn value_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn yaml_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Sequence(seq) => serde_json::Value::Array(seq.iter().map(yaml_to_json).collect()),
        Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map {
                obj.insert(value_to_string(k), yaml_to_json(val));
            }
            serde_json::Value::Object(obj)
        }
        Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

/// Python `str.format` 的常用子集：`{{` / `}}` 转义，`{name}` 替换。
pub fn python_format(template: &str, vars: &HashMap<&str, String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                out.push('{');
                i += 2;
                continue;
            }
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '}') {
                let name: String = chars[i + 1..i + 1 + end].iter().collect();
                let name = name.trim();
                if name.is_empty() || name.contains('{') {
                    bail!("invalid format field {{{name}}}");
                }
                let key = name.split('!').next().unwrap_or(name).split(':').next().unwrap_or(name);
                match vars.get(key) {
                    Some(val) => out.push_str(val),
                    None => bail!("配置 format 缺少字段: {key}"),
                }
                i += end + 2;
            } else {
                bail!("unmatched '{{' in format string");
            }
        } else if c == '}' {
            if i + 1 < chars.len() && chars[i + 1] == '}' {
                out.push('}');
                i += 2;
            } else {
                bail!("unmatched '}}' in format string");
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

pub fn persona_for(persona: &Value, group_id: i64) -> String {
    if let Some(map) = persona.as_mapping() {
        let as_int = Value::Number(group_id.into());
        let as_str = Value::String(group_id.to_string());
        if let Some(v) = map.get(&as_int).or_else(|| map.get(&as_str)) {
            return value_to_string(v);
        }
        if let Some(v) = map.get(&Value::String("default".into())) {
            return value_to_string(v);
        }
    }
    String::new()
}