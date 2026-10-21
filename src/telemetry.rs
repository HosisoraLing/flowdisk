use crate::{ApiResult, App, Job, bandwidth::private_json, now};
use anyhow::Result;
use axum::{Json, Router, extract::State, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::sync::Mutex;
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Totals {
    received: u64,
    measured_received: u64,
    sent: u64,
    native_seen: BTreeMap<String, u64>,
    daily: BTreeMap<u64, [u64; 2]>,
    history: Vec<Value>,
    peak_download: u64,
    peak_upload: u64,
    download_active_ms: u64,
    upload_active_ms: u64,
}
pub struct Telemetry {
    totals: Mutex<Totals>,
    path: PathBuf,
}
impl Telemetry {
    pub fn new(data: &Path, jobs: &BTreeMap<uuid::Uuid, Job>) -> Result<Self> {
        let path = data.join("totals.json");
        let totals = match std::fs::read(&path) {
            Ok(v) => serde_json::from_slice(&v)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut t = Totals::default();
                for j in jobs.values() {
                    t.received = t.received.saturating_add(j.stats.transferred_bytes);
                    t.native_seen
                        .insert(j.id.to_string(), j.stats.transferred_bytes);
                }
                t
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            totals: Mutex::new(totals),
            path,
        })
    }
    pub async fn sample(&self, app: &App, elapsed_ms: u64) -> Result<()> {
        let jobs = app.0.jobs.lock().await;
        let mut t = self.totals.lock().await;
        let mut received = 0u64;
        for j in jobs.values() {
            let n = j.stats.transferred_bytes;
            let old = t.native_seen.insert(j.id.to_string(), n).unwrap_or(0);
            received = received.saturating_add(n.saturating_sub(old));
        }
        t.native_seen
            .retain(|id, _| jobs.keys().any(|k| k.to_string() == *id));
        let native_active = jobs.values().any(|j| j.status == "downloading");
        let speed = jobs.values().map(|j| j.speed).sum::<u64>();
        drop(jobs);
        let (down, up, sent, external_received) = app.0.transfers.deltas().await;
        received = received.saturating_add(external_received);
        t.received = t.received.saturating_add(received);
        t.measured_received = t.measured_received.saturating_add(received);
        t.sent = t.sent.saturating_add(sent);
        t.peak_download = t.peak_download.max(speed + down);
        t.peak_upload = t.peak_upload.max(up);
        if native_active || app.0.transfers.downloading() {
            t.download_active_ms = t.download_active_ms.saturating_add(elapsed_ms);
        }
        if up > 0 {
            t.upload_active_ms = t.upload_active_ms.saturating_add(elapsed_ms);
        }
        let day = (now() + 8 * 3600) / 86400;
        let bucket = t.daily.entry(day).or_default();
        bucket[0] = bucket[0].saturating_add(received);
        bucket[1] = bucket[1].saturating_add(sent);
        while t.daily.len() > 366 {
            let first = *t.daily.keys().next().unwrap();
            t.daily.remove(&first);
        }
        t.history.push(json!({"at_ms":now()*1000,"speed":speed+down,"upload_speed":up,"interval_ms":elapsed_ms}));
        if t.history.len() > 180 {
            t.history.remove(0);
        }
        private_json(&self.path, &*t).await?;
        Ok(())
    }
}
async fn statistics(State(app): State<App>) -> ApiResult<Json<Value>> {
    let native = app.0.jobs.lock().await;
    let external = app.0.transfers.public().await;
    let mut counts = BTreeMap::<String, u64>::new();
    let mut sources = BTreeMap::<String, u64>::new();
    let mut size = 0;
    let mut done = 0;
    let mut down = 0;
    for j in native.values() {
        *counts.entry(j.status.clone()).or_default() += 1;
        *sources
            .entry(j.path.split('/').nth(1).unwrap_or("cloud").into())
            .or_default() += 1;
        size += j.size.unwrap_or(0);
        done += j.downloaded;
        down += j.speed;
    }
    let mut up = 0;
    for j in external {
        *counts
            .entry(j["status"].as_str().unwrap_or("unknown").into())
            .or_default() += 1;
        *sources
            .entry(j["kind"].as_str().unwrap_or("link").into())
            .or_default() += 1;
        size += j["size"].as_u64().unwrap_or(0);
        done += j["downloaded"].as_u64().unwrap_or(0);
        down += j["speed"].as_u64().unwrap_or(0);
        up += j["upload_speed"].as_u64().unwrap_or(0);
    }
    drop(native);
    let t = app.0.telemetry.totals.lock().await;
    let mut disk = Value::Null;
    if let Ok(output) = tokio::process::Command::new("df")
        .arg("-Pk")
        .arg(&app.0.downloads)
        .output()
        .await
        && output.status.success()
        && let Some(line) = String::from_utf8_lossy(&output.stdout).lines().last()
    {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 4 {
            disk = json!({"total":fields[1].parse::<u64>().unwrap_or(0)*1024,"free":fields[3].parse::<u64>().unwrap_or(0)*1024});
        }
    }
    let points = t.history.clone();
    let peak = points
        .iter()
        .filter_map(|p| p["speed"].as_u64())
        .max()
        .unwrap_or(0);
    Ok(Json(
        json!({"download_speed":down,"upload_speed":up,"received_bytes":t.received,"uploaded_bytes":t.sent,"planned_bytes":size,"completed_bytes":done,"remaining_bytes":size.saturating_sub(done),"counts":counts,"sources":sources,"daily":t.daily.iter().map(|(day,v)|json!({"day":day,"received":v[0],"sent":v[1]})).collect::<Vec<_>>(),"history":points,"peak_recent_speed":peak,"peak_download_speed":t.peak_download,"peak_upload_speed":t.peak_upload,"average_download_speed":(t.measured_received as u128*1000/t.download_active_ms.max(1) as u128).min(u64::MAX as u128) as u64,"download_active_ms":t.download_active_ms,"disk":disk,"day_timezone":"UTC+08:00","totals_since":"首次启用统计；包括仍保留的旧任务传输量，历史已删除任务无法恢复"}),
    ))
}
pub fn routes() -> Router<App> {
    Router::new().route("/statistics", get(statistics))
}
