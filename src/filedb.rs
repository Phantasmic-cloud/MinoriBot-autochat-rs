use crate::log;
use crate::util::truncate;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

static DBS: OnceLock<Mutex<HashMap<String, FileDb>>> = OnceLock::new();

#[derive(Clone)]
pub struct FileDb {
    path: PathBuf,
    data: Arc<Mutex<Value>>,
}

impl FileDb {
    fn new(path: PathBuf) -> Self {
        let data = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => {
                    log::debug(format!("加载数据库 {} 成功", path.display()));
                    v
                }
                Err(_) => {
                    log::debug(format!("加载数据库 {} 失败 使用空数据", path.display()));
                    Value::Object(Default::default())
                }
            },
            Err(_) => {
                log::debug(format!("加载数据库 {} 失败 使用空数据", path.display()));
                Value::Object(Default::default())
            }
        };
        FileDb {
            path,
            data: Arc::new(Mutex::new(data)),
        }
    }

    pub fn get(&self, key: &str) -> Value {
        self.data
            .lock()
            .as_object()
            .and_then(|m| m.get(key).cloned())
            .unwrap_or(Value::Null)
    }

    pub fn get_or(&self, key: &str, default: Value) -> Value {
        let v = self.get(key);
        if v.is_null() {
            default
        } else {
            v
        }
    }

    pub fn set(&self, key: &str, value: Value) {
        log::debug(format!(
            "设置数据库 {} {key} = {}",
            self.path.display(),
            truncate(&value.to_string(), 32)
        ));
        {
            let mut data = self.data.lock();
            if !data.is_object() {
                *data = Value::Object(Default::default());
            }
            data.as_object_mut().unwrap().insert(key.to_string(), value);
        }
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let data = self.data.lock().clone();
        let bytes = serde_json::to_vec_pretty(&data).unwrap_or_else(|_| b"{}".to_vec());
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, &self.path);
            log::debug(format!("保存数据库 {}", self.path.display()));
        }
    }
}

pub fn get_file_db(path: impl AsRef<Path>) -> FileDb {
    let path = path.as_ref();
    let key = path.to_string_lossy().to_string();
    let map = DBS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = map.lock();
    g.entry(key)
        .or_insert_with(|| FileDb::new(path.to_path_buf()))
        .clone()
}