use chrono::{DateTime, Duration, Local, TimeZone};

pub fn now_ts() -> f64 {
    let now = Local::now();
    now.timestamp() as f64 + f64::from(now.timestamp_subsec_nanos()) / 1e9
}

pub fn datetime_from_ts(ts: f64) -> DateTime<Local> {
    let secs = ts.trunc() as i64;
    let nsecs = ((ts.fract().abs()) * 1e9) as u32;
    Local
        .timestamp_opt(secs, nsecs)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(secs, 0).single().unwrap_or_else(Local::now))
}

pub fn get_readable_datetime(ts: f64, show_original_time: bool) -> String {
    let t = datetime_from_ts(ts);
    let now = Local::now();
    let mut diff = t.signed_duration_since(now);
    let mut suffix = "后";
    if diff.num_milliseconds() < 0 {
        suffix = "前";
        diff = -diff;
    }
    let total = diff.num_seconds();
    let text = if total < 60 {
        format!("{total}秒")
    } else if total < 60 * 60 {
        format!("{}分钟", total / 60)
    } else if total < 60 * 60 * 24 {
        format!("{}小时{}分钟", total / 3600, (total / 60) % 60)
    } else {
        format!("{}天", diff.num_days())
    };
    let text = format!("{text}{suffix}");
    if show_original_time {
        format!("{} ({text})", t.format("%Y-%m-%d %H:%M:%S"))
    } else {
        text
    }
}

#[allow(dead_code)]
pub fn get_readable_timedelta(delta: Duration) -> String {
    let mut s = delta.num_seconds();
    if s <= 0 {
        return "0秒".into();
    }
    let d = s / (24 * 3600);
    s %= 24 * 3600;
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    s %= 60;
    let mut ret = String::new();
    if d > 0 {
        ret.push_str(&format!("{d}天"));
    }
    if h > 0 && (true || ret.is_empty()) {
        ret.push_str(&format!("{h}小时"));
    }
    if m > 0 {
        ret.push_str(&format!("{m}分钟"));
    }
    if s > 0 && ret.is_empty() {
        ret.push_str(&format!("{s}秒"));
    }
    ret
}

pub fn truncate(s: &str, limit: i64) -> String {
    let limit = limit.max(0) as usize;
    let mut l = 0usize;
    for (i, c) in s.chars().enumerate() {
        if l >= limit {
            let prefix: String = s.chars().take(i).collect();
            return format!("{prefix}...");
        }
        l += if (c as u32) < 128 { 1 } else { 2 };
    }
    s.to_string()
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..a.len() {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

pub fn l2_sq_distance(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return f64::MAX;
    }
    let mut s = 0.0f64;
    for i in 0..a.len() {
        let d = a[i] as f64 - b[i] as f64;
        s += d * d;
    }
    s
}

pub fn json_f64_vec(v: &serde_json::Value) -> Option<Vec<f32>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        out.push(x.as_f64()? as f32);
    }
    Some(out)
}

pub fn f32_to_le_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn f32_from_le_bytes(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}