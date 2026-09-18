use crate::config::{yaml_to_json, Config};
use crate::log;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("RPC未连接")]
    NotConnected,
    #[error("RPC连接已关闭")]
    Closed,
    #[error("RPC请求超时")]
    Timeout,
    #[error("RPC请求错误: {0}")]
    Remote(String),
    #[error("{0}")]
    Other(String),
}

type Pending = HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Clone)]
pub struct RpcSession {
    inner: Arc<Inner>,
}

struct Inner {
    tx: Mutex<Option<mpsc::UnboundedSender<WsMessage>>>,
    pending: Mutex<Pending>,
    next_id: AtomicI64,
    _gen: AtomicU64,
}

impl RpcSession {
    pub fn new() -> Self {
        RpcSession {
            inner: Arc::new(Inner {
                tx: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicI64::new(1),
                _gen: AtomicU64::new(0),
            }),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.inner.tx.lock().is_some()
    }

    fn drop_pending(&self, err: RpcError) {
        let pending: Vec<_> = self.inner.pending.lock().drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(match &err {
                RpcError::Closed => RpcError::Closed,
                RpcError::NotConnected => RpcError::NotConnected,
                RpcError::Timeout => RpcError::Timeout,
                RpcError::Remote(s) => RpcError::Remote(s.clone()),
                RpcError::Other(s) => RpcError::Other(s.clone()),
            }));
        }
    }

    fn disconnect(&self) {
        *self.inner.tx.lock() = None;
        self.drop_pending(RpcError::Closed);
    }

    pub async fn run(self) {
        loop {
            let cfg = Config::global();
            let interval = cfg.f64_or("rpc.reconnect_interval", 5.0);
            if !self.is_connected() {
                match self.connect_once().await {
                    Ok(()) => {}
                    Err(e) => {
                        log::warning(format!(
                            "连接RPC服务器失败: {}，{interval}秒后重试",
                            e
                        ));
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs_f64(interval.max(0.1))).await;
        }
    }

    async fn connect_once(&self) -> anyhow::Result<()> {
        let cfg = Config::global();
        let host = cfg.str_or("rpc.host", "127.0.0.1");
        let port = cfg.i64_or("rpc.port", 555);
        let url = format!("ws://{host}:{port}");
        let (ws, _) = connect_async(&url).await?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();
        *self.inner.tx.lock() = Some(tx);
        self.inner._gen.fetch_add(1, Ordering::SeqCst);
        log::info(format!("成功连接到RPC服务器 {host}:{port}"));

        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let sess = self.clone();
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(WsMessage::Text(text)) => sess.on_text(&text),
                    Ok(WsMessage::Binary(bin)) => {
                        if let Ok(text) = std::str::from_utf8(&bin) {
                            sess.on_text(text);
                        }
                    }
                    Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) | Ok(WsMessage::Frame(_)) => {}
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                }
            }
            sess.disconnect();
            writer.abort();
            log::info("RPC连接已关闭");
        });
        Ok(())
    }

    fn on_text(&self, text: &str) {
        let v: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        let id = match v.get("id").and_then(|x| x.as_i64()) {
            Some(id) => id,
            None => return,
        };
        let tx = self.inner.pending.lock().remove(&id);
        if let Some(tx) = tx {
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(&err.to_string())
                    .to_string();
                let _ = tx.send(Err(RpcError::Remote(msg)));
            } else {
                let _ = tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
    }

    pub async fn call(&self, method: &str, args: Vec<Value>, timeout_secs: f64) -> Result<Value, RpcError> {
        if !self.is_connected() {
            return Err(RpcError::NotConnected);
        }
        let cfg = Config::global();
        let token = cfg.str("rpc.token");
        let mut params = Vec::with_capacity(args.len() + 1);
        params.push(Value::String(token));
        params.extend(args);

        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        log::debug(format!("发送RPC请求: {method} {}", json!(params[1..].to_vec())));

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id, tx);
        let send_ok = {
            let guard = self.inner.tx.lock();
            match guard.as_ref() {
                Some(s) => s.send(WsMessage::Text(req.to_string().into())).is_ok(),
                None => false,
            }
        };
        if !send_ok {
            self.inner.pending.lock().remove(&id);
            self.disconnect();
            return Err(RpcError::Closed);
        }

        let timeout = Duration::from_secs_f64(timeout_secs.max(0.1));
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => {
                self.inner.pending.lock().remove(&id);
                Err(RpcError::Timeout)
            }
        }
    }

    pub async fn get_self_info(&self, group_id: i64) -> Result<Value, RpcError> {
        let t = Config::global().f64_or("rpc.default_timeout", 5.0);
        self.call("get_self_info", vec![json!(group_id)], t).await
    }

    pub async fn send_group_msg(&self, group_id: i64, message: &str) -> Result<Value, RpcError> {
        let t = Config::global().f64_or("rpc.default_timeout", 5.0);
        self.call("send_group_msg", vec![json!(group_id), json!(message)], t)
            .await
    }

    pub async fn poke_group_member(&self, group_id: i64, user_id: i64) -> Result<Value, RpcError> {
        let t = Config::global().f64_or("rpc.default_timeout", 5.0);
        self.call("poke_group_member", vec![json!(group_id), json!(user_id)], t)
            .await
    }

    pub async fn set_msg_emoji_like(
        &self,
        group_id: i64,
        message_id: i64,
        emoji_id: &str,
    ) -> Result<Value, RpcError> {
        let t = Config::global().f64_or("rpc.default_timeout", 5.0);
        self.call(
            "set_msg_emoji_like",
            vec![json!(group_id), json!(message_id), json!(emoji_id)],
            t,
        )
        .await
    }

    pub async fn get_group_history_msg(&self, group_id: i64, limit: i64) -> Result<Value, RpcError> {
        let t = Config::global().f64_or("rpc.default_timeout", 5.0);
        self.call("get_group_history_msg", vec![json!(group_id), json!(limit)], t)
            .await
    }

    pub async fn get_new_msgs(&self) -> Result<Value, RpcError> {
        let t = Config::global().f64_or("rpc.default_timeout", 5.0);
        self.call("get_new_msgs", vec![], t).await
    }

    pub async fn query_llm(
        &self,
        model: Value,
        prompt: &str,
        images: Vec<Value>,
        options: Value,
    ) -> Result<Value, RpcError> {
        let timeout = options
            .get("timeout")
            .and_then(|v| v.as_f64())
            .unwrap_or(300.0)
            + 5.0;
        self.call(
            "query_llm",
            vec![model, json!(prompt), json!(images), options],
            timeout,
        )
        .await
    }

    pub async fn query_embeddings(&self, texts: Vec<String>, model_name: &str) -> Result<Value, RpcError> {
        self.call(
            "query_embedding",
            vec![json!(texts), json!(model_name)],
            60.0,
        )
        .await
    }
}

pub fn cfg_model(key: &str) -> Value {
    yaml_to_json(&Config::global().get(key))
}