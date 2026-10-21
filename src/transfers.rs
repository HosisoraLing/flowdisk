//! An isolated aria2 subprocess handles ordinary link downloads. Its RPC never
//! listens outside loopback; only typed, authenticated FlowDisk routes reach it.
use crate::{ApiError, ApiResult, App, SpeedStats, bad, now, safe_name};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::{get, post},
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tokio::{
    fs,
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct TransferFile {
    pub index: u64,
    pub name: String,
    pub length: u64,
    pub completed: u64,
    pub selected: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Transfer {
    id: Uuid,
    gid: String,
    uri: String,
    headers: BTreeMap<String, String>,
    pub kind: String,
    pub name: String,
    status: String,
    pub files: Vec<TransferFile>,
    group_id: Uuid,
    group_name: String,
    created_at: u64,
    error: Option<String>,
    downloaded: u64,
    size: u64,
    upload_speed: u64,
    uploaded: u64,
    peak_upload_speed: u64,
    connections: u64,
    last_received: u64,
    last_sent: u64,
    #[serde(default)]
    hidden: bool,
    #[serde(flatten)]
    stats: SpeedStats,
}
impl Transfer {
    fn public(&self) -> Value {
        json!({"id":self.id,"engine":"aria2","kind":self.kind,"name":self.name,"path":display_uri(&self.uri),"status":self.status,"size":self.size,"downloaded":self.downloaded,"speed":if matches!(self.status.as_str(),"downloading"|"seeding"){self.stats.speed_history.last().map_or(0,|p|p.speed)}else{0},"upload_speed":self.upload_speed,"uploaded":self.uploaded,"peak_upload_speed":self.peak_upload_speed,"share_ratio":if self.downloaded>0{self.uploaded as f64/self.downloaded as f64}else{0.},"connections":self.connections,"group_id":self.group_id,"group_name":self.group_name,"created_at":self.created_at,"error":self.error,"files":self.files,"peak_speed":self.stats.peak_speed,"average_speed":self.stats.average_speed,"active_ms":self.stats.active_ms,"transferred_bytes":self.stats.transferred_bytes,"speed_history":self.stats.speed_history})
    }
}
pub struct Transfers {
    client: Client,
    url: Url,
    secret: String,
    pub tasks: Mutex<BTreeMap<Uuid, Transfer>>,
    child: Mutex<Option<Child>>,
    path: PathBuf,
    downloads: PathBuf,
    available: AtomicBool,
    protocol_list: Vec<String>,
    downloading: AtomicBool,
    pub received_delta: AtomicU64,
    pub sent_delta: AtomicU64,
}
fn display_uri(raw: &str) -> String {
    if let Ok(mut url) = Url::parse(raw) {
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.to_string()
    } else {
        "本地元数据文件".into()
    }
}
fn link(raw: &str) -> Result<Url> {
    let u = Url::parse(raw)?;
    ensure!(
        ["http", "https", "ftp", "sftp"].contains(&u.scheme())
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && raw.len() <= 16384
            && !raw.contains(['\r', '\n']),
        "请输入 HTTP、HTTPS、FTP 或 SFTP 链接，凭据请勿放在 URL 中"
    );
    Ok(u)
}
fn number(v: &Value, key: &str) -> u64 {
    v[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v[key].as_u64())
        .unwrap_or(0)
}
async fn save(path: &FsPath, value: &impl Serialize) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let temp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut f = options.open(&temp).await?;
    f.write_all(&serde_json::to_vec(value)?).await?;
    f.sync_all().await?;
    fs::rename(temp, path).await?;
    Ok(())
}
impl Transfers {
    pub async fn new(data: &FsPath, downloads: &FsPath, token: &str) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let secret = format!("{:x}", Sha256::digest(format!("flowdisk-aria2:{token}")));
        let path = data.join("transfers.json");
        let mut tasks: BTreeMap<Uuid, Transfer> = match fs::read(&path).await {
            Ok(v) => serde_json::from_slice(&v)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        for t in tasks.values_mut() {
            if !matches!(t.status.as_str(), "completed" | "failed" | "selecting") {
                t.status = "paused".into();
            }
            t.upload_speed = 0;
        }
        fs::create_dir_all(downloads.join("transfers")).await?;
        let data = fs::canonicalize(data).await?;
        let downloads = fs::canonicalize(downloads).await?.join("transfers");
        let port = std::env::var("ARIA2_RPC_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(6800);
        let mut engine = Self {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()?,
            url: Url::parse(&format!("http://127.0.0.1:{port}/jsonrpc"))?,
            secret: secret.clone(),
            tasks: Mutex::new(tasks),
            child: Mutex::new(None),
            path,
            downloads,
            available: AtomicBool::new(false),
            protocol_list: vec!["http".into(), "https".into(), "ftp".into()],
            downloading: AtomicBool::new(false),
            received_delta: AtomicU64::new(0),
            sent_delta: AtomicU64::new(0),
        };
        if std::env::var("TRANSFER_ENGINE").is_ok_and(|s| s == "disabled") {
            return Ok(engine);
        }
        let session = data.join("aria2.session");
        if !fs::try_exists(&session).await? {
            fs::write(&session, b"").await?;
        }
        let config = data.join("aria2.conf");
        let bt_port = std::env::var("BT_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(6888);
        let contents = format!(
            "enable-rpc=true\nrpc-listen-all=false\nrpc-listen-port={port}\nrpc-secret={secret}\nrpc-save-upload-metadata=true\nenable-console-readout=false\nquiet=true\nconsole-log-level=error\nlog-level=error\nfile-allocation=none\ncontinue=true\nauto-file-renaming=false\nallow-overwrite=false\nsplit=8\nmax-connection-per-server=8\nmin-split-size=4M\nmax-concurrent-downloads=3\nmax-download-result=2000\nmax-tries=5\nretry-wait=2\nconnect-timeout=15\ntimeout=30\ncheck-certificate=true\nno-netrc=true\npause=true\npause-metadata=true\nfollow-torrent=false\nfollow-metalink=false\nseed-ratio=1.0\nseed-time=60\nlisten-port={bt_port}\ndht-listen-port={bt_port}\ndht-file-path={}\ndht-file-path6={}\ninput-file={}\nsave-session={}\nsave-session-interval=5\nauto-save-interval=5\nforce-save=true\ndir={}\n",
            data.join("dht.dat").display(),
            data.join("dht6.dat").display(),
            session.display(),
            session.display(),
            engine.downloads.display()
        );
        fs::write(&config, contents).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).await?;
            fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600)).await?;
        }
        match Command::new(std::env::var("ARIA2_BIN").unwrap_or_else(|_| "aria2c".into()))
            .arg(format!("--conf-path={}", config.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => {
                *engine.child.lock().await = Some(child);
                for _ in 0..30 {
                    if let Ok(v) = engine.rpc("getVersion", json!([])).await {
                        engine.available.store(true, Ordering::Relaxed);
                        if v["enabledFeatures"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|f| f == "SFTP"))
                        {
                            engine.protocol_list.push("sftp".into());
                        }
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            Err(_) => {
                eprintln!("普通链接下载引擎不可用，请使用含 aria2 的 Docker 镜像；网盘功能仍可用")
            }
        }
        Ok(engine)
    }
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
    pub fn protocols(&self) -> Vec<String> {
        self.protocol_list.clone()
    }
    pub fn downloading(&self) -> bool {
        self.downloading.load(Ordering::Relaxed)
    }
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let mut p = vec![json!(format!("token:{}", self.secret))];
        p.extend(params.as_array().context("RPC 参数无效")?.clone());
        let resp=self.client.post(self.url.clone()).json(&json!({"jsonrpc":"2.0","id":"flowdisk","method":format!("aria2.{method}"),"params":p})).send().await.map_err(|_|anyhow::anyhow!("下载引擎暂不可用，请检查容器状态"))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("下载引擎响应无效"))?;
        if v["error"].is_object() {
            bail!(
                "下载引擎拒绝操作（代码 {}），请检查链接、种子或任务状态",
                v["error"]["code"].as_i64().unwrap_or(-1)
            );
        }
        v.get("result").cloned().context("引擎未返回结果")
    }
    async fn persist(&self, tasks: &BTreeMap<Uuid, Transfer>) -> Result<()> {
        save(&self.path, tasks).await
    }
    pub async fn public(&self) -> Vec<Value> {
        let tasks = self.tasks.lock().await;
        let mut v: Vec<_> = tasks
            .values()
            .filter(|t| !t.hidden)
            .map(Transfer::public)
            .collect();
        v.sort_by_key(|t| std::cmp::Reverse(t["created_at"].as_u64()));
        v
    }
    pub async fn deltas(&self) -> (u64, u64, u64, u64) {
        let tasks = self.tasks.lock().await;
        let down = tasks
            .values()
            .filter(|t| t.status == "downloading")
            .map(|t| t.stats.speed_history.last().map_or(0, |p| p.speed))
            .sum();
        let up = tasks.values().map(|t| t.upload_speed).sum();
        (
            down,
            up,
            self.sent_delta.swap(0, Ordering::Relaxed),
            self.received_delta.swap(0, Ordering::Relaxed),
        )
    }
    pub async fn seed_options(&self, ratio: f64, minutes: u64) -> Result<()> {
        if !self.available() {
            return Ok(());
        }
        let options = json!({"seed-ratio":ratio.to_string(),"seed-time":minutes.to_string()});
        self.rpc("changeGlobalOption", json!([options])).await?;
        let tasks = self.tasks.lock().await;
        for task in tasks
            .values()
            .filter(|t| matches!(t.status.as_str(), "downloading" | "seeding" | "paused"))
        {
            let _ = self.rpc("changeOption", json!([task.gid, options])).await;
        }
        Ok(())
    }
    pub async fn poll(&self, elapsed_ms: u64) -> Result<()> {
        if !self.available() {
            return Ok(());
        }
        let mut tasks = self.tasks.lock().await;
        let mut downloading = false;
        for t in tasks.values_mut().filter(|t| !t.hidden) {
            let Ok(v) = self.rpc("tellStatus", json!([t.gid])).await else {
                continue;
            };
            let status = v["status"].as_str().unwrap_or("paused");
            let total = number(&v, "totalLength");
            let completed = number(&v, "completedLength");
            t.status = match status {
                "active" => "downloading",
                "waiting" => "queued",
                "complete" => "completed",
                "error" | "removed" => "failed",
                _ => "paused",
            }
            .into();
            downloading |= matches!(t.status.as_str(), "downloading" | "queued");
            let received = completed.saturating_sub(t.last_received);
            t.last_received = completed;
            let sent = number(&v, "uploadLength");
            let delta_sent = if sent >= t.last_sent {
                sent - t.last_sent
            } else {
                sent
            };
            t.last_sent = sent;
            t.uploaded = t.uploaded.saturating_add(delta_sent);
            self.received_delta.fetch_add(received, Ordering::Relaxed);
            self.sent_delta.fetch_add(delta_sent, Ordering::Relaxed);
            if status == "active" {
                t.stats.record(received, elapsed_ms, now() * 1000);
            }
            t.upload_speed = number(&v, "uploadSpeed");
            t.peak_upload_speed = t.peak_upload_speed.max(t.upload_speed);
            t.connections = number(&v, "connections");
            t.size = total;
            t.downloaded = completed;
            t.error = if status == "error" {
                Some(format!(
                    "引擎下载错误 {}，可暂停后重试；请检查链接权限、磁盘及服务器网络",
                    v["errorCode"].as_str().unwrap_or("未知")
                ))
            } else {
                None
            };
            if let Some(files) = v["files"].as_array() {
                let root = self.downloads.join(t.id.to_string());
                let mut list = vec![];
                for f in files {
                    let full = PathBuf::from(f["path"].as_str().unwrap_or(""));
                    if let Ok(relative) = full.strip_prefix(&root) {
                        let name = relative.to_string_lossy().to_string();
                        if !name.is_empty()
                            && !relative.components().any(|p| {
                                matches!(
                                    p,
                                    std::path::Component::ParentDir
                                        | std::path::Component::RootDir
                                        | std::path::Component::Prefix(_)
                                )
                            })
                        {
                            list.push(TransferFile {
                                index: number(f, "index"),
                                name,
                                length: number(f, "length"),
                                completed: number(f, "completedLength"),
                                selected: f["selected"] == "true",
                            });
                        }
                    }
                }
                t.files = list;
                if t.files.len() == 1 {
                    t.name = t.files[0].name.clone();
                }
            }
        }
        self.downloading.store(downloading, Ordering::Relaxed);
        self.persist(&tasks).await
    }
    pub async fn shutdown(&self) {
        if self.available() {
            let _ = self.rpc("forcePauseAll", json!([])).await;
            let _ = self.poll(0).await;
            let _ = self.rpc("saveSession", json!([])).await;
            let _ = self.rpc("shutdown", json!([])).await;
        }
        if let Some(mut child) = self.child.lock().await.take() {
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
}
#[derive(Deserialize)]
struct Input {
    uri: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    name: String,
    group_id: Option<Uuid>,
    group_name: Option<String>,
}
async fn create(State(app): State<App>, Json(input): Json<Input>) -> ApiResult<Json<Value>> {
    if !app.0.transfers.available() {
        return Err(bad("链接下载引擎不可用，请使用含 aria2 的 Docker 镜像"));
    }
    let url = link(input.uri.trim())
        .map_err(|_| bad("不支持此下载链接，请填写 HTTP、HTTPS、FTP 或 SFTP 地址"))?;
    if !app
        .0
        .transfers
        .protocols()
        .iter()
        .any(|p| p == url.scheme())
    {
        return Err(bad("当前引擎未编译这个协议"));
    }
    if input.headers.len() > 4
        || input.headers.iter().any(|(k, v)| {
            !["user-agent", "referer", "authorization", "cookie"]
                .contains(&k.to_ascii_lowercase().as_str())
                || v.len() > 8192
                || v.contains(['\r', '\n'])
        })
    {
        return Err(bad(
            "仅支持 User-Agent、Referer、Authorization 和 Cookie 请求头，且不能包含换行",
        ));
    }
    let id = Uuid::new_v4();
    let gid = id.simple().to_string()[..16].to_owned();
    let name = if input.name.trim().is_empty() {
        safe_name(
            url.path_segments()
                .and_then(|mut p| p.next_back())
                .filter(|p| !p.is_empty())
                .unwrap_or("download.bin"),
        )
    } else {
        safe_name(&input.name)
    };
    let group_id = input.group_id.unwrap_or_else(Uuid::new_v4);
    let group_name = input
        .group_name
        .filter(|s| s.len() <= 200)
        .unwrap_or_else(|| "链接下载".into());
    let task = Transfer {
        id,
        gid: gid.clone(),
        uri: input.uri.trim().into(),
        headers: input.headers,
        kind: url.scheme().into(),
        name: name.clone(),
        status: "queued".into(),
        files: vec![],
        group_id,
        group_name,
        created_at: now(),
        error: None,
        downloaded: 0,
        size: 0,
        upload_speed: 0,
        uploaded: 0,
        peak_upload_speed: 0,
        connections: 0,
        last_received: 0,
        last_sent: 0,
        hidden: false,
        stats: SpeedStats::default(),
    };
    let mut tasks = app.0.transfers.tasks.lock().await;
    if tasks.len() >= 2000 {
        return Err(bad("最多保留 2000 个链接任务，请清理旧任务和本地文件"));
    }
    tasks.insert(id, task.clone());
    app.0.transfers.persist(&tasks).await?;
    drop(tasks);
    let folder = app.0.transfers.downloads.join(id.to_string());
    fs::create_dir_all(&folder).await?;
    let options = json!({"gid":gid,"dir":folder.to_string_lossy(),"out":name,"pause":"false","header":task.headers.iter().map(|(k,v)|format!("{k}: {v}")).collect::<Vec<_>>(),"follow-torrent":"false","follow-metalink":"false"});
    if let Err(error) = app
        .0
        .transfers
        .rpc("addUri", json!([[task.uri], options]))
        .await
    {
        let mut tasks = app.0.transfers.tasks.lock().await;
        if let Some(t) = tasks.get_mut(&id) {
            t.status = "failed".into();
            t.error = Some(error.to_string());
        }
        app.0.transfers.persist(&tasks).await?;
        return Err(error.into());
    }
    app.0.transfers.downloading.store(true, Ordering::Relaxed);
    Ok(Json(task.public()))
}
async fn list(State(app): State<App>) -> Json<Value> {
    Json(json!(app.0.transfers.public().await))
}
async fn pause(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    app.0.transfers.rpc("forcePause", json!([task.gid])).await?;
    task.status = "paused".into();
    task.upload_speed = 0;
    app.0.transfers.persist(&tasks).await?;
    Ok(Json(json!({"ok":true})))
}
async fn resume(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    if task.hidden || task.status == "completed" {
        return Err(bad("任务已结束"));
    }
    app.0.transfers.rpc("unpause", json!([task.gid])).await?;
    task.status = "queued".into();
    task.error = None;
    app.0.transfers.persist(&tasks).await?;
    app.0.transfers.downloading.store(true, Ordering::Relaxed);
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct Remove {
    #[serde(default)]
    delete_files: bool,
}
async fn remove(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Query(q): Query<Remove>,
) -> ApiResult<Json<Value>> {
    let mut tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    if matches!(task.status.as_str(), "downloading" | "queued" | "seeding") {
        return Err(bad("请先暂停下载或做种，再移除任务"));
    }
    let _ = app.0.transfers.rpc("forceRemove", json!([task.gid])).await;
    let _ = app
        .0
        .transfers
        .rpc("removeDownloadResult", json!([task.gid]))
        .await;
    if q.delete_files {
        let dir = app.0.transfers.downloads.join(id.to_string());
        if fs::try_exists(&dir).await? {
            fs::remove_dir_all(dir).await?;
        }
        tasks.remove(&id);
    } else {
        task.hidden = true;
        task.uri.clear();
        task.headers.clear();
    }
    app.0.transfers.persist(&tasks).await?;
    Ok(Json(json!({"ok":true})))
}
async fn file(
    State(app): State<App>,
    Path((id, index)): Path<(Uuid, u64)>,
    headers: HeaderMap,
    method: Method,
) -> ApiResult<Response> {
    let tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get(&id).ok_or_else(|| bad("任务不存在"))?;
    let f = task
        .files
        .iter()
        .find(|f| f.index == index)
        .ok_or_else(|| bad("文件不存在"))?;
    if f.completed != f.length
        || !matches!(task.status.as_str(), "completed" | "seeding" | "paused")
    {
        return Err(bad("文件仍在下载或校验中"));
    }
    let root = app.0.transfers.downloads.join(id.to_string());
    let path = root.join(&f.name);
    let name = path
        .file_name()
        .context("文件名无效")?
        .to_string_lossy()
        .to_string();
    let real = fs::canonicalize(&path).await?;
    ensure!(
        real.starts_with(fs::canonicalize(&root).await?)
            && !fs::symlink_metadata(&path).await?.file_type().is_symlink(),
        "本地文件路径无效"
    );
    drop(tasks);
    crate::serve_file::send(&real, &name, &headers, method == Method::HEAD).await
}
async fn files(State(app): State<App>) -> Json<Value> {
    let tasks = app.0.transfers.tasks.lock().await;
    let mut files = vec![];
    for t in tasks.values() {
        for f in &t.files {
            if f.completed == f.length
                && matches!(t.status.as_str(), "completed" | "seeding" | "paused")
            {
                files.push(json!({"id":t.id,"index":f.index,"name":f.name,"size":f.length,"seeding":t.status=="seeding","url":format!("/api/transfers/{}/files/{}",t.id,f.index)}));
            }
        }
    }
    Json(json!(files))
}
async fn status(State(app): State<App>) -> Json<Value> {
    Json(json!({"available":app.0.transfers.available(),"protocols":app.0.transfers.protocols()}))
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/transfers", get(list).post(create))
        .route("/transfers/status", get(status))
        .route("/transfers/files", get(files))
        .route("/transfers/{id}", axum::routing::delete(remove))
        .route("/transfers/{id}/pause", post(pause))
        .route("/transfers/{id}/resume", post(resume))
        .route("/transfers/{id}/files/{index}", get(file))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
