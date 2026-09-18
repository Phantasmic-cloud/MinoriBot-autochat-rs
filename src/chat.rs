use crate::actions::{parse_actions, Action};
use crate::config::{persona_for, python_format, value_to_f64, Config};
use crate::format::{format_msgs, generate_summary, maybe_dump_prompt};
use crate::log;
use crate::memory::{EventMemory, SelfMemory};
use crate::rpc::cfg_model;
use crate::sticker::{get_sticker_multiplier, search_sticker, update_sticker_multipliers, StickerHit};
use crate::types::{collect_recent_pokes, remember_poke, AppState, Message};
use crate::util::{get_readable_datetime, json_f64_vec, now_ts, truncate};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone)]
struct GroupStatus {
    group_id: i64,
    willingness: f64,
    self_msg_ids: Vec<i64>,
    last_check_willing_time: Option<f64>,
    last_reply_time: Option<f64>,
}

impl GroupStatus {
    fn load(state: &AppState, group_id: i64) -> Self {
        let data = state.file_db.get(&format!("status_{group_id}"));
        GroupStatus {
            group_id,
            willingness: data.get("willingness").and_then(|v| v.as_f64()).unwrap_or(0.0),
            self_msg_ids: data
                .get("self_msg_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64)))
                        .collect()
                })
                .unwrap_or_default(),
            last_check_willing_time: data.get("last_check_willing_time").and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))
                }
            }),
            last_reply_time: data.get("last_reply_time").and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))
                }
            }),
        }
    }

    fn save(&self, state: &AppState) {
        state.file_db.set(
            &format!("status_{}", self.group_id),
            json!({
                "willingness": self.willingness,
                "self_msg_ids": self.self_msg_ids,
                "last_check_willing_time": self.last_check_willing_time,
                "last_reply_time": self.last_reply_time,
            }),
        );
    }
}

fn json_i64(v: Option<&Value>) -> Option<i64> {
    v.and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_u64().map(|u| u as i64))
            .or_else(|| x.as_f64().map(|f| f as i64))
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    })
}

async fn get_self_info(state: &AppState, group_id: i64) -> anyhow::Result<(i64, String)> {
    if let Some(v) = state.self_infos.lock().get(&group_id).cloned() {
        return Ok(v);
    }
    let info = state.rpc.get_self_info(group_id).await?;
    let self_id = json_i64(info.get("self_id")).unwrap_or(0);
    let nickname = info
        .get("nickname")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    state.self_infos.lock().insert(group_id, (self_id, nickname.clone()));
    Ok((self_id, nickname))
}

pub async fn chat(state: &AppState, msg: Message) {
    let (self_id, self_name) = match get_self_info(state, msg.group_id).await {
        Ok(v) => v,
        Err(e) => {
            log::error(format!("获取自身信息失败: {e}"));
            return;
        }
    };

    let is_poke = msg.is_poke();
    let poke_target = if is_poke { msg.poke_target_id() } else { 0 };
    if is_poke {
        remember_poke(&msg);
    }
    if msg.user_id == self_id {
        return;
    }
    if !is_poke && msg.plain_text().starts_with('/') {
        return;
    }
    if is_poke && poke_target != self_id {
        return;
    }

    let mut status = GroupStatus::load(state, msg.group_id);
    if let Some(last) = status.last_reply_time {
        if msg.time <= last {
            return;
        }
    }

    if is_poke {
        log::info(format!(
            "{} 的戳一戳 {}({}) -> {poke_target}",
            msg.group_id, msg.nickname, msg.user_id
        ));
    } else {
        log::info(format!(
            "{} 的新消息 {} {}({}): {}",
            msg.group_id,
            msg.msg_id,
            msg.nickname,
            msg.user_id,
            msg.plain_text()
        ));
    }

    let cfg = Config::global();
    let last_willingness = status.willingness;
    let mut delta = 0.0;
    if let Some(t) = status.last_check_willing_time {
        let time_passed = now_ts() - t;
        delta -= (cfg.f64_or("chat.willing.decrease_per_minute", 0.005) * time_passed / 60.0)
            .min(status.willingness);
    }
    if is_poke {
        delta += cfg.f64_or("chat.willing.increase_per_poke", 0.3);
    } else {
        delta += cfg.f64_or("chat.willing.increase_per_msg", 0.005);
        let mut got_at = false;
        let mut got_reply = false;
        for seg in &msg.msg {
            let stype = seg.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let sdata = seg.get("data").cloned().unwrap_or(json!({}));
            if !got_at && stype == "at" {
                if json_i64(sdata.get("qq")) == Some(self_id) {
                    delta += cfg.f64_or("chat.willing.increase_per_at", 1.0);
                    got_at = true;
                }
            }
            if !got_reply && stype == "reply" {
                if let Some(id) = json_i64(sdata.get("id")) {
                    if status.self_msg_ids.contains(&id) {
                        delta += cfg.f64_or("chat.willing.increase_per_reply", 0.3);
                        got_reply = true;
                    }
                }
            }
            if got_at && got_reply {
                break;
            }
        }
        let plain = msg.plain_text().to_lowercase();
        for (kw, val) in cfg.mapping("chat.willing.increase_keywords") {
            if plain.contains(&kw.to_lowercase()) {
                if let Some(v) = value_to_f64(&val) {
                    delta += v;
                }
            }
        }
    }
    let scale = cfg
        .mapping("chat.willing.group_scale")
        .get(&msg.group_id.to_string())
        .and_then(value_to_f64)
        .unwrap_or(1.0);
    delta *= scale;
    status.willingness += delta;
    status.willingness = status.willingness.min(cfg.f64_or("chat.willing.limit", 1.0));
    status.last_check_willing_time = Some(now_ts());
    status.save(state);

    let reply_rate = status.willingness.clamp(0.0, 1.0);
    if rand::random::<f64>() > reply_rate {
        log::info(format!(
            "意愿值: {last_willingness:.4} -> {:.4}",
            status.willingness
        ));
        return;
    }
    log::info(format!(
        "意愿值: {last_willingness:.4} -> {:.4}, 决定回复该消息",
        status.willingness
    ));
    let delay = cfg.f64_or("chat.get_history_msg_delay_seconds", 0.0);
    if delay > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    }

    log::info("=".repeat(20));
    log::info(format!("开始对消息 {} 进行聊天处理", msg.msg_id));

    let (recent_msgs, recent_text, recent_summary, query_embs, recent_emb) =
        match prepare_context(state, &msg, is_poke).await {
            Ok(v) => v,
            Err(e) => {
                log::error(format!("处理消息时失败，放弃聊天处理: {e}"));
                return;
            }
        };

    let mem = state.memory(msg.group_id);
    let short_em_num = cfg.usize_or("chat.mem.short_em_num", 3);
    let long_em_num = cfg.usize_or("chat.mem.long_em_num", 1);
    let mut em_text = String::new();
    let mut short_ems: Vec<EventMemory> = Vec::new();
    let long_ems: Vec<EventMemory>;
    if short_em_num + long_em_num > 0 {
        short_ems = match mem.em_query(
            &query_embs,
            short_em_num,
            "short_term",
            cfg.f64_or("chat.mem.em_time_decay_per_hour", 0.02),
        ) {
            Ok(v) => v,
            Err(e) => {
                log::error(format!("获取记忆时失败，放弃聊天处理: {e}"));
                return;
            }
        };
        long_ems = match mem.em_query(&query_embs, long_em_num, "long_term", 0.0) {
            Ok(v) => v,
            Err(e) => {
                log::error(format!("获取记忆时失败，放弃聊天处理: {e}"));
                return;
            }
        };
        log::info(format!(
            "获取短期事件记忆共 {} 条: {:?}",
            short_ems.len(),
            short_ems.iter().map(|e| &e.id).collect::<Vec<_>>()
        ));
        log::info(format!(
            "获取长期事件记忆共 {} 条: {:?}",
            long_ems.len(),
            long_ems.iter().map(|e| &e.id).collect::<Vec<_>>()
        ));
        if !short_ems.is_empty() || !long_ems.is_empty() {
            em_text.push_str("可能与你当前聊天内容相关的记忆事件:\n```\n");
            for em in short_ems.iter().chain(long_ems.iter()) {
                em_text.push_str(&format!(
                    "{}: {}\n",
                    get_readable_datetime(em.created_at, true),
                    em.text
                ));
            }
            em_text.push_str("```\n");
        }
    } else {
        long_ems = vec![];
    }

    let sm_num = cfg.usize_or("chat.mem.sm_num", 5);
    let mut sm_text = String::new();
    if sm_num > 0 {
        let mut sms: Vec<SelfMemory> = mem.sm_get();
        if sms.len() > sm_num {
            sms = sms.split_off(sms.len() - sm_num);
        }
        log::info(format!(
            "获取自身记忆共 {} 条: {:?}",
            sms.len(),
            sms.iter().map(|s| &s.id).collect::<Vec<_>>()
        ));
        if !sms.is_empty() {
            sm_text.push_str("你自己过去的回复记录供参考:\n```\n");
            for sm in &sms {
                let body = if !sm.sticker.is_empty() {
                    format!("[表情包: {}]", sm.sticker)
                } else {
                    sm.text.clone()
                };
                sm_text.push_str(&format!(
                    "{} [{}]: {body}\n",
                    get_readable_datetime(sm.time, true),
                    sm.id
                ));
            }
            sm_text.push_str("```\n");
        }
    }

    let um_num = cfg.usize_or("chat.mem.um_num", 3);
    let mut um_text = String::new();
    let mut top_user_ids: Vec<i64> = Vec::new();
    if um_num > 0 {
        let mut user_msg_counts: HashMap<i64, i64> = HashMap::new();
        for m in &recent_msgs {
            *user_msg_counts.entry(m.user_id).or_insert(0) += 1;
        }
        let mut top_users: Vec<(i64, i64)> = user_msg_counts.into_iter().collect();
        top_users.sort_by(|a, b| b.1.cmp(&a.1));
        let mut candidate_uids: Vec<i64> = top_users.into_iter().map(|(uid, _)| uid).collect();
        let full_msg: String = recent_msgs.iter().map(|m| m.plain_text()).collect();
        let mentioned = mem.um_query_uid_by_name_in_message(&full_msg);
        for uid in mentioned {
            if !candidate_uids.contains(&uid) {
                candidate_uids.insert(0, uid);
            }
        }
        candidate_uids.truncate(um_num);
        let mut ums_content = Vec::new();
        for user_id in &candidate_uids {
            top_user_ids.push(*user_id);
            if let Some(um) = mem.um_get(*user_id) {
                let mut u_info = format!("用户ID: {user_id}\n");
                if !um.names.is_empty() {
                    u_info.push_str(&format!("  - 曾用名: {}\n", um.names.join(", ")));
                }
                if !um.profile.is_empty() {
                    u_info.push_str(&format!("  - 简介: {}\n", um.profile));
                }
                if !um.recent_events.is_empty() {
                    u_info.push_str("  - 最近事件:\n");
                    for (t, txt) in &um.recent_events {
                        u_info.push_str(&format!(
                            "    [{}]: {txt}\n",
                            get_readable_datetime(*t, true)
                        ));
                    }
                }
                ums_content.push(u_info);
            }
        }
        if !ums_content.is_empty() {
            um_text.push_str("你对聊天中的部分用户的记忆:\n```\n");
            um_text.push_str(&ums_content.join("\n"));
            um_text.push_str("```\n");
        }
    }

    let recent_block = format!("以下是最近的聊天记录:\n```\n{recent_text}\n```");
    let persona_val = cfg.get("chat.prompt.persona");
    let persona = persona_for(&persona_val, msg.group_id);
    let framework = cfg.str("chat.prompt.framework");
    let mut vars = HashMap::new();
    vars.insert("self_id", self_id.to_string());
    vars.insert("self_name", self_name);
    vars.insert("persona", persona);
    vars.insert("recent_text", recent_block);
    vars.insert("em_text", em_text);
    vars.insert("sm_text", sm_text);
    vars.insert("um_text", um_text);
    let full_prompt = match python_format(&framework, &vars) {
        Ok(p) => p,
        Err(e) => {
            log::error(format!("请求LLM生成回复时失败，放弃聊天处理: {e}"));
            return;
        }
    };
    maybe_dump_prompt(&full_prompt);

    log::info(format!(
        "开始请求LLM生成回复，输入长度 {} 字符",
        full_prompt.chars().count()
    ));
    let options = json!({
        "timeout": cfg.f64_or("chat.llm.timeout", 120.0),
        "max_tokens": cfg.i64_or("chat.llm.max_tokens", 10240),
        "json_reply": true,
        "json_key_restraints": [
            {"key": "actions", "type": "list"},
            {"key": "user_updates", "type": "list"},
        ],
    });
    let llm_response = match state
        .rpc
        .query_llm(cfg_model("chat.llm.model"), &full_prompt, vec![], options)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            log::error(format!("请求LLM生成回复时失败，放弃聊天处理: {e}"));
            return;
        }
    };
    log::info(format!("LLM生成回复成功: {llm_response}"));
    let actions = parse_actions(&llm_response);
    let user_updates = llm_response.get("user_updates").cloned().unwrap_or(json!([]));

    let send_msg_id_texts = match execute_actions(state, &msg, &recent_msgs, &actions).await {
        Ok(v) => v,
        Err(e) => {
            log::error(format!("发送回复时失败: {e}"));
            return;
        }
    };

    {
        let mut status = GroupStatus::load(state, msg.group_id);
        let last = status.willingness;
        status.willingness *= cfg.f64_or("chat.willing.decay_after_send", 0.6);
        status.willingness -= cfg.f64_or("chat.willing.decrease_after_send", 0.5);
        status.willingness = status.willingness.max(0.0);
        status.save(state);
        log::info(format!("聊天后意愿值: {last:.4} -> {:.4}", status.willingness));
    }

    if let Err(e) = mem.em_add(&recent_summary, &recent_emb, 0) {
        log::error(format!("更新记忆失败: {e}"));
        return;
    }
    for em in &short_ems {
        let _ = mem.em_increase_weight(
            &em.id,
            cfg.i64_or("chat.mem.short_em_reward", 1),
            cfg.i64_or("chat.mem.em_long_term_threshold", 10),
        );
    }
    let forget_days = cfg.f64_or("chat.mem.short_em_forget_days", 3.0);
    let forget_time = now_ts() - forget_days * 86400.0;
    let _ = mem.em_forget(forget_time, cfg.f64_or("chat.mem.short_em_forget_prob", 0.1));

    if let Some(arr) = user_updates.as_array() {
        for update in arr {
            let uid = match json_i64(update.get("user_id")) {
                Some(u) => u,
                None => continue,
            };
            if !top_user_ids.contains(&uid) && uid != msg.user_id {
                continue;
            }
            let mut new_names = Vec::new();
            if let Some(n) = update.get("new_name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    new_names.push(n.to_string());
                }
            }
            for m in recent_msgs.iter().rev() {
                if m.user_id == uid {
                    new_names.push(m.nickname.clone());
                    break;
                }
            }
            let wrong_names = update
                .get("wrong_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let profile = update.get("profile").and_then(|v| v.as_str()).map(|s| s.to_string());
            let event = update.get("new_event").and_then(|v| v.as_str()).map(|s| s.to_string());
            mem.um_update(
                uid,
                new_names,
                wrong_names,
                profile,
                event,
                cfg.usize_or("chat.mem.um_max_events", 5),
                cfg.usize_or("chat.mem.um_max_names", 10),
            );
        }
    }

    let keep_count = cfg.usize_or("chat.mem.sm_keep_count", 10);
    for (msg_id, kind, content) in send_msg_id_texts {
        if kind == "sticker" {
            mem.sm_add(msg_id, keep_count, "", &content);
        } else {
            mem.sm_add(msg_id, keep_count, &content, "");
        }
    }

    log::info(format!("完成对消息 {} 的聊天处理", msg.msg_id));
    log::info("=".repeat(20));
    let _ = long_ems;
}

async fn prepare_context(
    state: &AppState,
    msg: &Message,
    is_poke: bool,
) -> anyhow::Result<(Vec<Message>, String, String, Vec<Vec<f32>>, Vec<f32>)> {
    let cfg = Config::global();
    let raw = state
        .rpc
        .get_group_history_msg(msg.group_id, cfg.i64_or("chat.history_msg_num", 20))
        .await?;
    let mut recent_msgs: Vec<Message> = raw
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| Message::from_json(v, Some(msg.group_id)))
        .filter(|m| !m.plain_text().starts_with('/'))
        .collect();
    if !is_poke && msg.msg_id != 0 && !recent_msgs.iter().any(|m| m.msg_id == msg.msg_id) {
        recent_msgs.push(msg.clone());
    }
    let since = recent_msgs
        .iter()
        .map(|m| m.time)
        .fold(None, |acc: Option<f64>, t| Some(acc.map(|a| a.min(t)).unwrap_or(t)))
        .unwrap_or(msg.time);
    let mut poke_keys: HashSet<(i64, i64, i64)> = recent_msgs
        .iter()
        .filter(|m| m.is_poke())
        .map(|m| m.poke_key())
        .collect();
    for poke in collect_recent_pokes(msg.group_id, since) {
        let key = poke.poke_key();
        if poke_keys.insert(key) {
            recent_msgs.push(poke);
        }
    }
    log::info(format!("获取最近共 {} 条有效聊天记录", recent_msgs.len()));

    let (self_id, _) = get_self_info(state, msg.group_id).await?;
    let recent_text = format_msgs(
        state,
        &recent_msgs,
        cfg.f64_or("image_caption.image_limit", 1.0),
        cfg.f64_or("image_caption.image_prob", 1.0),
        cfg.f64_or("image_caption.emotion_limit", 1.0),
        cfg.f64_or("image_caption.emotion_prob", 0.2),
        self_id,
    )
    .await;
    let recent_summary = generate_summary(state, &recent_text).await;
    if recent_summary.is_empty() {
        anyhow::bail!("生成聊天记录摘要失败，放弃聊天处理");
    }
    let mut last_long_msg = None;
    for m in recent_msgs.iter().rev() {
        let t = m.plain_text();
        if t.chars().count() >= 4 {
            last_long_msg = Some(t);
            break;
        }
    }
    let mut texts_to_embed = vec![recent_summary.clone()];
    if let Some(t) = last_long_msg {
        texts_to_embed.push(t);
    }
    let emb_model = cfg.str("chat.llm.emb_model");
    let embs_v = state.rpc.query_embeddings(texts_to_embed, &emb_model).await?;
    let query_embs: Vec<Vec<f32>> = embs_v
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(json_f64_vec)
        .collect();
    let recent_emb = query_embs
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("embedding 为空"))?;
    Ok((recent_msgs, recent_text, recent_summary, query_embs, recent_emb))
}

struct ExecAction {
    action: Action,
    hit: Option<StickerHit>,
}

async fn execute_actions(
    state: &AppState,
    msg: &Message,
    recent_msgs: &[Message],
    actions: &[Action],
) -> anyhow::Result<Vec<(i64, &'static str, String)>> {
    let cfg = Config::global();
    let mut send_msg_id_texts: Vec<(i64, &'static str, String)> = Vec::new();
    let mut first_action = true;
    let interval = cfg.f64_or("chat.reply_interval_seconds", 1.0);

    async fn wait_interval(first: &mut bool, interval: f64) {
        if *first {
            *first = false;
            return;
        }
        tokio::time::sleep(Duration::from_secs_f64(interval.max(0.0))).await;
    }

    let sticker_indexes: Vec<usize> = actions
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a, Action::Sticker { .. }))
        .map(|(i, _)| i)
        .collect();
    let mut sticker_hits: HashMap<usize, StickerHit> = HashMap::new();
    if !sticker_indexes.is_empty() {
        let timeout = cfg.f64_or("chat.sticker.timeout", 5.0);
        let fut = async {
            for i in &sticker_indexes {
                if let Action::Sticker { query } = &actions[*i] {
                    sticker_hits.insert(*i, search_sticker(&state.rpc, msg.group_id, query).await);
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs_f64(timeout.max(0.1)), fut).await {
            Ok(()) => {}
            Err(_) => log::warning("Sticker搜索超时，抛弃未就绪的表情包"),
        }
    }

    let mut exec_actions = Vec::new();
    for (i, action) in actions.iter().enumerate() {
        match action {
            Action::Sticker { .. } => {
                match sticker_hits.get(&i) {
                    Some(hit) if hit.path.is_some() => {
                        exec_actions.push(ExecAction {
                            action: action.clone(),
                            hit: Some(StickerHit {
                                path: hit.path.clone(),
                                sid: hit.sid,
                                all_sids: hit.all_sids.clone(),
                                old_multipliers: hit.old_multipliers.clone(),
                            }),
                        });
                    }
                    Some(_) => log::info("未匹配到表情包，跳过发送"),
                    None => log::info("表情包未就绪，跳过发送"),
                }
            }
            _ => exec_actions.push(ExecAction {
                action: action.clone(),
                hit: None,
            }),
        }
    }

    let at_re = Regex::new(r"\[@(\d+)\]").unwrap();
    let reply_re = Regex::new(r"\[reply=(-?\d+)\]").unwrap();
    let mut text_index = 0i32;

    for item in exec_actions {
        wait_interval(&mut first_action, interval).await;
        match item.action {
            Action::Text { mut text } => {
                text_index += 1;
                if text.is_empty() {
                    log::info(format!("LLM生成的回复{text_index}为空，放弃发送"));
                    continue;
                }
                let mut at_id: Option<i64> = None;
                let mut reply_id: Option<i64> = None;
                if let Some(cap) = at_re.captures(&text) {
                    if let Ok(id) = cap[1].parse::<i64>() {
                        at_id = Some(id);
                        text = text.replace(&cap[0], "");
                        if recent_msgs.iter().any(|m| m.user_id == id) {
                            text = format!("[CQ:at,qq={id}]{text}");
                        }
                    }
                }
                if let Some(cap) = reply_re.captures(&text) {
                    if let Ok(id) = cap[1].parse::<i64>() {
                        reply_id = Some(id);
                        text = text.replace(&cap[0], "");
                        if recent_msgs.iter().any(|m| m.msg_id == id) {
                            text = format!("[CQ:reply,id={id}]{text}");
                        }
                    }
                }
                text = truncate(&text, cfg.i64_or("chat.reply_max_length", 512));
                log::info(format!(
                    "自动聊天生成回复{text_index}: {text} at_id={:?} reply_id={:?}",
                    at_id, reply_id
                ));
                match state.rpc.send_group_msg(msg.group_id, &text).await {
                    Ok(ret) => {
                        let send_msg_id = json_i64(ret.get("message_id")).unwrap_or(0);
                        send_msg_id_texts.push((send_msg_id, "text", text));
                        log::info(format!("发送回复{text_index}成功: send_msg_id={send_msg_id}"));
                        note_sent_msg(state, msg.group_id, send_msg_id);
                    }
                    Err(e) => log::warning(format!("发送回复失败: {e}")),
                }
            }
            Action::Sticker { query } => {
                let hit = match item.hit {
                    Some(h) => h,
                    None => continue,
                };
                let path = match &hit.path {
                    Some(p) => p.clone(),
                    None => {
                        log::info("未匹配到表情包，跳过发送");
                        continue;
                    }
                };
                let cq = format!("[CQ:image,file=file://{path}]");
                match state.rpc.send_group_msg(msg.group_id, &cq).await {
                    Ok(ret) => {
                        let send_msg_id = json_i64(ret.get("message_id")).unwrap_or(0);
                        note_sent_msg(state, msg.group_id, send_msg_id);
                        let sid = hit.sid.unwrap_or(0);
                        log::info(format!("表情包发送成功: sid={sid}"));
                        let emotion = query.get("emotion").and_then(|v| v.as_str()).unwrap_or("").trim();
                        let scene = query.get("scene").and_then(|v| v.as_str()).unwrap_or("").trim();
                        let desc = if !emotion.is_empty() && !scene.is_empty() {
                            format!("{emotion}/{scene}")
                        } else if !emotion.is_empty() {
                            emotion.to_string()
                        } else if !scene.is_empty() {
                            scene.to_string()
                        } else {
                            format!("sid={sid}")
                        };
                        send_msg_id_texts.push((send_msg_id, "sticker", desc));
                        update_sticker_multipliers(msg.group_id, hit.sid, &hit.all_sids);
                        let changed: Vec<String> = hit
                            .all_sids
                            .iter()
                            .filter_map(|s| {
                                let old = hit.old_multipliers.get(s).copied().unwrap_or(1.0);
                                let new = get_sticker_multiplier(msg.group_id, *s);
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
                    }
                    Err(e) => log::warning(format!("表情包发送失败: {e}")),
                }
            }
            Action::Poke { ids } => {
                for uid in ids {
                    match state.rpc.poke_group_member(msg.group_id, uid).await {
                        Ok(_) => log::info(format!("戳一戳成功: user_id={uid}")),
                        Err(e) => log::warning(format!("戳一戳失败 user_id={uid}: {e}")),
                    }
                }
            }
            Action::React { msg_id, emoji_id } => {
                if !recent_msgs.iter().any(|m| m.msg_id == msg_id) {
                    log::info(format!("贴表情跳过，消息不在最近记录中: msg_id={msg_id}"));
                    continue;
                }
                match state
                    .rpc
                    .set_msg_emoji_like(msg.group_id, msg_id, &emoji_id)
                    .await
                {
                    Ok(_) => log::info(format!("贴表情成功: msg_id={msg_id} emoji_id={emoji_id}")),
                    Err(e) => log::warning(format!("贴表情失败 msg_id={msg_id} emoji_id={emoji_id}: {e}")),
                }
            }
        }
    }
    Ok(send_msg_id_texts)
}

fn note_sent_msg(state: &AppState, group_id: i64, send_msg_id: i64) {
    let mut status = GroupStatus::load(state, group_id);
    status.self_msg_ids.push(send_msg_id);
    if status.self_msg_ids.len() > 100 {
        let extra = status.self_msg_ids.len() - 100;
        status.self_msg_ids.drain(0..extra);
    }
    status.last_reply_time = Some(now_ts());
    status.save(state);
}

pub async fn run_loop(state: AppState) {
    let mut group_tx: HashMap<i64, mpsc::UnboundedSender<Message>> = HashMap::new();
    log::info("开始监听新消息");
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let msgs = match state.rpc.get_new_msgs().await {
            Ok(v) => v,
            Err(e) => {
                log::warning(format!("获取新消息失败: {e}"));
                continue;
            }
        };
        let arr = match msgs.as_array() {
            Some(a) => a.clone(),
            None => continue,
        };
        for v in arr {
            let Some(msg) = Message::from_json(&v, None) else {
                continue;
            };
            let group_id = msg.group_id;
            if let Some(tx) = group_tx.get(&group_id) {
                if tx.send(msg).is_err() {
                    group_tx.remove(&group_id);
                } else {
                    continue;
                }
            }
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(msg);
            group_tx.insert(group_id, tx);
            let st = state.clone();
            tokio::spawn(group_worker(st, group_id, rx));
        }
        group_tx.retain(|_, tx| !tx.is_closed());
    }
}

async fn group_worker(state: AppState, group_id: i64, mut rx: mpsc::UnboundedReceiver<Message>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(3 * 60 * 60), rx.recv()).await {
            Ok(Some(msg)) => {
                chat(&state, msg).await;
            }
            Ok(None) | Err(_) => {
                log::info(format!("群 {group_id} 闲置超时，Worker 退出"));
                break;
            }
        }
    }
}