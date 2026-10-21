mod auth;
mod baidu;
mod control;
mod download;
mod external;
mod files;
mod gateway;
mod review;
mod serve_file;
mod thunder;
mod transfers;

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    fs,
    sync::{Mutex, Semaphore},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
struct App(Arc<Inner>);
struct Inner {
    gateway: gateway::Gateway,
    client: reqwest::Client,
    jobs: Mutex<BTreeMap<Uuid, Job>>,
    running: Mutex<BTreeMap<Uuid, Running>>,
    slots: Arc<Semaphore>,
    baidu_slots: Arc<Semaphore>,
    connections: Arc<Semaphore>,
    data: PathBuf,
    downloads: PathBuf,
    token: String,
    secure_cookie: bool,
    threads: usize,
    control: control::Control,
    transfers: transfers::Transfers,
}
struct Running {
    cancel: CancellationToken,
    progress: download::Progress,
    sampled_at: Option<Instant>,
    sampled_bytes: u64,
}
impl Running {
    fn sample(&mut self, job: &mut Job) {
        let Some(last) = self.sampled_at else {
            return;
        };
        self.sampled_at = Some(Instant::now());
        let elapsed = last.elapsed().as_millis() as u64;
        if elapsed == 0 {
            return;
        }
        let bytes = self.progress.transferred.load(Ordering::Relaxed);
        job.stats.record(
            bytes.saturating_sub(self.sampled_bytes),
            elapsed,
            now() * 1000,
        );
        self.sampled_bytes = bytes;
        job.speed = job.stats.speed_history.last().map_or(0, |p| p.speed);
        job.downloaded = self.progress.bytes.load(Ordering::Relaxed);
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SpeedStats {
    peak_speed: u64,
    average_speed: u64,
    transferred_bytes: u64,
    active_ms: u64,
    speed_history: Vec<SpeedPoint>,
}
#[derive(Clone, Serialize, Deserialize)]
struct SpeedPoint {
    at_ms: u64,
    speed: u64,
}
impl SpeedStats {
    fn record(&mut self, bytes: u64, elapsed_ms: u64, at_ms: u64) {
        if elapsed_ms == 0 {
            return;
        }
        self.active_ms = self.active_ms.saturating_add(elapsed_ms);
        self.transferred_bytes = self.transferred_bytes.saturating_add(bytes);
        let rate =
            |n: u64, ms: u64| ((n as u128 * 1000) / ms.max(1) as u128).min(u64::MAX as u128) as u64;
        let speed = rate(bytes, elapsed_ms);
        self.peak_speed = self.peak_speed.max(speed);
        self.average_speed = rate(self.transferred_bytes, self.active_ms);
        self.speed_history.push(SpeedPoint { at_ms, speed });
        if self.speed_history.len() > 180 {
            self.speed_history.remove(0);
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Job {
    id: Uuid,
    path: String,
    name: String,
    status: String,
    size: Option<u64>,
    downloaded: u64,
    speed: u64,
    connections: usize,
    error: Option<String>,
    sha256: Option<String>,
    created_at: u64,
    #[serde(default)]
    group_id: Option<Uuid>,
    #[serde(default)]
    group_name: Option<String>,
    #[serde(default)]
    passwords: Vec<String>,
    #[serde(default, flatten)]
    stats: SpeedStats,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self(StatusCode::BAD_GATEWAY, format!("{err:#}"))
    }
}
impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("本地文件操作失败：{err}"),
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;
fn bad(message: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

impl App {
    async fn save(&self, jobs: &BTreeMap<Uuid, Job>) -> Result<()> {
        let file = self.0.data.join("jobs.json.tmp");
        fs::write(&file, serde_json::to_vec_pretty(jobs)?).await?;
        fs::File::open(&file).await?.sync_all().await?;
        fs::rename(file, self.0.data.join("jobs.json")).await?;
        Ok(())
    }
    async fn start(&self, id: Uuid) -> ApiResult<()> {
        // Lock order is always running -> jobs. Keep the entry until the worker exits.
        let mut running = self.0.running.lock().await;
        let mut jobs = self.0.jobs.lock().await;
        let job = jobs
            .get_mut(&id)
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "任务不存在".into()))?;
        if running.contains_key(&id) {
            return Err(bad("任务正在运行或暂停中，请稍后再试"));
        }
        if job.status == "completed" {
            return Err(bad("任务已完成"));
        }
        job.status = "queued".into();
        job.error = None;
        job.speed = 0;
        let downloaded = job.downloaded;
        self.save(&jobs).await?;
        let cancel = CancellationToken::new();
        let progress = download::Progress {
            bytes: Arc::new(AtomicU64::new(downloaded)),
            transferred: Arc::new(AtomicU64::new(0)),
        };
        running.insert(
            id,
            Running {
                cancel: cancel.clone(),
                progress: progress.clone(),
                sampled_at: None,
                sampled_bytes: 0,
            },
        );
        let app = self.clone();
        tokio::spawn(async move {
            let result = async {
                let native_baidu = {
                    let jobs=app.0.jobs.lock().await;
                    jobs.get(&id).is_some_and(|j|baidu::owns(&j.path)) && app.0.gateway.baidu.connected().await
                };
                let _baidu_slot = if native_baidu {
                    Some(tokio::select! {_ = cancel.cancelled() => bail!("已暂停"),p=app.0.baidu_slots.clone().acquire_owned()=>p?})
                } else {None};
                let permit = tokio::select! {
                    _ = cancel.cancelled() => bail!("已暂停"),
                    p = app.0.slots.clone().acquire_owned() => p?,
                };
                let _permit = permit;
                {
                    let mut running = app.0.running.lock().await;
                    if let Some(runtime) = running.get_mut(&id) {
                        runtime.sampled_at = Some(Instant::now());
                    }
                    let mut jobs = app.0.jobs.lock().await;
                    jobs.get_mut(&id).context("任务不存在")?.status = "downloading".into();
                }
                download::run(&app, id, &cancel, progress.clone()).await
            }
            .await;
            let mut running = app.0.running.lock().await;
            let mut jobs = app.0.jobs.lock().await;
            if let Some(job) = jobs.get_mut(&id) {
                if let Some(runtime) = running.get_mut(&id) {
                    runtime.sample(job);
                }
                job.downloaded = progress.bytes.load(Ordering::Relaxed);
                job.speed = 0;
                job.connections = 0;
                match result {
                    Ok(hash) => {
                        job.status = "completed".into();
                        job.sha256 = Some(hash);
                    }
                    Err(_) if cancel.is_cancelled() => {
                        job.status = "paused".into();
                    }
                    Err(err) => {
                        job.status = "failed".into();
                        job.error = Some(format!("{err:#}"));
                    }
                }
            }
            if let Err(err) = app.save(&jobs).await {
                eprintln!("保存任务状态失败: {err}");
            }
            running.remove(&id);
        });
        Ok(())
    }
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 4096
        && !path.contains(['\0', '\\'])
        && !path.split('/').any(|s| s == "." || s == "..")
}
fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .scan(0, |bytes, c| {
            *bytes += c.len_utf8();
            (*bytes <= 200).then_some(c)
        })
        .collect();
    let name = name.trim_matches([' ', '.']);
    if name.is_empty() {
        "download.bin".into()
    } else {
        name.into()
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn number(key: &str, default: usize, max: usize) -> Result<usize> {
    let n = env::var(key)
        .ok()
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(default);
    ensure!((1..=max).contains(&n), "{key} 必须介于 1 与 {max} 之间");
    Ok(n)
}

async fn auth(State(app): State<App>, request: axum::extract::Request, next: Next) -> Response {
    let headers = request.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';')
                .find_map(|c| c.trim().strip_prefix("flowdisk_session="))
        });
    let supplied = bearer.or(cookie).unwrap_or("");
    if !same_secret(supplied.as_bytes(), app.0.token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"请先登录"}))).into_response();
    }
    next.run(request).await
}
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(a);
    let b = Sha256::digest(b);
    a.iter().zip(b).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}
#[derive(Deserialize)]
struct Login {
    token: String,
}
async fn session(State(app): State<App>, Json(login): Json<Login>) -> ApiResult<Response> {
    if !same_secret(login.token.as_bytes(), app.0.token.as_bytes()) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "访问密钥不正确".into()));
    }
    let secure = if app.0.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "flowdisk_session={}; HttpOnly; SameSite=Strict; Path=/api; Max-Age=86400{secure}",
        app.0.token
    );
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"ok":true}))).into_response())
}
async fn logout() -> impl IntoResponse {
    (
        [(
            header::SET_COOKIE,
            "flowdisk_session=; HttpOnly; SameSite=Strict; Path=/api; Max-Age=0",
        )],
        Json(json!({"ok":true})),
    )
}
#[derive(Deserialize)]
struct Browse {
    #[serde(default = "root")]
    path: String,
    #[serde(default = "one")]
    page: u64,
    #[serde(default)]
    refresh: bool,
}
fn root() -> String {
    "/".into()
}
fn one() -> u64 {
    1
}
async fn browse(State(app): State<App>, Query(q): Query<Browse>) -> ApiResult<Json<Value>> {
    if !valid_path(&q.path) || q.page == 0 || q.page > 1_000_000 {
        return Err(bad("目录或页码不合法"));
    }
    Ok(Json(app.0.gateway.post("/api/fs/list", json!({"path":q.path,"password":"","page":q.page,"per_page":100,"refresh":q.refresh})).await?))
}
async fn jobs(State(app): State<App>) -> Json<Vec<Job>> {
    let mut jobs: Vec<_> = app.0.jobs.lock().await.values().cloned().collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.created_at));
    Json(jobs)
}
#[derive(Deserialize)]
struct NewJob {
    path: String,
}
async fn create(State(app): State<App>, Json(new): Json<NewJob>) -> ApiResult<Json<Value>> {
    if !valid_path(&new.path) {
        return Err(bad("文件路径不合法"));
    }
    let meta = app.0.gateway.metadata(&new.path).await?;
    if meta.is_dir {
        let ids = control::enqueue_paths(&app, vec![new.path], None).await?;
        return Ok(Json(json!({"jobs":ids})));
    }
    let job = Job {
        id: Uuid::new_v4(),
        path: new.path,
        name: safe_name(&meta.name),
        status: "paused".into(),
        size: Some(meta.size),
        downloaded: 0,
        speed: 0,
        connections: 0,
        error: None,
        sha256: None,
        created_at: now(),
        group_id: None,
        group_name: None,
        passwords: vec![],
        stats: SpeedStats::default(),
    };
    {
        let mut jobs = app.0.jobs.lock().await;
        if jobs.len() >= 1000 {
            return Err(bad("最多保留 1000 个任务，请清理已完成任务"));
        }
        if jobs.values().any(|j| {
            j.path == job.path && matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
        }) {
            return Err(bad("该文件已有下载任务"));
        }
        jobs.insert(job.id, job.clone());
        app.save(&jobs).await?;
    }
    app.start(job.id).await?;
    Ok(Json(serde_json::to_value(job).unwrap()))
}
async fn pause(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let running = app.0.running.lock().await;
    let mut jobs = app.0.jobs.lock().await;
    let job = jobs
        .get_mut(&id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "任务不存在".into()))?;
    if let Some(run) = running.get(&id) {
        job.status = "pausing".into();
        run.cancel.cancel();
    }
    app.save(&jobs).await?;
    Ok(Json(json!({"ok":true})))
}
async fn resume(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    app.start(id).await?;
    Ok(Json(json!({"ok":true})))
}
async fn remove(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let running = app.0.running.lock().await;
    if running.contains_key(&id) {
        return Err(bad("请先暂停任务，等待暂停完成后再移除"));
    }
    let mut jobs = app.0.jobs.lock().await;
    let job = jobs
        .get(&id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "任务不存在".into()))?;
    // Completed downloads remain on disk when removing the task history.
    if job.status != "completed" {
        let parts = app.0.data.join("parts").join(id.to_string());
        if fs::try_exists(&parts).await? {
            fs::remove_dir_all(parts).await?;
        }
        let output = app.0.downloads.join(id.to_string());
        if fs::try_exists(&output).await? {
            fs::remove_dir_all(output).await?;
        }
    }
    jobs.remove(&id);
    app.save(&jobs).await?;
    Ok(Json(json!({"ok":true})))
}
async fn file(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    method: axum::http::Method,
) -> ApiResult<Response> {
    let job = app
        .0
        .jobs
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "任务不存在".into()))?;
    if job.status != "completed" {
        return Err(bad("文件尚未完成"));
    }
    let path = app.0.downloads.join(id.to_string()).join(&job.name);
    serve_file::send(
        &path,
        &job.name,
        &headers,
        method == axum::http::Method::HEAD,
    )
    .await
}
async fn status(State(app): State<App>) -> Json<Value> {
    let gateway = app
        .0
        .gateway
        .post(
            "/api/fs/list",
            json!({"path":"/","page":1,"per_page":1,"password":"","refresh":false}),
        )
        .await;
    Json(
        json!({"gateway_connected":gateway.is_ok(),"gateway_error":gateway.err().map(|e|format!("{e:#}")),"threads":app.0.threads,"download_location":"容器 /downloads（宿主机 ./downloads）","providers":["百度网盘","夸克","115","迅雷云盘"]}),
    )
}
async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; frame-src https://uop.quark.cn https://i.xunlei.com https://captcha.xunlei.com; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}

#[tokio::main]
async fn main() -> Result<()> {
    let token = env::var("APP_TOKEN")
        .context("必须设置 APP_TOKEN（至少 24 位字母、数字、下划线或横线）")?;
    ensure!(
        token.len() >= 24
            && token.len() <= 256
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "APP_TOKEN 格式不合法"
    );
    let data = PathBuf::from(env::var("DATA_DIR").unwrap_or_else(|_| "data".into()));
    let downloads = PathBuf::from(env::var("DOWNLOAD_DIR").unwrap_or_else(|_| "downloads".into()));
    fs::create_dir_all(&data).await?;
    fs::create_dir_all(&downloads).await?;
    let jobs_file = data.join("jobs.json");
    let mut saved: BTreeMap<Uuid, Job> = if fs::try_exists(&jobs_file).await? {
        serde_json::from_slice(&fs::read(&jobs_file).await?)
            .context("任务记录损坏，拒绝覆盖；请检查 jobs.json")?
    } else {
        BTreeMap::new()
    };
    for job in saved.values_mut() {
        ensure!(safe_name(&job.name) == job.name, "任务记录文件名不合法");
        job.speed = 0;
        job.connections = 0;
        if matches!(job.status.as_str(), "queued" | "downloading" | "pausing") {
            job.status = "paused".into();
        }
    }
    let threads = number("CONNECTIONS_PER_FILE", 8, 32)?;
    let gateway = gateway::Gateway::new(&data)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(40))
        .pool_max_idle_per_host(32)
        .tcp_nodelay(true)
        .user_agent(gateway::USER_AGENT)
        .build()?;
    let control = control::Control::new(&data, &token).await?;
    for (id, share) in control.shares.lock().await.iter() {
        for job in saved
            .values_mut()
            .filter(|j| share.jobs.contains(&j.id) && j.group_id.is_none())
        {
            job.group_id = Some(*id);
            job.group_name = Some(format!(
                "{} · 分享下载",
                control::PROVIDERS
                    .iter()
                    .find(|(key, _)| *key == share.provider)
                    .map(|(_, name)| *name)
                    .unwrap_or("网盘")
            ));
        }
    }
    let transfers = transfers::Transfers::new(&data, &downloads, &token).await?;
    let app = App(Arc::new(Inner {
        gateway,
        client,
        jobs: Mutex::new(saved),
        running: Mutex::new(BTreeMap::new()),
        slots: Arc::new(Semaphore::new(number("ACTIVE_DOWNLOADS", 2, 8)?)),
        connections: Arc::new(Semaphore::new(number("GLOBAL_CONNECTIONS", 24, 64)?)),
        baidu_slots: Arc::new(Semaphore::new(1)),
        data,
        downloads,
        token,
        secure_cookie: env::var("COOKIE_SECURE").is_ok_and(|v| v == "true"),
        threads,
        control,
        transfers,
    }));
    app.save(&*app.0.jobs.lock().await).await?;
    let monitor = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_save = Instant::now();
        loop {
            tick.tick().await;
            let mut running = monitor.0.running.lock().await;
            let mut jobs = monitor.0.jobs.lock().await;
            for (id, runtime) in running.iter_mut() {
                if let Some(job) = jobs.get_mut(id)
                    && job.status != "queued"
                {
                    runtime.sample(job);
                }
            }
            if !running.is_empty() && last_save.elapsed() >= Duration::from_secs(5) {
                if let Err(error) = monitor.save(&jobs).await {
                    eprintln!("保存速度统计失败: {error}");
                }
                last_save = Instant::now();
            }
        }
    });
    let transfer_worker = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last = Instant::now();
        loop {
            tick.tick().await;
            let elapsed = last.elapsed().as_millis() as u64;
            last = Instant::now();
            if let Err(_error) = transfer_worker.0.transfers.poll(elapsed).await {
                eprintln!("链接任务状态暂未同步");
            }
        }
    });
    let share_worker = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            control::spawn_pending(&share_worker, None).await;
        }
    });
    let api = Router::new()
        .merge(control::routes())
        .merge(files::routes())
        .merge(review::routes())
        .merge(transfers::routes())
        .route("/status", get(status))
        .route("/browse", get(browse))
        .route("/jobs", get(jobs).post(create))
        .route("/jobs/{id}", axum::routing::delete(remove))
        .route("/jobs/{id}/pause", post(pause))
        .route("/jobs/{id}/resume", post(resume))
        .route("/jobs/{id}/file", get(file))
        .route("/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    let router = Router::new()
        .nest("/api", api)
        .route("/api/session", post(session))
        .route("/oauth/baidu/callback", get(auth::baidu_callback))
        .route("/oauth/{provider}/callback", get(auth::external_callback))
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/review.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/review.js"),
                )
            }),
        )
        .route(
            "/tools.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/tools.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(app.clone());
    let address = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("FlowDisk 已启动: {address}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            app.0.transfers.shutdown().await;
            for running in app.0.running.lock().await.values() {
                running.cancel.cancel();
            }
            loop {
                if app.0.running.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_path_traversal_and_unsafe_names() {
        assert!(!valid_path("/baidu/../secret"));
        assert!(!valid_path("/115/a\\b"));
        assert!(valid_path("/夸克/资料/a.mp4"));
        assert_eq!(safe_name("../../bad\n"), "_.._bad_");
        assert_eq!(safe_name(".."), "download.bin");
        assert!(safe_name(&"资料😀".repeat(100)).len() <= 200);
        assert!(!same_secret(b"short", b"long-secret"));
        assert!(same_secret(b"abc", b"abc"));
    }
}

#[cfg(test)]
mod speed_tests {
    use super::*;
    #[test]
    fn speed_tracks_payload_and_active_time_and_keeps_old_jobs_readable() {
        let mut stats = SpeedStats::default();
        stats.record(1000, 1000, 1000);
        stats.record(3000, 1000, 2000);
        // A pause adds no active time. The next sample can have a much later timestamp.
        stats.record(1000, 1000, 102000);
        assert_eq!(stats.peak_speed, 3000);
        assert_eq!(stats.average_speed, 1666);
        assert_eq!(stats.active_ms, 3000);
        for n in 0..200 {
            stats.record(0, 1000, 103000 + n * 1000);
        }
        assert_eq!(stats.speed_history.len(), 180);
        assert_eq!(stats.peak_speed, 3000);
        let value = json!({"id":Uuid::new_v4(),"path":"/baidu/a","name":"a","status":"paused","size":9000000,"downloaded":8000000,"speed":0,"connections":0,"error":null,"sha256":null,"created_at":0});
        let mut job: Job = serde_json::from_value(value).unwrap();
        assert!(job.stats.speed_history.is_empty());
        let mut runtime = Running {
            cancel: CancellationToken::new(),
            progress: download::Progress {
                bytes: Arc::new(AtomicU64::new(8000000)),
                transferred: Arc::new(AtomicU64::new(0)),
            },
            sampled_at: Some(Instant::now() - Duration::from_secs(1)),
            sampled_bytes: 0,
        };
        let mut queued = Running {
            cancel: CancellationToken::new(),
            progress: runtime.progress.clone(),
            sampled_at: None,
            sampled_bytes: 0,
        };
        queued.sample(&mut job);
        assert!(queued.sampled_at.is_none());
        assert_eq!(job.stats.active_ms, 0);
        runtime.sample(&mut job);
        assert_eq!(job.speed, 0); // Resumed bytes on disk must not become a speed spike.
        assert_eq!(job.stats.transferred_bytes, 0);
        assert_eq!(job.downloaded, 8000000);
    }
}
