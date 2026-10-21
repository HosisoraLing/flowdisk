use crate::{ApiResult, App, Job, bad, now, safe_name, valid_path};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
use tokio::{fs, sync::Mutex};
use uuid::Uuid;

pub const PROVIDERS: [(&str, &str); 6] = [
    ("baidu", "百度网盘"),
    ("quark", "夸克网盘"),
    ("115", "115 网盘"),
    ("thunder", "迅雷云盘"),
    ("onedrive", "OneDrive"),
    ("google", "Google Drive"),
];
pub struct Control {
    auth: crate::auth::Auth,
    pub shares: Mutex<BTreeMap<Uuid, Share>>,
    active: Mutex<BTreeSet<Uuid>>,
    logins: Mutex<BTreeMap<Uuid, Value>>,
    account_lock: Mutex<()>,
    pub file_lock: Mutex<()>,
    state_path: PathBuf,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Share {
    id: Uuid,
    pub(crate) provider: String,
    url: String,
    #[serde(default)]
    passcode: String,
    pub(crate) status: String,
    pub(crate) message: String,
    pub(crate) jobs: Vec<Uuid>,
    #[serde(default)]
    pub(crate) roots: Vec<String>,
    #[serde(default)]
    pub(crate) entries: Vec<crate::review::Entry>,
    #[serde(default)]
    pub(crate) capacity: Value,
    #[serde(default)]
    pub(crate) transfer_approved: bool,
    interaction: Option<Uuid>,
    created_at: u64,
}
impl Share {
    fn public(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        v.as_object_mut().unwrap().remove("passcode");
        v.as_object_mut().unwrap().remove("entries");
        v.as_object_mut().unwrap().remove("roots");
        v["has_contents"] = json!(!self.entries.is_empty());
        v["file_count"] = json!(self.entries.iter().filter(|e| !e.is_dir).count());
        v["total_size"] = json!(
            self.entries
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.size)
                .sum::<u64>()
        );
        v
    }
}
impl Control {
    pub(crate) fn native_auth(&self) -> &crate::auth::Auth {
        &self.auth
    }
    pub async fn new(data: &std::path::Path, _token: &str) -> Result<Self> {
        let path = data.join("shares.json");
        let mut shares: BTreeMap<Uuid, Share> = if fs::try_exists(&path).await? {
            serde_json::from_slice(&fs::read(&path).await?).context("分享任务记录损坏，拒绝覆盖")?
        } else {
            BTreeMap::new()
        };
        for share in shares.values_mut() {
            if matches!(share.status.as_str(), "importing" | "needs_interaction") {
                share.status = "waiting_login".into();
                share.interaction = None;
                share.message = "服务已重启，正在检查登录后恢复任务".into();
            }
        }
        Ok(Self {
            auth: crate::auth::Auth::new()?,
            shares: Mutex::new(shares),
            active: Mutex::new(BTreeSet::new()),
            logins: Mutex::new(BTreeMap::new()),
            account_lock: Mutex::new(()),
            file_lock: Mutex::new(()),
            state_path: path,
        })
    }
    pub(crate) async fn save(&self, shares: &BTreeMap<Uuid, Share>) -> Result<()> {
        let temp = self.state_path.with_extension("tmp");
        fs::write(&temp, serde_json::to_vec_pretty(shares)?).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        fs::File::open(&temp).await?.sync_all().await?;
        fs::rename(temp, &self.state_path).await?;
        Ok(())
    }
    async fn authenticate(
        &self,
        path: &str,
        method: Method,
        body: Option<Value>,
    ) -> ApiResult<Value> {
        self.auth
            .request(path, method, body.unwrap_or_else(|| json!({})))
            .await
    }
}
fn provider(value: &str) -> ApiResult<()> {
    if PROVIDERS.iter().any(|(key, _)| *key == value) {
        Ok(())
    } else {
        Err(bad("不支持的网盘"))
    }
}
fn driver_provider(driver: &str) -> Option<&'static str> {
    match driver {
        "BaiduNetdisk" | "BaiduNative" => Some("baidu"),
        "Quark" | "QuarkOpen" => Some("quark"),
        "115 Cloud" | "115 Open" => Some("115"),
        "Onedrive" => Some("onedrive"),
        "GoogleDrive" => Some("google"),
        "Thunder" | "ThunderExpert" | "ThunderBrowser" | "ThunderBrowserExpert" => Some("thunder"),
        _ => None,
    }
}
async fn storages(app: &App) -> Result<Vec<Value>> {
    let result = app
        .0
        .gateway
        .get("/api/admin/storage/list?page=1&per_page=0")
        .await?;
    let mut all = result["content"].as_array().cloned().unwrap_or_default();
    if let Some(native) = app.0.gateway.baidu.storage().await {
        all.insert(0, native);
    }
    Ok(all)
}
async fn account_storage(app: &App, key: &str) -> Result<Option<Value>> {
    Ok(storages(app)
        .await?
        .into_iter()
        .filter(|s| {
            driver_provider(s["driver"].as_str().unwrap_or("")) == Some(key)
                && s["disabled"] != true
        })
        .min_by_key(|s| s["mount_path"].as_str().unwrap_or("").to_owned()))
}
async fn upsert_storage(
    app: &App,
    key: &str,
    credential: &Value,
    mount: Option<&str>,
) -> ApiResult<String> {
    let driver = credential["driver"]
        .as_str()
        .ok_or_else(|| bad("授权未包含网盘适配器"))?;
    let path = mount
        .map(str::to_owned)
        .unwrap_or_else(|| format!("/{key}"));
    if driver == "BaiduNative" {
        if app.0.jobs.lock().await.values().any(|j| {
            crate::baidu::owns(&j.path)
                && matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
        }) {
            return Err(bad("请先暂停百度的下载任务，再更换账号"));
        }
        let all = storages(app).await?;
        if all
            .iter()
            .any(|s| s["mount_path"] == crate::baidu::MOUNT && s["driver"] != "BaiduNative")
        {
            return Err(bad("请先退出原有百度网盘账号，再使用扫码连接"));
        }
        app.0
            .gateway
            .baidu
            .connect(
                credential["addition"]["cookie"]
                    .as_str()
                    .ok_or_else(|| bad("百度登录凭据缺失"))?,
            )
            .await?;
        return Ok(crate::baidu::MOUNT.into());
    }
    if path == crate::baidu::MOUNT && app.0.gateway.baidu.connected().await {
        return Err(bad("请先退出当前百度扫码账号，再切换为官方授权"));
    }
    let mut addition = json!({});
    let query =
        Url::parse_with_params("http://local/api/admin/driver/info", [("driver", driver)]).unwrap();
    if let Ok(info) = app
        .0
        .gateway
        .get(&format!("{}?{}", query.path(), query.query().unwrap()))
        .await
        && let Some(items) = info["additional"]
            .as_array()
            .or_else(|| info["common"].as_array())
    {
        for item in items {
            if let (Some(name), Some(default)) = (item["name"].as_str(), item.get("default")) {
                if default.is_null() || default == "" {
                    continue;
                }
                let value = if item["type"] == "number" {
                    default
                        .as_str()
                        .and_then(|v| v.parse::<i64>().ok())
                        .map(Value::from)
                        .unwrap_or_else(|| default.clone())
                } else if item["type"] == "bool" {
                    Value::Bool(default == "true" || default == true)
                } else {
                    default.clone()
                };
                addition[name] = value;
            }
        }
    }
    let supplied = credential["addition"]
        .as_object()
        .ok_or_else(|| bad("授权数据缺少必要字段"))?;
    for (key, value) in supplied {
        addition[key] = value.clone();
    }
    let existing = storages(app)
        .await?
        .into_iter()
        .find(|s| s["mount_path"] == path);
    if let Some(existing) = &existing {
        let root = existing["mount_path"].as_str().unwrap_or("");
        if app.0.jobs.lock().await.values().any(|j| {
            (j.path.starts_with(&format!("{root}/"))
                || (["onedrive", "google"].contains(&key)
                    && j.path.starts_with(&format!("/{key}分享/"))))
                && matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
        }) {
            return Err(bad("请先暂停这个网盘的下载任务，再更换登录账号"));
        }
    }
    let mut storage=existing.clone().unwrap_or_else(||json!({"mount_path":path,"order":0,"cache_expiration":1,"web_proxy":true,"webdav_policy":"native_proxy","extract_folder":"front","enable_sign":true}));
    storage["driver"] = driver.into();
    storage["addition"] = serde_json::to_string(&addition).unwrap().into();
    storage["disabled"] = false.into();
    let created = app
        .0
        .gateway
        .post(
            if existing.is_some() {
                "/api/admin/storage/update"
            } else {
                "/api/admin/storage/create"
            },
            storage,
        )
        .await?;
    if app
        .0
        .gateway
        .post(
            "/api/fs/list",
            json!({"path":path,"page":1,"per_page":1,"password":"","refresh":true}),
        )
        .await
        .is_err()
    {
        if let Some(old) = existing {
            let _ = app.0.gateway.post("/api/admin/storage/update", old).await;
        } else if let Some(id) = created["id"].as_u64() {
            let _ = app
                .0
                .gateway
                .post(&format!("/api/admin/storage/delete?id={id}"), json!({}))
                .await;
        }
        return Err(bad(
            "账号授权验证失败，已恢复原连接，请重新登录或检查授权信息",
        ));
    }
    Ok(path)
}
async fn accounts(State(app): State<App>) -> ApiResult<Json<Value>> {
    let all = storages(&app).await?;
    let result:Vec<_>=PROVIDERS.iter().map(|(key,name)|{
        let storage=all.iter().find(|s|driver_provider(s["driver"].as_str().unwrap_or(""))==Some(*key)&&s["disabled"]!=true);
        let connected=storage.is_some_and(|s|s["status"]=="work");
        json!({"id":key,"name":name,"connected":connected,"mount_path":storage.map(|s|s["mount_path"].clone()),"status":if connected{"connected"}else{"login_required"},"methods":crate::auth::methods(key)})
    }).collect();
    Ok(Json(json!(result)))
}
#[derive(Deserialize)]
struct LoginStart {
    #[serde(default = "sms")]
    mode: String,
}
fn sms() -> String {
    "sms".into()
}
async fn login(
    State(app): State<App>,
    Path(key): Path<String>,
    Json(body): Json<LoginStart>,
) -> ApiResult<Json<Value>> {
    provider(&key)?;
    if !["sms", "qr", "oauth", "cookie", "credential"].contains(&body.mode.as_str()) {
        return Err(bad("登录方式不合法"));
    }
    let v = app
        .0
        .control
        .authenticate(
            "/sessions",
            Method::POST,
            Some(json!({"provider":key,"mode":body.mode})),
        )
        .await?;
    Ok(Json(v))
}
async fn login_status(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    finish_login(&app, id, None).await.map(Json)
}
async fn login_action(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(value): Json<Value>,
) -> ApiResult<Json<Value>> {
    finish_login(&app, id, Some(value)).await.map(Json)
}
async fn finish_login(app: &App, id: Uuid, action: Option<Value>) -> ApiResult<Value> {
    if action.is_none()
        && let Some(v) = app.0.control.logins.lock().await.get(&id).cloned()
    {
        return Ok(v);
    }
    let _lock = app.0.control.account_lock.lock().await;
    let path = format!(
        "/sessions/{id}{}",
        if action.is_some() { "/action" } else { "" }
    );
    let mut result = app
        .0
        .control
        .authenticate(
            &path,
            if action.is_some() {
                Method::POST
            } else {
                Method::GET
            },
            action,
        )
        .await?;
    if result["purpose"] == "login" && result["status"] == "ready" {
        let key = result["provider"]
            .as_str()
            .ok_or_else(|| bad("无效的登录会话"))?
            .to_owned();
        provider(&key)?;
        let credential = app
            .0
            .control
            .authenticate(&format!("/sessions/{id}/credentials"), Method::GET, None)
            .await?;
        let mount = match upsert_storage(app, &key, &credential, None).await {
            Ok(mount) => mount,
            Err(error) => {
                app.0.control.native_auth().reject(id).await;
                return Err(error);
            }
        };
        result["status"] = "connected".into();
        result["message"] = "登录成功，已连接网盘".into();
        result["mount_path"] = mount.into();
        let mut saved = app.0.control.logins.lock().await;
        saved.retain(|_, v| v["expires_at"].as_u64().unwrap_or(0) > now() * 1000);
        saved.insert(id, result.clone());
        drop(saved);
        let _ = app
            .0
            .control
            .authenticate(&format!("/sessions/{id}"), Method::DELETE, None)
            .await;
        spawn_pending(app, Some(&key)).await;
    }
    Ok(result)
}
async fn close_login(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    app.0.control.logins.lock().await.remove(&id);
    Ok(Json(
        app.0
            .control
            .authenticate(&format!("/sessions/{id}"), Method::DELETE, None)
            .await
            .unwrap_or(json!({"ok":true})),
    ))
}
async fn disconnect(State(app): State<App>, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    provider(&key)?;
    let _lock = app.0.control.account_lock.lock().await;
    let managed = storages(&app)
        .await?
        .into_iter()
        .filter(|s| {
            driver_provider(s["driver"].as_str().unwrap_or("")) == Some(key.as_str())
                || s["mount_path"]
                    .as_str()
                    .is_some_and(|p| p.starts_with(&format!("/{key}分享/")))
        })
        .collect::<Vec<_>>();
    let jobs = app.0.jobs.lock().await;
    if managed.iter().any(|s| {
        let mount = s["mount_path"].as_str().unwrap_or("");
        jobs.values().any(|j| {
            (j.path.starts_with(&format!("{mount}/"))
                || (["onedrive", "google"].contains(&key.as_str())
                    && j.path.starts_with(&format!("/{key}分享/"))))
                && matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
        })
    }) {
        return Err(bad("请先暂停这个网盘的下载任务，再退出账号"));
    }
    drop(jobs);
    for storage in managed {
        if storage["driver"] == "BaiduNative" {
            app.0.gateway.baidu.disconnect().await?;
            continue;
        }
        app.0
            .gateway
            .post(
                &format!(
                    "/api/admin/storage/delete?id={}",
                    storage["id"].as_u64().unwrap_or(0)
                ),
                json!({}),
            )
            .await?;
    }
    app.0
        .control
        .logins
        .lock()
        .await
        .retain(|_, v| v["provider"] != key);
    let _ = app
        .0
        .control
        .authenticate(&format!("/providers/{key}"), Method::DELETE, None)
        .await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct ShareInput {
    url: String,
    #[serde(default)]
    passcode: String,
}
pub fn parse_share(raw: &str) -> Result<(String, String, String)> {
    let start = raw.find("https://").context("请输入 HTTPS 网盘分享链接")?;
    let end = raw[start..]
        .find(|c: char| c.is_whitespace() || "，。；）】\"<>".contains(c))
        .map(|n| start + n)
        .unwrap_or(raw.len());
    let text = &raw[start..end];
    let url = Url::parse(text)?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none(),
        "分享链接不合法"
    );
    let host = url.host_str().unwrap_or("");
    let segments: Vec<_> = url
        .path_segments()
        .context("无效分享地址")?
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(provider) = crate::external::share_provider(&url)? {
        return Ok((provider.into(), url.to_string(), String::new()));
    }
    ensure!(
        segments.len() == 2
            && segments[0] == "s"
            && !segments[1].is_empty()
            && segments[1]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "请输入官方网盘分享链接"
    );
    let key = match host {
        "pan.quark.cn" => "quark",
        "pan.baidu.com" => "baidu",
        "115.com" | "115cdn.com" | "anxia.com" => "115",
        "pan.xunlei.com" => "thunder",
        _ => anyhow::bail!("请输入支持的网盘官方分享链接"),
    };
    let mut code = url
        .query_pairs()
        .find(|(k, _)| ["pwd", "password", "pass_code"].contains(&k.as_ref()))
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if code.is_empty()
        && let Some((_, tail)) = raw
            .split_once("提取码")
            .or_else(|| raw.split_once("访问码"))
    {
        code = tail
            .trim_start_matches(|c: char| c.is_whitespace() || "：:".contains(c))
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
    }
    Ok((key.into(), url.to_string(), code))
}
async fn shares(State(app): State<App>) -> Json<Value> {
    let shares = app.0.control.shares.lock().await;
    let mut rows: Vec<_> = shares.values().cloned().collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Json(json!(rows.iter().map(Share::public).collect::<Vec<_>>()))
}
async fn add_share(
    State(app): State<App>,
    Json(input): Json<ShareInput>,
) -> ApiResult<Json<Value>> {
    if input.url.len() > 8192 {
        return Err(bad("分享链接过长"));
    }
    let (key, url, detected) = parse_share(&input.url).map_err(|e| bad(&e.to_string()))?;
    let passcode = if input.passcode.is_empty() {
        detected
    } else {
        input.passcode
    };
    if passcode.len() > 16 || !passcode.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(bad("提取码格式不正确"));
    }
    let share = Share {
        id: Uuid::new_v4(),
        provider: key,
        url,
        passcode,
        status: "waiting_login".into(),
        message: "正在检查账号，随后读取分享内容供选择".into(),
        jobs: Vec::new(),
        roots: Vec::new(),
        entries: Vec::new(),
        capacity: Value::Null,
        transfer_approved: false,
        interaction: None,
        created_at: now(),
    };
    {
        let mut all = app.0.control.shares.lock().await;
        if all.len() >= 500 {
            return Err(bad("最多保留 500 个分享任务，请清理历史记录"));
        }
        if let Some(existing) = all
            .values()
            .find(|s| s.url == share.url && !matches!(s.status.as_str(), "failed" | "cancelled"))
        {
            return Ok(Json(existing.public()));
        }
        all.insert(share.id, share.clone());
        app.0.control.save(&all).await?;
    }
    spawn_pending(&app, Some(&share.provider)).await;
    Ok(Json(share.public()))
}
async fn retry_share(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let key = {
        let mut all = app.0.control.shares.lock().await;
        let s = all.get_mut(&id).ok_or_else(|| bad("分享任务不存在"))?;
        if ![
            "failed",
            "waiting_login",
            "cancelled",
            "ready",
            "queued",
            "awaiting_transfer",
            "insufficient_capacity",
        ]
        .contains(&s.status.as_str())
        {
            return Err(bad("分享任务仍在处理中"));
        }
        s.status = "waiting_login".into();
        s.interaction = None;
        let key = s.provider.clone();
        app.0.control.save(&all).await?;
        key
    };
    spawn_pending(&app, Some(&key)).await;
    Ok(Json(json!({"ok":true})))
}
async fn remove_share(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut all = app.0.control.shares.lock().await;
    let s = all.get_mut(&id).ok_or_else(|| bad("分享任务不存在"))?;
    if app.0.control.active.lock().await.contains(&id) {
        s.status = "cancelled".into();
        s.message = "已取消后续下载；网盘已保存的副本会保留".into();
    } else {
        all.remove(&id);
    }
    app.0.control.save(&all).await?;
    Ok(Json(json!({"ok":true})))
}
async fn update_share(
    app: &App,
    id: Uuid,
    status: &str,
    message: &str,
    interaction: Option<Uuid>,
    jobs: Option<Vec<Uuid>>,
) -> Result<()> {
    let mut all = app.0.control.shares.lock().await;
    if let Some(s) = all.get_mut(&id) {
        if s.status == "cancelled" {
            return Ok(());
        }
        s.status = status.into();
        s.message = message.into();
        s.interaction = interaction;
        if let Some(jobs) = jobs {
            s.jobs = jobs;
        }
    }
    app.0.control.save(&all).await
}
pub async fn spawn_pending(app: &App, key: Option<&str>) {
    let connected = if key.is_none() {
        match storages(app).await {
            Ok(all) => Some(
                all.into_iter()
                    .filter(|s| s["status"] == "work" && s["disabled"] != true)
                    .filter_map(|s| {
                        driver_provider(s["driver"].as_str().unwrap_or("")).map(str::to_owned)
                    })
                    .collect::<BTreeSet<_>>(),
            ),
            Err(_) => return,
        }
    } else {
        None
    };
    let ids: Vec<_> = app
        .0
        .control
        .shares
        .lock()
        .await
        .values()
        .filter(|s| {
            key.is_none_or(|key| s.provider == key)
                && connected
                    .as_ref()
                    .is_none_or(|keys| keys.contains(&s.provider))
                && matches!(s.status.as_str(), "waiting_login" | "needs_interaction")
        })
        .map(|s| s.id)
        .collect();
    for id in ids {
        let mut active = app.0.control.active.lock().await;
        if !active.insert(id) {
            continue;
        }
        drop(active);
        let app = app.clone();
        tokio::spawn(async move {
            let result = process_share(&app, id).await;
            if let Err(err) = result {
                let login = err.1.contains("登录") || err.1.contains("授权");
                let _ = update_share(
                    &app,
                    id,
                    if login { "waiting_login" } else { "failed" },
                    &err.1,
                    None,
                    None,
                )
                .await;
            }
            app.0.control.active.lock().await.remove(&id);
        });
    }
}
async fn process_share(app: &App, id: Uuid) -> ApiResult<()> {
    let s = app
        .0
        .control
        .shares
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| bad("分享任务不存在"))?;
    // Serialize Thunder token rotation with account changes and other imports.
    let _account = if s.provider == "thunder" {
        Some(app.0.control.account_lock.lock().await)
    } else {
        None
    };
    let Some(storage) = account_storage(app, &s.provider).await? else {
        update_share(
            app,
            id,
            "waiting_login",
            "请先登录这个网盘，登录后将读取内容供选择",
            None,
            None,
        )
        .await?;
        return Ok(());
    };
    if storage["status"] != "work" {
        update_share(
            app,
            id,
            "waiting_login",
            "网盘授权已失效，请重新登录，随后读取内容供选择",
            None,
            None,
        )
        .await?;
        return Ok(());
    }
    let mount = storage["mount_path"]
        .as_str()
        .context("网盘挂载无效")?
        .to_owned();
    update_share(app, id, "importing", "正在解析分享内容，请稍候", None, None).await?;
    let addition: Value = serde_json::from_str(storage["addition"].as_str().unwrap_or("{}"))
        .map_err(anyhow::Error::from)?;
    app.0
        .control
        .authenticate(
            &format!("/providers/{}/credentials", s.provider),
            Method::POST,
            Some(addition),
        )
        .await?;
    // A preview may require a cloud copy. Show its space cost before submitting
    // that mutation; approval starts preparation, never a download job.
    let mut reused = !s.jobs.is_empty() || !s.roots.is_empty();
    if reused && s.provider != "115" {
        let known = if s.roots.is_empty() {
            vec![format!(
                "{}/FlowDisk分享/分享-{id}",
                mount.trim_end_matches('/')
            )]
        } else {
            s.roots.clone()
        };
        for path in known {
            if app.0.gateway.metadata(&path).await.is_err() {
                reused = false;
                break;
            }
        }
    }
    if !["115", "onedrive", "google"].contains(&s.provider.as_str()) && !reused {
        let estimate = if s.provider == "thunder" {
            crate::thunder::import_share(
                &app.0.gateway,
                &app.0.data,
                &storage,
                id,
                &s.url,
                &s.passcode,
                true,
            )
            .await?
        } else if storage["driver"] == "BaiduNative" {
            app.0
                .gateway
                .baidu
                .inspect_share(
                    id,
                    &Url::parse(&s.url).map_err(anyhow::Error::from)?,
                    &s.passcode,
                )
                .await?
        } else {
            app.0
                .control
                .authenticate(
                    &format!("/providers/{}/share", s.provider),
                    Method::POST,
                    Some(json!({"id":id,"url":s.url,"passcode":s.passcode,"inspect_only":true})),
                )
                .await?
        };
        let usage = if estimate["total_space"].as_u64().is_some()
            && estimate["used_space"].as_u64().is_some()
        {
            estimate.clone()
        } else if storage["driver"] == "BaiduNative" {
            app.0.gateway.baidu.quota().await.unwrap_or(Value::Null)
        } else {
            storage["mount_details"].clone()
        };
        let total = usage["total_space"].as_u64();
        let used = usage["used_space"].as_u64();
        let required = estimate["required_bytes"]
            .as_u64()
            .context("无法估算分享转存大小")?;
        let capacity = match (total, used) {
            (Some(total), Some(used)) => {
                let free = total.saturating_sub(used);
                json!({"known":true,"total":total,"used":used,"free":free,"required":required,"after":free.saturating_sub(required),"enough":required<=free,"file_count":estimate["file_count"],"requires_transfer":true})
            }
            _ => {
                json!({"known":false,"required":required,"file_count":estimate["file_count"],"requires_transfer":true})
            }
        };
        let insufficient = capacity["enough"] == false;
        {
            let mut all = app.0.control.shares.lock().await;
            if let Some(current) = all.get_mut(&id)
                && current.status != "cancelled"
            {
                current.capacity = capacity;
                if insufficient || !s.transfer_approved {
                    current.status = if insufficient {
                        "insufficient_capacity"
                    } else {
                        "awaiting_transfer"
                    }
                    .into();
                    current.message = if insufficient {
                        "网盘容量不足，请先管理文件后重新检查"
                    } else {
                        "请查看容量，确认转存后即可浏览和预览文件"
                    }
                    .into();
                }
            }
            app.0.control.save(&all).await?;
        }
        if insufficient || !s.transfer_approved {
            return Ok(());
        }
    }
    let result = if ["onedrive", "google"].contains(&s.provider.as_str()) {
        let roots = app
            .0
            .gateway
            .external
            .prepare(&app.0.gateway, &storage, id, &s.url)
            .await?;
        json!({"absolute_paths":roots})
    } else if s.provider == "thunder" {
        let _files = app.0.control.file_lock.lock().await;
        crate::thunder::import_share(
            &app.0.gateway,
            &app.0.data,
            &storage,
            id,
            &s.url,
            &s.passcode,
            false,
        )
        .await?
    } else if storage["driver"] == "BaiduNative" {
        let url = Url::parse(&s.url).map_err(anyhow::Error::from)?;
        json!({"paths":app.0.gateway.baidu.import_share(id,&url,&s.passcode).await?})
    } else {
        app.0
            .control
            .authenticate(
                &format!("/providers/{}/share", s.provider),
                Method::POST,
                Some(json!({"id":id,"url":s.url,"passcode":s.passcode})),
            )
            .await?
    };
    let paths = if let Some(paths) = result["absolute_paths"].as_array() {
        paths
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    } else if result["share_mount"].is_object() {
        let path = format!("/{}分享/{id}", s.provider);
        let mount = upsert_storage(app, &s.provider, &result["share_mount"], Some(&path)).await?;
        vec![mount]
    } else {
        result["paths"]
            .as_array()
            .context("尚未获得保存位置，请在官方网盘完成转存")?
            .iter()
            .map(|v| {
                let path = v.as_str().unwrap_or("");
                format!("{}{}", mount.trim_end_matches('/'), path)
            })
            .collect()
    };
    if app
        .0
        .control
        .shares
        .lock()
        .await
        .get(&id)
        .is_none_or(|s| s.status == "cancelled")
    {
        return Ok(());
    }
    let _files = app.0.control.file_lock.lock().await;
    {
        let mut all = app.0.control.shares.lock().await;
        if let Some(s) = all.get_mut(&id)
            && s.status != "cancelled"
        {
            s.roots = paths.clone();
        }
        app.0.control.save(&all).await?;
    }
    let entries = crate::review::collect(app, &paths).await?;
    let mut all = app.0.control.shares.lock().await;
    if let Some(s) = all.get_mut(&id)
        && s.status != "cancelled"
    {
        s.roots = paths;
        s.entries = entries;
        s.status = "ready".into();
        s.message = "内容已就绪，请预览并选择要下载的文件".into();
    }
    app.0.control.save(&all).await?;
    Ok(())
}
pub async fn enqueue_paths(
    app: &App,
    paths: Vec<String>,
    group: Option<(Uuid, String)>,
    create_folder: bool,
) -> ApiResult<Vec<Uuid>> {
    let (group_id, group_name) = group.unwrap_or_else(|| {
        (
            Uuid::new_v4(),
            if paths.len() == 1 {
                format!(
                    "下载 · {}",
                    paths[0]
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("下载")
                )
            } else {
                format!("批量下载 · {} 项", paths.len())
            },
        )
    });
    let roots = paths.clone();
    let mut single_directory = None;
    let mut pending = paths;
    let mut found = Vec::new();
    let mut visited = BTreeSet::new();
    while let Some(path) = pending.pop() {
        if !valid_path(&path) {
            return Err(bad("文件路径不合法"));
        }
        if !visited.insert(path.clone()) {
            continue;
        }
        if visited.len() > 5000 {
            return Err(bad("目录层级或项目过多，请选择更具体的目录"));
        }
        let meta = app.0.gateway.metadata(&path).await?;
        if !meta.is_dir {
            found.push((path, meta.name, meta.size));
            if found.len() > 1000 {
                return Err(bad("一次最多下载 1000 个文件，请拆分任务"));
            }
            continue;
        }
        if roots.len() == 1 && roots[0] == path {
            single_directory = Some(path.clone());
        }
        let list = app
            .0
            .gateway
            .post(
                "/api/fs/list",
                json!({"path":path,"page":1,"per_page":0,"password":"","refresh":true}),
            )
            .await?;
        for file in list["content"].as_array().into_iter().flatten() {
            let name = file["name"].as_str().context("无效的文件名")?;
            if !remote_name(name) {
                return Err(bad("网盘文件名不合法"));
            }
            pending.push(format!("{}/{name}", path.trim_end_matches('/')));
        }
    }
    if found.is_empty() {
        return Err(bad("所选目录中没有可下载的文件"));
    }
    let mut jobs = app.0.jobs.lock().await;
    let folder = if create_folder && found.len() > 1 {
        crate::local::batch_folder(&group_name, group_id)
    } else {
        String::new()
    };
    let common = single_directory.unwrap_or_else(|| {
        crate::local::common_parent(&roots.iter().map(String::as_str).collect::<Vec<_>>())
    });
    let mut ids = Vec::new();
    let mut fresh = Vec::new();
    let new_count = found
        .iter()
        .filter(|(path, _, _)| !jobs.values().any(|j| j.path == *path))
        .count();
    if jobs.len() + new_count > 1000 {
        return Err(bad("下载任务已达上限，请先清理任务记录"));
    }
    for (path, name, size) in found {
        if let Some(job) = jobs.values_mut().find(|j| j.path == path) {
            job.group_id = Some(group_id);
            job.group_name = Some(group_name.clone());
            ids.push(job.id);
            if job.status == "completed"
                && !fs::try_exists(crate::local::job_file(app, job)).await?
            {
                job.status = "paused".into();
                job.downloaded = 0;
                job.sha256 = None;
            }
            if matches!(job.status.as_str(), "paused" | "failed") {
                fresh.push(job.id);
            }
            continue;
        }
        let relative = path
            .strip_prefix(&common)
            .unwrap_or(&name)
            .trim_start_matches('/');
        let parent = relative.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let dir = [folder.as_str(), parent]
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(safe_name_path)
            .collect::<Vec<_>>()
            .join("/");
        crate::local::checked_path(&app.0.downloads, &dir, true).await?;
        let output_name = crate::local::available_name(app, &jobs, &dir, &safe_name(&name)).await?;
        let job = Job {
            id: Uuid::new_v4(),
            path,
            name: output_name,
            local_dir: Some(dir),
            status: "paused".into(),
            size: Some(size),
            downloaded: 0,
            speed: 0,
            connections: 0,
            error: None,
            sha256: None,
            created_at: now(),
            group_id: Some(group_id),
            group_name: Some(group_name.clone()),
            passwords: vec![],
            stats: crate::SpeedStats::default(),
        };
        ids.push(job.id);
        fresh.push(job.id);
        jobs.insert(job.id, job);
    }
    app.save(&jobs).await?;
    drop(jobs);
    for id in fresh {
        app.start(id).await?;
    }
    Ok(ids)
}
fn safe_name_path(path: &str) -> String {
    path.split('/').map(safe_name).collect::<Vec<_>>().join("/")
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 1024
        && !name.chars().any(|c| c.is_control() || "/\\".contains(c))
}
// Existing remote names travel in JSON. Local output names are sanitized separately.
// Cloud drives can contain control characters that must not block the entire folder.
pub fn remote_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 1024
        && !name.contains(['\0', '/', '\\'])
}
#[derive(Deserialize)]
struct SmbInput {
    name: String,
    host: String,
    #[serde(default = "smb_port")]
    port: u16,
    share: String,
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    root: String,
}
fn smb_port() -> u16 {
    445
}
async fn connections(State(app): State<App>) -> ApiResult<Json<Value>> {
    let rows=storages(&app).await?.into_iter().filter(|s|s["driver"]=="SMB").map(|s|{
        let addition:Value=serde_json::from_str(s["addition"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
        json!({"id":s["id"],"name":s["mount_path"].as_str().unwrap_or("").trim_start_matches('/'),"mount_path":s["mount_path"],"status":if s["disabled"]==true{"disabled"}else if s["status"]=="work"{"connected"}else{"unavailable"},"address":addition["address"],"share":addition["share_name"]})
    }).collect::<Vec<_>>();
    Ok(Json(json!(rows)))
}
async fn connect_smb(
    State(app): State<App>,
    Json(input): Json<SmbInput>,
) -> ApiResult<Json<Value>> {
    if !valid_name(&input.name)
        || input.name.len() > 80
        || input.host.len() > 253
        || input.port == 0
        || !valid_name(&input.share)
        || input.username.is_empty()
        || input.username.len() > 256
        || input.password.len() > 4096
        || input.root.len() > 1024
        || input.root.contains(['\\', '\0'])
        || input.root.split('/').any(|p| p == "..")
    {
        return Err(bad("SMB 名称、地址、共享名或凭据格式无效"));
    }
    let host = reqwest::Url::parse(&format!("smb://{}:{}", input.host, input.port))
        .map_err(|_| bad("SMB 服务器地址无效，请填写 IP 或主机名"))?;
    if host.username() != ""
        || host.password().is_some()
        || !matches!(host.path(), "" | "/")
        || host.query().is_some()
        || host.fragment().is_some()
        || host.host().is_none()
    {
        return Err(bad("SMB 地址不能包含凭据、路径或查询参数"));
    }
    let mount = format!("/SMB-{}", input.name);
    let _lock = app.0.control.account_lock.lock().await;
    let credential = json!({"driver":"SMB","addition":{"address":format!("{}:{}",host.host().unwrap(),input.port),"share_name":input.share,"username":input.username,"password":input.password,"root_folder_path":if input.root.is_empty(){"."}else{&input.root}}});
    upsert_storage(&app, "smb", &credential, Some(&mount)).await?;
    Ok(Json(json!({"ok":true,"mount_path":mount})))
}
async fn disconnect_smb(State(app): State<App>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    let _lock = app.0.control.account_lock.lock().await;
    let storage = storages(&app)
        .await?
        .into_iter()
        .find(|s| s["id"].as_u64() == Some(id) && s["driver"] == "SMB")
        .ok_or_else(|| bad("SMB 连接不存在"))?;
    let mount = storage["mount_path"].as_str().unwrap_or("");
    if app.0.jobs.lock().await.values().any(|j| {
        j.path.starts_with(&format!("{mount}/"))
            && matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
    }) {
        return Err(bad("请先暂停这个 SMB 连接的下载任务"));
    }
    app.0
        .gateway
        .post(&format!("/api/admin/storage/delete?id={id}"), json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/connections", get(connections))
        .route("/connections/smb", post(connect_smb))
        .route("/connections/{id}", axum::routing::delete(disconnect_smb))
        .route("/accounts", get(accounts))
        .route("/accounts/{key}/login", post(login))
        .route("/accounts/{key}", axum::routing::delete(disconnect))
        .route("/logins/{id}", get(login_status).delete(close_login))
        .route("/logins/{id}/action", post(login_action))
        .route("/shares", get(shares).post(add_share))
        .route("/shares/{id}", axum::routing::delete(remove_share))
        .route("/shares/{id}/retry", post(retry_share))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn share_inputs_are_scoped_to_official_hosts() {
        assert!(remote_name("\u{7f}remote.txt"));
        assert!(!valid_name("\u{7f}remote.txt"));
        for name in ["..", ".", "a/b", "a\\b", "\0bad"] {
            assert!(!remote_name(name));
        }
        let (p,u,c)=parse_share("分享：https://pan.quark.cn/s/fixture-share#/list/share/00000000000000000000000000000000 提取码：abcd").unwrap();
        assert_eq!(p, "quark");
        assert!(u.contains("#/list/share/"));
        assert_eq!(c, "abcd");
        for url in [
            "http://pan.quark.cn/s/abc",
            "https://pan.quark.cn.evil.test/s/abc",
            "https://127.0.0.1/s/abc",
            "https://evil@pan.quark.cn/s/abc",
            "https://pan.quark.cn:8443/s/abc",
        ] {
            assert!(parse_share(url).is_err(), "{url}");
        }
        assert!(!valid_name("../secret"));
        assert!(!valid_name("x\n"));
        assert!(valid_name("资料.zip"));
    }
}
