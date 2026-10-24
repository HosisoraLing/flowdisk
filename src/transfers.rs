//! An isolated aria2 subprocess handles ordinary link downloads. Its RPC never
//! listens outside loopback; only typed, authenticated FlowDisk routes reach it.
use crate::{ApiResult, App, SpeedStats, bad, now, safe_name};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Method},
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
    time::Duration,
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
    #[serde(default)]
    pub local_path: Option<String>,
    #[serde(default)]
    pub local_deleted: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Transfer {
    pub(crate) id: Uuid,
    gid: String,
    uri: String,
    headers: BTreeMap<String, String>,
    #[serde(default)]
    torrent: String,
    #[serde(default)]
    approved: bool,
    #[serde(default)]
    peers: u64,
    #[serde(default)]
    seeders: u64,
    #[serde(default)]
    pieces: u64,
    #[serde(default)]
    completed_pieces: u64,
    #[serde(default)]
    seed_ms: u64,
    pub kind: String,
    pub name: String,
    pub(crate) status: String,
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
    pub(crate) hidden: bool,
    #[serde(default)]
    pub(crate) destination: Option<String>,
    #[serde(default)]
    seed_check: bool,
    #[serde(default)]
    seed_only: bool,
    #[serde(default)]
    seed_forgotten: bool,
    #[serde(default)]
    seed_error: Option<String>,
    #[serde(default)]
    info_hash: String,
    #[serde(default)]
    seed_policy_ratio: f64,
    #[serde(default)]
    seed_policy_minutes: u64,
    #[serde(flatten)]
    stats: SpeedStats,
}
impl Transfer {
    fn public(&self) -> Value {
        json!({"id":self.id,"engine":"aria2","kind":self.kind,"name":self.name,"path":display_uri(&self.uri),"status":self.status,"size":self.size,"downloaded":self.downloaded,"speed":if self.status=="downloading"{self.stats.speed_history.last().map_or(0,|p|p.speed)}else{0},"upload_speed":self.upload_speed,"uploaded":self.uploaded,"peak_upload_speed":self.peak_upload_speed,"share_ratio":if self.downloaded>0{self.uploaded as f64/self.downloaded as f64}else{0.},"connections":self.connections,"peers":self.peers,"seeders":self.seeders,"pieces":self.pieces,"completed_pieces":self.completed_pieces,"seed_ms":self.seed_ms,"group_id":self.group_id,"group_name":self.group_name,"created_at":self.created_at,"error":self.error,"files":self.files,"peak_speed":self.stats.peak_speed,"average_speed":self.stats.average_speed,"active_ms":self.stats.active_ms,"average_upload_speed":(self.uploaded as u128*1000/self.stats.active_ms.max(1) as u128).min(u64::MAX as u128) as u64,"transferred_bytes":self.stats.transferred_bytes,"speed_history":self.stats.speed_history})
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
    connections_per_file: usize,
    min_split_mib: usize,
    downloading: AtomicBool,
    pub received_delta: AtomicU64,
    pub sent_delta: AtomicU64,
    last_saved: AtomicU64,
}
fn display_uri(raw: &str) -> String {
    if raw.starts_with("magnet:") {
        return "magnet · BitTorrent".into();
    }
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
            if let Some(dir) = &t.destination {
                crate::local::valid_relative(dir)?;
            }
            for f in &t.files {
                ensure!(
                    !FsPath::new(&f.name).components().any(|p| matches!(
                        p,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )),
                    "本地文件路径无效"
                );
                if let Some(path) = &f.local_path {
                    crate::local::valid_relative(path)?;
                }
            }
            if !matches!(t.status.as_str(), "completed" | "failed" | "selecting") {
                t.status = "paused".into();
            }
            t.upload_speed = 0;
            if t.seed_only {
                t.status = "completed".into();
            }
            t.seed_check = false;
            t.seed_only = false;
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
            connections_per_file: crate::number("LINK_CONNECTIONS_PER_FILE", 8, 16)?,
            min_split_mib: crate::number("LINK_MIN_SPLIT_MIB", 1, 1024)?,
            downloading: AtomicBool::new(false),
            received_delta: AtomicU64::new(0),
            sent_delta: AtomicU64::new(0),
            last_saved: AtomicU64::new(now()),
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
        let connections = engine.connections_per_file;
        let min_split = engine.min_split_mib;
        let contents = format!(
            "enable-rpc=true\nrpc-listen-all=false\nrpc-listen-port={port}\nrpc-secret={secret}\nrpc-save-upload-metadata=true\nenable-console-readout=false\nquiet=true\nconsole-log-level=error\nlog-level=error\nfile-allocation=none\ncontinue=true\nauto-file-renaming=false\nallow-overwrite=false\nsplit={connections}\nmax-connection-per-server={connections}\nmin-split-size={min_split}M\nmax-concurrent-downloads=3\nmax-download-result=2000\nmax-tries=5\nretry-wait=2\nconnect-timeout=15\ntimeout=30\ncheck-certificate=true\nno-netrc=true\nbt-save-metadata=true\npause=true\npause-metadata=true\nfollow-torrent=false\nfollow-metalink=false\nseed-ratio=1.0\nseed-time=60\nlisten-port={bt_port}\ndht-listen-port={bt_port}\ndht-file-path={}\ndht-file-path6={}\ninput-file={}\nsave-session={}\nsave-session-interval=5\nauto-save-interval=5\nforce-save=true\ndir={}\n",
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
        let mut command =
            Command::new(std::env::var("ARIA2_BIN").unwrap_or_else(|_| "aria2c".into()));
        command
            .arg(format!("--conf-path={}", config.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // Compose uses uppercase names; aria2 only reads the lowercase proxy variables.
        // Keep proxy credentials out of the config file and command-line arguments.
        for (upper, lower) in [
            ("HTTP_PROXY", "http_proxy"),
            ("HTTPS_PROXY", "https_proxy"),
            ("NO_PROXY", "no_proxy"),
        ] {
            if let Some(value) = std::env::var_os(upper) {
                command.env(lower, value);
            }
        }
        match command.spawn() {
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
    pub(crate) async fn persist(&self, tasks: &BTreeMap<Uuid, Transfer>) -> Result<()> {
        save(&self.path, tasks).await?;
        self.last_saved.store(now(), Ordering::Relaxed);
        Ok(())
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
        let mut tasks = self.tasks.lock().await;
        for task in tasks.values_mut().filter(|t| {
            matches!(
                t.status.as_str(),
                "downloading" | "seeding" | "paused" | "checking"
            )
        }) {
            if task.seed_only {
                task.seed_policy_ratio = ratio;
                task.seed_policy_minutes = minutes;
            }
            let _ = self.rpc("changeOption", json!([task.gid, options])).await;
        }
        self.persist(&tasks).await
    }
    pub async fn poll(&self, elapsed_ms: u64) -> Result<()> {
        if !self.available() {
            return Ok(());
        }
        let mut tasks = self.tasks.lock().await;
        let mut downloading = false;
        let mut changed = false;
        let mut active = false;
        for t in tasks
            .values_mut()
            .filter(|t| !t.hidden && t.status != "completed")
        {
            let previous = (
                t.status.clone(),
                t.gid.clone(),
                t.files.len(),
                t.torrent.len(),
            );
            let Ok(mut v) = self.rpc("tellStatus", json!([t.gid])).await else {
                continue;
            };
            if t.seed_only {
                self.poll_seed(t, &v, elapsed_ms).await;
                changed |= previous.0 != t.status || previous.1 != t.gid;
                active |= t.seed_only;
                continue;
            }
            if let Some(child) = v["followedBy"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                let next = child.to_owned();
                self.cache_torrent(t, &v).await;
                if let Ok(child_state) = self.rpc("tellStatus", json!([next])).await {
                    t.gid = next;
                    t.kind = "torrent".into();
                    t.approved = false;
                    t.last_received = 0;
                    t.last_sent = 0;
                    v = child_state;
                }
            }
            self.cache_torrent(t, &v).await;
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
            let is_bt =
                v["bittorrent"].is_object() || matches!(t.kind.as_str(), "magnet" | "torrent");
            if is_bt && status == "active" && total > 0 && completed >= total {
                t.status = "seeding".into();
                t.seed_ms = t.seed_ms.saturating_add(elapsed_ms);
            }
            if is_bt && status == "active" && total == 0 {
                t.status = "metadata".into();
            }

            t.peers = number(&v, "connections");
            t.seeders = number(&v, "numSeeders");
            t.pieces = number(&v, "numPieces");
            t.completed_pieces = v["bitfield"]
                .as_str()
                .map(|b| {
                    b.chars()
                        .filter_map(|c| c.to_digit(16))
                        .map(u32::count_ones)
                        .sum::<u32>() as u64
                })
                .unwrap_or(0)
                .min(t.pieces);
            let received = completed.saturating_sub(t.last_received);
            t.last_received = t.last_received.max(completed);
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
                let speed = number(&v, "downloadSpeed");
                t.stats.peak_speed = t.stats.peak_speed.max(speed);
                if let Some(point) = t.stats.speed_history.last_mut() {
                    point.speed = speed;
                }
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
                let root = self.task_dir(t);
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
                                local_path: t
                                    .files
                                    .iter()
                                    .find(|old| old.index == number(f, "index"))
                                    .and_then(|old| old.local_path.clone()),
                                local_deleted: t.files.iter().any(|old| {
                                    old.index == number(f, "index") && old.local_deleted
                                }),
                            });
                        }
                    }
                }
                if list.len() > 5000 {
                    let _ = self.rpc("forcePause", json!([t.gid])).await;
                    t.status = "failed".into();
                    t.error = Some("种子超过 5000 个文件，请拆分后添加".into());
                    list.truncate(5000);
                }
                t.files = list;
                if is_bt {
                    t.size = t
                        .files
                        .iter()
                        .filter(|f| f.selected)
                        .map(|f| f.length)
                        .sum();
                    t.downloaded = t
                        .files
                        .iter()
                        .filter(|f| f.selected)
                        .map(|f| f.completed)
                        .sum();
                    if status == "active" && t.approved && t.size > 0 && t.downloaded >= t.size {
                        if t.status != "seeding" {
                            t.seed_ms = t.seed_ms.saturating_add(elapsed_ms);
                        }
                        t.status = "seeding".into();
                    }
                    if !t.approved && status == "paused" && !t.files.is_empty() {
                        t.status = "selecting".into();
                    }
                    if let Some(name) = v["bittorrent"]["info"]["name"].as_str() {
                        t.name = name.to_owned();
                    }
                }
                if t.files.len() == 1 {
                    t.name = t.files[0].name.clone();
                }
            }
            downloading |= matches!(t.status.as_str(), "downloading" | "queued" | "metadata");
            active |= matches!(
                t.status.as_str(),
                "downloading" | "queued" | "metadata" | "seeding"
            );
            changed |= previous
                != (
                    t.status.clone(),
                    t.gid.clone(),
                    t.files.len(),
                    t.torrent.len(),
                );
        }
        self.downloading.store(downloading, Ordering::Relaxed);
        if changed
            || elapsed_ms == 0
            || (active && now().saturating_sub(self.last_saved.load(Ordering::Relaxed)) >= 15)
        {
            self.persist(&tasks).await?;
        }
        Ok(())
    }
    fn task_dir(&self, task: &Transfer) -> PathBuf {
        task.destination
            .as_ref()
            .map(|p| self.downloads.parent().unwrap().join(p))
            .unwrap_or_else(|| self.downloads.join(task.id.to_string()))
    }
    async fn add(&self, task: &Transfer, paused: bool) -> Result<()> {
        let folder = self.task_dir(task);
        let root = self.downloads.parent().context("下载根目录无效")?;
        crate::local::checked_path(root, &crate::local::relative(root, &folder)?, true).await?;
        fs::create_dir_all(&folder).await?;
        let mut options = json!({"gid":task.gid,"dir":folder.to_string_lossy(),"pause":if paused{"true"}else{"false"},"header":task.headers.iter().map(|(k,v)|format!("{k}: {v}")).collect::<Vec<_>>(),"pause-metadata":"true","follow-torrent":"true","follow-metalink":"false"});
        if !matches!(task.kind.as_str(), "torrent" | "magnet") {
            options["out"] = task.name.clone().into();
        }
        if task.approved && !task.files.is_empty() {
            options["select-file"] = task
                .files
                .iter()
                .filter(|f| f.selected)
                .map(|f| f.index.to_string())
                .collect::<Vec<_>>()
                .join(",")
                .into();
        }
        if !task.torrent.is_empty() {
            self.rpc("addTorrent", json!([task.torrent, [], options]))
                .await?;
        } else {
            self.rpc("addUri", json!([[task.uri], options])).await?;
        }
        Ok(())
    }
    pub async fn shutdown(&self) {
        if self.available() {
            let _ = self.rpc("forcePauseAll", json!([])).await;
            let _ = self.poll(0).await;
            let _ = self.rpc("saveSession", json!([])).await;
            let _ = self.rpc("shutdown", json!([])).await;
        }
        if let Some(mut child) = self.child.lock().await.take()
            && tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
        {
            let _ = child.kill().await;
        }
    }
}
#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    torrent_base64: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    name: String,
    group_id: Option<Uuid>,
    group_name: Option<String>,
    #[serde(default = "crate::local::yes")]
    create_folder: bool,
}
async fn create(State(app): State<App>, Json(input): Json<Input>) -> ApiResult<Json<Value>> {
    let _guard = app.0.control.file_lock.lock().await;
    if !app.0.transfers.available() {
        return Err(bad("链接下载引擎不可用，请使用含 aria2 的 Docker 镜像"));
    }
    use base64::Engine;
    let uploaded = !input.torrent_base64.is_empty();
    let magnet = input.uri.trim().starts_with("magnet:");
    let url = if uploaded {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&input.torrent_base64)
            .map_err(|_| bad("种子文件编码无效"))?;
        if bytes.len() > 8 * 1024 * 1024 || bytes.first() != Some(&b'd') {
            return Err(bad("请选择不超过 8 MiB 的 .torrent 文件"));
        }
        Url::parse("https://example.invalid/torrent").unwrap()
    } else if magnet {
        let u = Url::parse(input.uri.trim()).map_err(|_| bad("磁力链接无效"))?;
        let supported = u.query_pairs().filter(|(k, _)| k == "xt").any(|(_, v)| {
            v.strip_prefix("urn:btih:").is_some_and(|h| {
                (h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                    || (h.len() == 32
                        && h.bytes()
                            .all(|b| b.is_ascii_alphabetic() || (b'2'..=b'7').contains(&b)))
            })
        });
        if input.uri.len() > 16384 || !supported {
            return Err(bad(
                "需要包含有效 BT v1 info-hash 的磁力链接，暂不支持仅 BT v2 的磁力链接",
            ));
        }
        u
    } else {
        let u =
            link(input.uri.trim()).map_err(|_| bad("请填写 HTTP/HTTPS/FTP/SFTP 或 magnet 链接"))?;
        if !app.0.transfers.protocols().iter().any(|p| p == u.scheme()) {
            return Err(bad("当前引擎未编译这个协议"));
        }
        u
    };
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
    let destination = if input.create_folder {
        crate::local::batch_folder(&group_name, group_id)
    } else {
        String::new()
    };
    let mut task = Transfer {
        id,
        gid: gid.clone(),
        uri: input.uri.trim().into(),
        headers: input.headers,
        torrent: input.torrent_base64,
        approved: !uploaded && !magnet,
        peers: 0,
        seeders: 0,
        pieces: 0,
        completed_pieces: 0,
        seed_ms: 0,
        kind: if uploaded { "torrent" } else { url.scheme() }.into(),
        name: name.clone(),
        status: if uploaded {
            "selecting"
        } else if magnet {
            "metadata"
        } else {
            "queued"
        }
        .into(),
        files: vec![],
        group_id,
        group_name,
        destination: Some(destination),
        seed_check: false,
        seed_only: false,
        seed_forgotten: false,
        seed_error: None,
        info_hash: String::new(),
        seed_policy_ratio: 0.,
        seed_policy_minutes: 0,
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
    if !matches!(task.kind.as_str(), "torrent" | "magnet") {
        let directory = app.0.transfers.task_dir(&task);
        if fs::try_exists(directory.join(&task.name)).await?
            || tasks
                .values()
                .any(|t| app.0.transfers.task_dir(t) == directory && t.name == task.name)
        {
            task.name = format!(
                "{}-{}",
                &id.simple().to_string()[..8],
                task.name.chars().take(60).collect::<String>()
            );
        }
    }
    tasks.insert(id, task.clone());
    app.0.transfers.persist(&tasks).await?;
    app.0.transfers.downloading.store(true, Ordering::Relaxed);
    let native = app.0.bandwidth.native_active.load(Ordering::Relaxed);
    let added = async {
        app.0.bandwidth.budget(&app, native).await?;
        app.0.transfers.add(&task, uploaded).await
    }
    .await;
    if let Err(error) = added {
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
    let seed = app
        .0
        .transfers
        .tasks
        .lock()
        .await
        .get(&id)
        .is_some_and(|t| t.seed_only || (t.bt() && t.status == "seeding"));
    if seed {
        return set_seed(
            State(app),
            Path(id),
            Json(SeedInput {
                enabled: false,
                forget: false,
            }),
        )
        .await;
    }
    let mut tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    app.0.transfers.rpc("forcePause", json!([task.gid])).await?;
    task.status = "paused".into();
    task.upload_speed = 0;
    app.0.transfers.persist(&tasks).await?;
    Ok(Json(json!({"ok":true})))
}
async fn resume(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let seed = app
        .0
        .transfers
        .tasks
        .lock()
        .await
        .get(&id)
        .is_some_and(|t| t.bt() && t.seed_ready());
    if seed {
        return set_seed(
            State(app),
            Path(id),
            Json(SeedInput {
                enabled: true,
                forget: false,
            }),
        )
        .await;
    }
    let mut tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    if task.hidden || task.status == "completed" {
        return Err(bad("任务已结束"));
    }
    if !task.approved && !task.files.is_empty() {
        return Err(bad("请先选择种子文件，再开始下载"));
    }
    app.0.transfers.downloading.store(true, Ordering::Relaxed);
    let native = app.0.bandwidth.native_active.load(Ordering::Relaxed);
    app.0.bandwidth.budget(&app, native).await?;
    if !task.bt() {
        // Restored aria2 sessions can still contain the old per-task split options.
        let _ = app
            .0
            .transfers
            .rpc(
                "changeOption",
                json!([task.gid, {
                    "split":app.0.transfers.connections_per_file.to_string(),
                    "max-connection-per-server":app.0.transfers.connections_per_file.to_string(),
                    "min-split-size":format!("{}M",app.0.transfers.min_split_mib)
                }]),
            )
            .await;
    }
    if app
        .0
        .transfers
        .rpc("unpause", json!([task.gid]))
        .await
        .is_err()
    {
        let _ = app
            .0
            .transfers
            .rpc("removeDownloadResult", json!([task.gid]))
            .await;
        task.gid = Uuid::new_v4().simple().to_string()[..16].into();
        app.0.transfers.add(task, false).await?;
    }

    task.status = "queued".into();
    task.error = None;
    app.0.transfers.persist(&tasks).await?;
    app.0.transfers.downloading.store(true, Ordering::Relaxed);
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct Selection {
    indices: Vec<u64>,
}
async fn select(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(input): Json<Selection>,
) -> ApiResult<Json<Value>> {
    let mut tasks = app.0.transfers.tasks.lock().await;
    let t = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    if !matches!(t.status.as_str(), "selecting" | "paused") || t.hidden {
        return Err(bad("请先暂停任务再调整文件范围"));
    }
    if input.indices.is_empty()
        || input.indices.len() > 5000
        || input
            .indices
            .iter()
            .any(|i| !t.files.iter().any(|f| f.index == *i))
    {
        return Err(bad("请选择有效的下载文件"));
    }
    let selected = input
        .indices
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    app.0
        .transfers
        .rpc("changeOption", json!([t.gid,{"select-file":selected}]))
        .await?;
    app.0.transfers.downloading.store(true, Ordering::Relaxed);
    let native = app.0.bandwidth.native_active.load(Ordering::Relaxed);
    app.0.bandwidth.budget(&app, native).await?;
    t.approved = true;
    for f in &mut t.files {
        f.selected = input.indices.contains(&f.index);
    }
    app.0.transfers.persist(&tasks).await?;
    let t = tasks.get_mut(&id).unwrap();
    app.0.transfers.rpc("unpause", json!([t.gid])).await?;
    t.status = "queued".into();
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
    let _guard = app.0.control.file_lock.lock().await;
    let mut tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    if matches!(
        task.status.as_str(),
        "downloading" | "queued" | "seeding" | "metadata" | "checking"
    ) {
        return Err(bad("请先暂停下载或做种，再移除任务"));
    }
    let _ = app.0.transfers.rpc("forceRemove", json!([task.gid])).await;
    let _ = app
        .0
        .transfers
        .rpc("removeDownloadResult", json!([task.gid]))
        .await;
    if q.delete_files {
        for f in &task.files {
            let path = crate::local::transfer_file(&app, task, f);
            let relative = crate::local::relative(&app.0.downloads, &path)?;
            crate::local::checked_path(&app.0.downloads, &relative, true).await?;
            if fs::try_exists(&path).await? {
                fs::remove_file(path).await?;
            }
        }
        let dir = app.0.transfers.downloads.join(id.to_string());
        if task.destination.is_none() && fs::try_exists(&dir).await? {
            fs::remove_dir_all(dir).await?;
        } else if let Some(dir) = &task.destination
            && !dir.is_empty()
        {
            let _ = fs::remove_dir(app.0.downloads.join(dir)).await;
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
    let _guard = app.0.control.file_lock.lock().await;
    let tasks = app.0.transfers.tasks.lock().await;
    let task = tasks.get(&id).ok_or_else(|| bad("任务不存在"))?;
    let f = task
        .files
        .iter()
        .find(|f| f.index == index)
        .ok_or_else(|| bad("文件不存在"))?;
    if f.local_deleted
        || f.completed != f.length
        || !matches!(
            task.status.as_str(),
            "completed" | "seeding" | "paused" | "checking"
        )
    {
        return Err(bad("文件仍在下载或校验中"));
    }
    let root = app.0.transfers.task_dir(task);
    let path = f
        .local_path
        .as_ref()
        .map(|p| app.0.downloads.join(p))
        .unwrap_or_else(|| root.join(&f.name));
    let name = path
        .file_name()
        .context("文件名无效")?
        .to_string_lossy()
        .to_string();
    let real = fs::canonicalize(&path).await?;
    crate::local::checked_path(
        &app.0.downloads,
        &crate::local::relative(&app.0.downloads, &path)?,
        false,
    )
    .await?;
    if !real.starts_with(fs::canonicalize(&app.0.downloads).await?)
        || fs::symlink_metadata(&path).await?.file_type().is_symlink()
    {
        return Err(bad("本地文件路径无效"));
    }
    drop(tasks);
    crate::serve_file::send(&real, &name, &headers, method == Method::HEAD).await
}
async fn files(State(app): State<App>) -> Json<Value> {
    let tasks = app.0.transfers.tasks.lock().await;
    let mut files = vec![];
    for t in tasks.values() {
        for f in &t.files {
            if !f.local_deleted
                && f.completed == f.length
                && matches!(
                    t.status.as_str(),
                    "completed" | "seeding" | "paused" | "checking"
                )
            {
                files.push(json!({"id":t.id,"index":f.index,"name":f.name,"size":f.length,"seeding":t.status=="seeding","url":format!("/api/transfers/{}/files/{}",t.id,f.index)}));
            }
        }
    }
    Json(json!(files))
}
async fn status(State(app): State<App>) -> Json<Value> {
    Json(
        json!({"available":app.0.transfers.available(),"protocols":app.0.transfers.protocols(),"connections_per_file":app.0.transfers.connections_per_file,"min_split_mib":app.0.transfers.min_split_mib}),
    )
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/transfers", get(list).post(create))
        .route("/transfers/status", get(status))
        .route("/seeding", get(seeding))
        .route("/transfers/{id}/seed", post(set_seed))
        .route("/transfers/files", get(files))
        .route("/transfers/{id}", axum::routing::delete(remove))
        .route("/transfers/{id}/select", post(select))
        .route("/transfers/{id}/pause", post(pause))
        .route("/transfers/{id}/resume", post(resume))
        .route("/transfers/{id}/files/{index}", get(file))
        .layer(DefaultBodyLimit::max(12 * 1024 * 1024))
}

impl Transfer {
    pub(crate) fn seed_linked(&self) -> bool {
        self.bt() && !self.seed_forgotten
    }
    pub(crate) fn bt(&self) -> bool {
        matches!(self.kind.as_str(), "torrent" | "magnet")
    }
    fn seed_ready(&self) -> bool {
        let files: Vec<_> = self.files.iter().filter(|f| f.selected).collect();
        !files.is_empty()
            && files
                .iter()
                .all(|f| !f.local_deleted && f.completed == f.length)
    }
    pub(crate) fn seed_summary(&self) -> Value {
        let enabled = self.seed_only || self.status == "seeding";
        let ready = self.seed_ready();
        json!({"id":self.id,"name":self.name,"status":self.status,"enabled":enabled,"checking":self.seed_check,"can_start":ready&&!enabled&&!self.seed_forgotten,"reason":if self.seed_forgotten{"种子关联已移除"}else if !ready{"选中的文件尚未下载完成或已经删除"}else{""},"upload_speed":self.upload_speed,"uploaded":self.uploaded,"share_ratio":if self.downloaded>0{self.uploaded as f64/self.downloaded as f64}else{0.},"peers":self.peers,"seed_ms":self.seed_ms,"files":self.files.iter().filter(|f|f.selected).count(),"error":self.seed_error})
    }
}
impl Transfers {
    async fn cache_torrent(&self, t: &mut Transfer, v: &Value) {
        if !t.torrent.is_empty() || t.seed_forgotten {
            return;
        }
        if let Some(hash) = v["infoHash"]
            .as_str()
            .filter(|s| s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            t.info_hash = hash.to_ascii_lowercase();
        }
        if !t.bt() && t.info_hash.is_empty() && !v["followedBy"].is_array() {
            return;
        }
        if t.info_hash.is_empty()
            && let Ok(url) = Url::parse(&t.uri)
        {
            for (key, value) in url.query_pairs() {
                if key == "xt"
                    && let Some(hash) = value
                        .strip_prefix("urn:btih:")
                        .filter(|h| h.len() == 40 && h.bytes().all(|c| c.is_ascii_hexdigit()))
                {
                    t.info_hash = hash.to_ascii_lowercase();
                }
            }
        }
        let mut candidates = vec![];
        let root = self.downloads.parent().unwrap();
        let directory = self.task_dir(t);
        if !t.info_hash.is_empty() {
            candidates.push(directory.join(format!("{}.torrent", t.info_hash)));
        }
        candidates.push(directory.join(&t.name));
        if let Ok(url) = Url::parse(&t.uri)
            && let Some(name) = url.path_segments().and_then(|mut p| p.next_back())
        {
            candidates.push(directory.join(safe_name(name)));
        }
        for path in candidates {
            let Ok(relative) = crate::local::relative(root, &path) else {
                continue;
            };
            if crate::local::checked_path(root, &relative, false)
                .await
                .is_err()
            {
                continue;
            }
            if let Ok(meta) = fs::symlink_metadata(&path).await
                && meta.is_file()
                && meta.len() <= 8 * 1024 * 1024
                && let Ok(raw) = fs::read(path).await
                && raw.first() == Some(&b'd')
            {
                use base64::Engine;
                t.torrent = base64::engine::general_purpose::STANDARD.encode(raw);
                break;
            }
        }
    }
    async fn seed_files(&self, t: &Transfer) -> Result<Vec<String>> {
        let root = self.downloads.parent().context("下载根目录无效")?;
        let mut paths = vec![];
        for f in t.files.iter().filter(|f| f.selected) {
            ensure!(
                !f.local_deleted && f.completed == f.length,
                "选中的文件尚未完成或已经删除"
            );
            let path = f
                .local_path
                .as_ref()
                .map(|p| root.join(p))
                .unwrap_or_else(|| self.task_dir(t).join(&f.name));
            let relative = crate::local::relative(root, &path)?;
            crate::local::checked_path(root, &relative, false).await?;
            let meta = fs::symlink_metadata(path).await?;
            ensure!(
                meta.is_file() && meta.len() == f.length,
                "本地文件缺失或大小已经变化，未启动做种"
            );
            paths.push(format!("{}={relative}", f.index));
        }
        ensure!(!paths.is_empty(), "请先选择并完成下载");
        Ok(paths)
    }
    async fn add_seed(&self, t: &Transfer, checking: bool) -> Result<()> {
        ensure!(!t.torrent.is_empty(), "缺少种子元数据，请重新导入原种子");
        let root = self.downloads.parent().context("下载根目录无效")?;
        ensure!(t.seed_policy_minutes > 0, "当前做种策略已停止上传");
        let paths = self.seed_files(t).await?;
        let selected = t
            .files
            .iter()
            .filter(|f| f.selected)
            .map(|f| f.index.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let options = json!({"gid":t.gid,"dir":root.to_string_lossy(),"index-out":paths,"select-file":selected,"pause":if checking{"true"}else{"false"},"file-allocation":"none","allow-overwrite":"false","auto-file-renaming":"false","check-integrity":if checking{"true"}else{"false"},"hash-check-only":if checking{"true"}else{"false"},"bt-seed-unverified":if checking{"false"}else{"true"},"bt-hash-check-seed":"true","bt-save-metadata":"false","follow-torrent":"false","seed-ratio":if checking{"0".to_string()}else{t.seed_policy_ratio.to_string()},"seed-time":if checking{"0".to_string()}else{t.seed_policy_minutes.to_string()}});
        self.rpc("addTorrent", json!([t.torrent, [], options]))
            .await?;
        if checking {
            let state = self.rpc("tellStatus", json!([t.gid])).await?;
            let matches = state["files"].as_array().is_some_and(|files| {
                t.files.iter().filter(|f| f.selected).all(|f| {
                    files.iter().any(|actual| {
                        number(actual, "index") == f.index
                            && number(actual, "length") == f.length
                            && actual["path"].as_str().is_some_and(|p| {
                                FsPath::new(p)
                                    == f.local_path
                                        .as_ref()
                                        .map(|p| root.join(p))
                                        .unwrap_or_else(|| self.task_dir(t).join(&f.name))
                            })
                    })
                })
            });
            if !matches {
                let _ = self.rpc("forceRemove", json!([t.gid])).await;
                let _ = self.rpc("removeDownloadResult", json!([t.gid])).await;
                bail!("种子元数据与本地文件不匹配，未启动校验或修改文件");
            }
            self.rpc("unpause", json!([t.gid])).await?;
        }
        Ok(())
    }
    async fn release_seed(&self, t: &Transfer) -> Result<()> {
        // A missing GID is safe only when the RPC engine itself is still reachable.
        self.rpc("getGlobalStat", json!([])).await?;
        if let Ok(v) = self.rpc("tellStatus", json!([t.gid])).await {
            if matches!(v["status"].as_str(), Some("active" | "waiting")) {
                self.rpc("forcePause", json!([t.gid])).await?;
                let mut stopped = false;
                for _ in 0..40 {
                    let v = self.rpc("tellStatus", json!([t.gid])).await?;
                    if matches!(
                        v["status"].as_str(),
                        Some("paused" | "complete" | "error" | "removed")
                    ) {
                        stopped = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                ensure!(stopped, "做种尚未停止，请稍后重试");
            }
            let _ = self.rpc("forceRemove", json!([t.gid])).await;
            let _ = self.rpc("removeDownloadResult", json!([t.gid])).await;
        }
        Ok(())
    }
    async fn poll_seed(&self, t: &mut Transfer, v: &Value, elapsed: u64) {
        let state = v["status"].as_str().unwrap_or("error");
        if t.seed_check {
            if matches!(state, "active" | "waiting" | "paused") {
                t.status = if state == "paused" {
                    "paused"
                } else {
                    "checking"
                }
                .into();
                return;
            }
            let checked = v["files"].as_array().is_some_and(|files| {
                t.files.iter().filter(|f| f.selected).all(|f| {
                    files.iter().any(|actual| {
                        number(actual, "index") == f.index
                            && number(actual, "length") == f.length
                            && number(actual, "completedLength") == f.length
                    })
                })
            });
            if state == "complete" && checked {
                let _ = self.rpc("removeDownloadResult", json!([t.gid])).await;
                t.gid = Uuid::new_v4().simple().to_string()[..16].into();
                t.seed_check = false;
                t.last_sent = 0;
                match self.add_seed(t, false).await {
                    Ok(()) => {
                        t.status = "seeding".into();
                        t.seed_error = None;
                        return;
                    }
                    Err(_) => {
                        t.seed_error =
                            Some("文件已保留，但做种启动失败，请检查引擎或文件后重试".into())
                    }
                }
            } else {
                t.seed_error = Some("种子校验未通过，已停止；没有重新下载或覆盖本地文件".into());
            }
            t.seed_check = false;
            t.seed_only = false;
            t.status = "completed".into();
            t.upload_speed = 0;
            return;
        }
        let sent = number(v, "uploadLength");
        let delta = if sent >= t.last_sent {
            sent - t.last_sent
        } else {
            sent
        };
        t.uploaded = t.uploaded.saturating_add(delta);
        t.last_sent = sent;
        self.sent_delta.fetch_add(delta, Ordering::Relaxed);
        t.upload_speed = number(v, "uploadSpeed");
        t.peak_upload_speed = t.peak_upload_speed.max(t.upload_speed);
        t.peers = number(v, "connections");
        t.seeders = number(v, "numSeeders");
        if state == "active" {
            t.status = "seeding".into();
            t.seed_ms = t.seed_ms.saturating_add(elapsed);
        } else if state == "waiting" {
            t.status = "queued".into();
        } else if state == "paused" {
            t.status = "paused".into();
            t.upload_speed = 0;
        } else {
            t.status = "completed".into();
            t.seed_only = false;
            t.upload_speed = 0;
            if state == "error" {
                t.seed_error = Some("做种引擎已停止，本地文件保留".into());
            }
        }
    }
}
#[derive(Deserialize)]
struct SeedInput {
    enabled: bool,
    #[serde(default)]
    forget: bool,
}
async fn seeding(State(app): State<App>) -> Json<Value> {
    let tasks = app.0.transfers.tasks.lock().await;
    Json(json!(
        tasks
            .values()
            .filter(|t| t.bt() && !t.seed_forgotten)
            .map(Transfer::seed_summary)
            .collect::<Vec<_>>()
    ))
}
async fn set_seed(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(input): Json<SeedInput>,
) -> ApiResult<Json<Value>> {
    let _guard = app.0.control.file_lock.lock().await;
    let mut tasks = app.0.transfers.tasks.lock().await;
    let t = tasks.get_mut(&id).ok_or_else(|| bad("任务不存在"))?;
    if !t.bt() || t.seed_forgotten {
        return Err(bad("该文件没有可用的 BT 种子关联"));
    }
    if !t.seed_ready() {
        return Err(bad("请先下载完成选中的文件；未完成任务请在下载任务页暂停"));
    }
    if input.enabled && input.forget {
        return Err(bad("不能同时开始做种和移除关联"));
    }
    if !app.0.transfers.available() {
        return Err(bad("做种引擎不可用"));
    }
    if input.enabled {
        if t.seed_only || t.status == "seeding" {
            return Ok(Json(t.seed_summary()));
        }
        let value = app
            .0
            .transfers
            .rpc("tellStatus", json!([t.gid]))
            .await
            .unwrap_or(Value::Null);
        app.0.transfers.cache_torrent(t, &value).await;
        if t.torrent.is_empty() {
            return Err(bad("缺少种子元数据，请重新导入原种子；本地文件未修改"));
        }
        app.0.transfers.seed_files(t).await?;
        let settings = app.0.bandwidth.settings.lock().await.clone();
        if settings.seed_minutes == 0 {
            return Err(bad("做种时长目前为 0，请先在首页带宽策略中设置做种时长"));
        }
        app.0.transfers.release_seed(t).await?;
        t.seed_policy_ratio = settings.seed_ratio;
        t.seed_policy_minutes = settings.seed_minutes;
        t.gid = Uuid::new_v4().simple().to_string()[..16].into();
        app.0.transfers.add_seed(t, true).await?;
        t.seed_only = true;
        t.seed_check = true;
        t.seed_error = None;
        t.hidden = false;
        t.status = "checking".into();
        t.last_sent = 0;
    } else {
        app.0.transfers.release_seed(t).await?;
        t.seed_only = false;
        t.seed_check = false;
        t.status = "completed".into();
        t.upload_speed = 0;
        if input.forget {
            t.seed_forgotten = true;
            t.torrent.clear();
            t.info_hash.clear();
            t.seed_error = None;
        }
    }
    let result = t.seed_summary();
    app.0.transfers.persist(&tasks).await?;
    Ok(Json(result))
}
