use crate::{ApiResult, App, bad};
use anyhow::Result;
use axum::{Json, Router, extract::State, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
use tokio::{fs, sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_limit: u64,
    pub upload_limit: u64,
    pub seed_ratio: f64,
    pub seed_minutes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            download_limit: 0,
            upload_limit: 0,
            seed_ratio: 1.,
            seed_minutes: 60,
        }
    }
}
pub struct Bandwidth {
    pub settings: Mutex<Settings>,
    path: PathBuf,
    pub native_active: AtomicBool,
    rate: AtomicU64,
    pacing: Mutex<Instant>,
    budget_lock: Mutex<()>,
    last_budget: Mutex<Option<(u64, u64)>>,
}
pub async fn private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("tmp");
    let bytes = serde_json::to_vec(value)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    use tokio::io::AsyncWriteExt;
    let mut file = options.open(&temp).await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    fs::rename(temp, path).await?;
    Ok(())
}
impl Bandwidth {
    pub fn new(data: &Path) -> Result<Self> {
        let settings = match std::fs::read(data.join("settings.json")) {
            Ok(v) => serde_json::from_slice(&v)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            settings: Mutex::new(settings),
            path: data.join("settings.json"),
            native_active: AtomicBool::new(false),
            rate: AtomicU64::new(0),
            pacing: Mutex::new(Instant::now()),
            budget_lock: Mutex::new(()),
            last_budget: Mutex::new(None),
        })
    }
    // One shared pacer covers every native connection. Sleep while holding the
    // pacer so cancelling a worker cannot reserve future bandwidth for minutes.
    pub async fn wait(&self, bytes: usize, cancel: &CancellationToken) -> Result<()> {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return Ok(());
        }
        let mut last = tokio::select! {_ =cancel.cancelled()=>anyhow::bail!("已暂停"),lock=self.pacing.lock()=>lock};
        let delay = Duration::from_secs_f64(bytes as f64 / rate as f64);
        let until = (*last).max(Instant::now()) + delay;
        tokio::select! {_ =cancel.cancelled()=>anyhow::bail!("已暂停"),_=tokio::time::sleep_until(until)=>{}}
        *last = Instant::now();
        Ok(())
    }
    pub async fn budget(&self, app: &App, _native: bool) -> Result<()> {
        let _lock = self.budget_lock.lock().await;
        let settings = self.settings.lock().await.clone();
        let native = self.native_active.load(Ordering::Relaxed);
        let external = app.0.transfers.downloading();
        let total = settings.download_limit;
        let native_rate = if total > 0 && native && external {
            (total / 2).max(1)
        } else {
            total
        };
        let external_rate = if total > 0 && native && external {
            total.saturating_sub(native_rate).max(1)
        } else {
            total
        };
        // Set the engine budget before allowing a newly queued native worker.
        let mut last = self.last_budget.lock().await;
        if app.0.transfers.available() && *last != Some((external_rate, settings.upload_limit)) {
            app.0.transfers.rpc("changeGlobalOption",json!([{"max-overall-download-limit":external_rate.to_string(),"max-overall-upload-limit":settings.upload_limit.to_string()}])).await?;
            *last = Some((external_rate, settings.upload_limit));
        }
        self.rate.store(native_rate, Ordering::Relaxed);
        Ok(())
    }
}
async fn settings(State(app): State<App>) -> Json<Value> {
    let mut v = serde_json::to_value(app.0.bandwidth.settings.lock().await.clone()).unwrap();
    v["engine_available"] = app.0.transfers.available().into();
    v["protocols"] = app.0.transfers.protocols().into();
    Json(v)
}
async fn save_settings(
    State(app): State<App>,
    Json(value): Json<Settings>,
) -> ApiResult<Json<Value>> {
    for limit in [value.download_limit, value.upload_limit] {
        if limit > 0 && !(1024..=100 * 1024 * 1024 * 1024).contains(&limit) {
            return Err(bad("限速至少 1 KiB/s，0 表示不限速"));
        }
    }
    if !value.seed_ratio.is_finite()
        || !(0.0..=10000.).contains(&value.seed_ratio)
        || value.seed_minutes > 525600
    {
        return Err(bad("做种比例或时长无效"));
    }
    {
        let mut settings = app.0.bandwidth.settings.lock().await;
        private_json(&app.0.bandwidth.path, &value).await?;
        *settings = value.clone();
    }
    let native = app.0.bandwidth.native_active.load(Ordering::Relaxed);
    app.0.bandwidth.budget(&app, native).await?;
    app.0
        .transfers
        .seed_options(value.seed_ratio, value.seed_minutes)
        .await?;
    Ok(Json(serde_json::to_value(value).unwrap()))
}
pub fn routes() -> Router<App> {
    Router::new().route("/settings", get(settings).put(save_settings))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shared_pacer_is_bounded_and_cancellable() {
        let data = std::env::temp_dir().join(format!("flowdisk-limit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&data).unwrap();
        let t = Bandwidth::new(&data).unwrap();
        t.rate.store(128 * 1024, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let start = Instant::now();
        t.wait(16 * 1024, &cancel).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(115));
        cancel.cancel();
        assert!(t.wait(16 * 1024, &cancel).await.is_err());
        std::fs::remove_dir_all(data).unwrap();
    }
}
