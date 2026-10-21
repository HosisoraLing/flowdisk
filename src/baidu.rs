//! Cookie-based Baidu adapter: no browser process, application secret, or third-party parser.
use crate::{
    gateway::{Link, Metadata},
    now, valid_path,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{
    Client, Method, Url,
    header::{COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{fs, sync::Mutex};
use uuid::Uuid;

pub const MOUNT: &str = "/baidu";
pub const NETDISK_UA: &str = "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;";
const BROWSER: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/132.0.0.0 Safari/537.36";
#[derive(Clone, Serialize, Deserialize)]
struct Account {
    cookie: String,
    token: String,
}
pub struct Baidu {
    client: Client,
    account: Mutex<Option<Account>>,
    valid: AtomicBool,
    path: PathBuf,
    cache: Mutex<BTreeMap<String, (u64, Value)>>,
    base: Url,
}
pub fn owns(path: &str) -> bool {
    path == MOUNT || path.starts_with("/baidu/")
}
fn remote(path: &str) -> Result<&str> {
    ensure!(valid_path(path) && owns(path), "百度文件路径不合法");
    Ok(path
        .strip_prefix(MOUNT)
        .filter(|p| !p.is_empty())
        .unwrap_or("/"))
}
pub fn cookie_map(raw: &str) -> Result<BTreeMap<String, String>> {
    ensure!(
        raw.len() <= 12000 && !raw.contains(['\r', '\n']),
        "百度授权格式不合法"
    );
    let raw = raw.trim();
    let raw = raw.strip_prefix("Cookie:").unwrap_or(raw).trim();
    let map: BTreeMap<_, _> = raw
        .split(';')
        .filter_map(|part| {
            part.trim()
                .split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect();
    ensure!(
        map.get("BDUSS").is_some_and(|v| v.len() >= 8),
        "请提供含 BDUSS 的百度 Cookie，或使用扫码登录"
    );
    HeaderValue::from_str(raw).context("百度 Cookie 格式不合法")?;
    Ok(map)
}
pub fn cookies(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}
fn collect(response: &reqwest::Response, map: &mut BTreeMap<String, String>) {
    for h in response.headers().get_all("set-cookie") {
        if let Ok(value) = h.to_str()
            && let Some((k, v)) = value.split(';').next().unwrap_or("").split_once('=')
        {
            map.insert(k.into(), v.into());
        }
    }
}
pub fn parse_jsonp(text: &str) -> Result<Value> {
    let start = text.find('{').context("百度响应格式已变化")?;
    let end = text.rfind('}').context("百度响应格式已变化")?;
    ensure!(start <= end, "百度响应不合法");
    serde_json::from_str(&text[start..=end]).context("百度响应不是有效 JSON")
}
fn checked(v: Value) -> Result<Value> {
    let n = v["errno"]
        .as_i64()
        .or_else(|| v["error_code"].as_i64())
        .context("百度接口响应缺少状态码")?;
    if n == 0 {
        return Ok(v);
    }
    let message = match n {
        -6 | 111 | 110 | 9019 => "百度登录已失效，请重新扫码登录",
        -9 => "百度分享提取码错误",
        -62 | -19 | -63 | 8001 => "百度要求安全验证，请在官方网盘完成验证后重试",
        -7 | 2 => "百度文件或分享不存在，可能已失效",
        -30 => "目标位置已有同名文件，请更换名称",
        _ => "百度拒绝此操作，请检查权限、空间或账号限制",
    };
    bail!("{message}（代码 {n}）")
}
fn scalar(v: &Value) -> String {
    v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
}
fn native_entry(v: &Value) -> Result<Value> {
    let name = v["server_filename"]
        .as_str()
        .or_else(|| v["path"].as_str().and_then(|s| s.rsplit('/').next()))
        .context("百度文件名缺失")?;
    ensure!(crate::control::valid_name(name), "百度文件名不合法");
    Ok(
        json!({"name":name,"size":v["size"].as_u64().unwrap_or(0),"is_dir":v["isdir"]==1||v["isdir"]==true,"modified":v["server_mtime"].as_u64().unwrap_or(0)*1000,"hash_info":{"md5":v["md5"]},"raw_url":""}),
    )
}
impl Baidu {
    pub fn new(data: &Path) -> Result<Self> {
        let path = data.join("baidu.json");
        let account = match std::fs::read(&path) {
            Ok(data) => Some(
                serde_json::from_slice::<Account>(&data).context("百度账号文件损坏，拒绝覆盖")?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(a) = &account {
            cookie_map(&a.cookie)?;
        }
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(NETDISK_UA)
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(30))
                .build()?,
            valid: AtomicBool::new(account.is_some()),
            account: Mutex::new(account),
            path,
            cache: Mutex::new(BTreeMap::new()),
            base: Url::parse("https://pan.baidu.com/")?,
        })
    }
    pub async fn storage(&self) -> Option<Value> {
        self.account.lock().await.as_ref().map(|a|json!({"id":0,"mount_path":MOUNT,"driver":"BaiduNative","disabled":false,"status":if self.valid.load(Ordering::Relaxed){"work"}else{"login_required"},"addition":serde_json::to_string(&json!({"cookie":a.cookie})).unwrap()}))
    }
    pub async fn connected(&self) -> bool {
        self.account.lock().await.is_some()
    }
    async fn current(&self) -> Result<Account> {
        self.account
            .lock()
            .await
            .clone()
            .context("请先登录百度网盘")
    }
    async fn save(&self, account: &Account) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(account)?).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        fs::File::open(&tmp).await?.sync_all().await?;
        fs::rename(tmp, &self.path).await?;
        Ok(())
    }
    pub async fn connect(&self, raw: &str) -> Result<()> {
        let mut map = cookie_map(raw)?;
        let u = self.base.join("api/gettemplatevariable")?;
        let u = Url::parse_with_params(
            u.as_str(),
            [
                ("clienttype", "0"),
                ("app_id", "250528"),
                ("web", "1"),
                ("fields", "[\"bdstoken\",\"uk\"]"),
            ],
        )?;
        let value = self.call(u, Method::GET, None, &mut map).await?;
        let token = value["result"]["bdstoken"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("百度未返回登录授权，请重新扫码或检查 Cookie")?;
        let account = Account {
            cookie: cookies(&map),
            token: token.into(),
        };
        self.list_page(&account, "/", 1, 1).await?;
        let mut saved = self.account.lock().await;
        self.save(&account).await?;
        *saved = Some(account);
        self.valid.store(true, Ordering::Relaxed);
        self.cache.lock().await.clear();
        Ok(())
    }
    pub async fn disconnect(&self) -> Result<()> {
        let mut saved = self.account.lock().await;
        match fs::remove_file(&self.path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        };
        *saved = None;
        self.valid.store(false, Ordering::Relaxed);
        self.cache.lock().await.clear();
        Ok(())
    }
    async fn call(
        &self,
        url: Url,
        method: Method,
        form: Option<&[(String, String)]>,
        map: &mut BTreeMap<String, String>,
    ) -> Result<Value> {
        let mut request = self
            .client
            .request(method, url)
            .header(COOKIE, cookies(map))
            .header(REFERER, "https://pan.baidu.com/disk/main");
        if let Some(form) = form {
            request = request.form(form);
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("无法连接百度网盘，请稍后重试"))?;
        collect(&response, map);
        ensure!(
            response.status().is_success(),
            "百度网盘请求失败（HTTP {}）",
            response.status().as_u16()
        );
        let v: Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("百度接口响应已变化或要求安全验证"))?;
        checked(v)
    }
    async fn api(
        &self,
        a: &Account,
        path: &str,
        params: Vec<(&str, String)>,
        form: Option<Vec<(String, String)>>,
    ) -> Result<Value> {
        let mut u = self.base.join(path)?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("app_id", "250528")
                .append_pair("web", "1")
                .append_pair("clienttype", "0")
                .append_pair("bdstoken", &a.token);
            for (k, v) in params {
                q.append_pair(k, &v);
            }
        }
        let result = self
            .call(
                u,
                if form.is_some() {
                    Method::POST
                } else {
                    Method::GET
                },
                form.as_deref(),
                &mut cookie_map(&a.cookie)?,
            )
            .await;
        if result
            .as_ref()
            .is_err_and(|e| e.to_string().contains("登录已失效"))
            && self
                .account
                .lock()
                .await
                .as_ref()
                .is_some_and(|saved| saved.cookie == a.cookie)
        {
            self.valid.store(false, Ordering::Relaxed);
        }
        result
    }
    async fn list_page(&self, a: &Account, path: &str, page: u64, size: u64) -> Result<Vec<Value>> {
        let v = self
            .api(
                a,
                "api/list",
                vec![
                    ("dir", path.into()),
                    ("page", page.to_string()),
                    ("num", size.to_string()),
                    ("order", "name".into()),
                    ("desc", "0".into()),
                    ("showempty", "0".into()),
                ],
                None,
            )
            .await?;
        let list = v["list"]
            .as_array()
            .context("百度目录响应缺少文件列表")?
            .clone();
        let mut cache = self.cache.lock().await;
        if cache.len() + list.len() > 10000 {
            cache.clear();
        }
        for item in &list {
            if let Some(path) = item["path"].as_str() {
                ensure!(valid_path(path), "百度返回了不合法文件路径");
                cache.insert(path.into(), (now(), item.clone()));
            }
        }
        Ok(list)
    }
    async fn item(&self, a: &Account, path: &str) -> Result<Value> {
        if path == "/" {
            return Ok(json!({"server_filename":"baidu","path":"/","isdir":1,"size":0}));
        }
        if let Some((_, v)) = self
            .cache
            .lock()
            .await
            .get(path)
            .filter(|(t, _)| now() < *t + 30)
        {
            return Ok(v.clone());
        }
        let parent = path.rsplit_once('/').unwrap().0;
        for page in 1..=100 {
            let list = self
                .list_page(a, if parent.is_empty() { "/" } else { parent }, page, 1000)
                .await?;
            if let Some(item) = list.iter().find(|v| v["path"] == path) {
                return Ok(item.clone());
            }
            if list.len() < 1000 {
                break;
            }
        }
        bail!("百度文件不存在，请刷新目录")
    }
    pub async fn fs(&self, route: &str, b: &Value) -> Result<Value> {
        let a = self.current().await?;
        let path = remote(
            b["path"]
                .as_str()
                .or_else(|| b["src_dir"].as_str())
                .or_else(|| b["dir"].as_str())
                .unwrap_or(""),
        )?;
        match route {
            "/api/fs/list" => {
                if b["refresh"] == true {
                    self.cache.lock().await.clear();
                }
                let page = b["page"].as_u64().unwrap_or(1);
                let size = b["per_page"].as_u64().unwrap_or(100);
                let mut all = Vec::new();
                if size == 0 {
                    for p in 1..=100 {
                        let list = self.list_page(&a, path, p, 1000).await?;
                        let len = list.len();
                        all.extend(list);
                        ensure!(all.len() <= 10000, "百度目录过大，请选择更具体的目录");
                        if len < 1000 {
                            break;
                        }
                        ensure!(p < 100, "百度目录项目过多");
                    }
                } else {
                    all = self.list_page(&a, path, page, size.min(1000)).await?;
                }
                let total = if size > 0 && all.len() == size as usize {
                    page * size + 1
                } else {
                    (page.saturating_sub(1)) * size + all.len() as u64
                };
                Ok(
                    json!({"content":all.iter().map(native_entry).collect::<Result<Vec<_>>>()?,"total":total}),
                )
            }
            "/api/fs/get" => {
                let item = self.item(&a, path).await?;
                let mut v = native_entry(&item)?;
                v["modified"] = scalar(&item["server_mtime"]).into();
                Ok(v)
            }
            "/api/fs/mkdir" => {
                self.mkdir(&a, path).await?;
                Ok(json!({}))
            }
            "/api/fs/rename" => {
                ensure!(path != "/", "不能重命名百度根目录");
                let name = b["name"].as_str().context("缺少名称")?;
                ensure!(crate::control::valid_name(name), "文件名不合法");
                let target = format!("{}/{}", path.rsplit_once('/').unwrap().0, name);
                self.ensure_absent(&a, &target).await?;
                let form = json!([{"path":path,"newname":name}]);
                self.manager(&a, "rename", form).await?;
                Ok(json!({}))
            }
            "/api/fs/move" => {
                let dest = remote(b["dst_dir"].as_str().context("缺少目标目录")?)?;
                let names = b["names"].as_array().context("缺少文件列表")?;
                let mut list = Vec::new();
                for name in names {
                    let name = name.as_str().context("文件名不合法")?;
                    ensure!(crate::control::valid_name(name), "文件名不合法");
                    self.ensure_absent(&a, &format!("{}/{name}", dest.trim_end_matches('/')))
                        .await?;
                    list.push(json!({"path":format!("{}/{name}",path.trim_end_matches('/')),"dest":dest,"newname":name}));
                }
                self.manager(&a, "move", json!(list)).await?;
                Ok(json!({}))
            }
            "/api/fs/remove" => {
                let names = b["names"].as_array().context("缺少文件列表")?;
                let mut list = Vec::new();
                for name in names {
                    let name = name.as_str().context("文件名不合法")?;
                    ensure!(crate::control::valid_name(name), "文件名不合法");
                    list.push(format!("{}/{name}", path.trim_end_matches('/')));
                }
                self.manager(&a, "delete", json!(list)).await?;
                Ok(json!({}))
            }
            _ => bail!("百度不支持此文件操作"),
        }
    }
    async fn manager(&self, a: &Account, operation: &str, list: Value) -> Result<()> {
        let v = self
            .api(
                a,
                "api/filemanager",
                vec![
                    ("opera", operation.into()),
                    ("async", "0".into()),
                    ("onnest", "fail".into()),
                ],
                Some(vec![("filelist".into(), list.to_string())]),
            )
            .await?;
        if let Some(info) = v["info"].as_array() {
            ensure!(
                info.iter().all(|v| v["errno"].as_i64().unwrap_or(0) == 0),
                "部分百度文件操作失败，请刷新目录检查"
            );
        }
        self.cache.lock().await.clear();
        Ok(())
    }
    async fn mkdir(&self, a: &Account, path: &str) -> Result<()> {
        self.api(
            a,
            "api/create",
            vec![("a", "commit".into())],
            Some(vec![
                ("path".into(), path.into()),
                ("isdir".into(), "1".into()),
                ("block_list".into(), "[]".into()),
            ]),
        )
        .await?;
        self.cache.lock().await.clear();
        Ok(())
    }
    async fn ensure_absent(&self, a: &Account, path: &str) -> Result<()> {
        let parent = path.rsplit_once('/').unwrap().0;
        for page in 1..=100 {
            let list = self
                .list_page(a, if parent.is_empty() { "/" } else { parent }, page, 1000)
                .await?;
            ensure!(
                !list.iter().any(|v| v["path"] == path),
                "目标位置已有同名内容，拒绝覆盖"
            );
            if list.len() < 1000 {
                return Ok(());
            }
        }
        bail!("目录过大，无法确认是否存在同名文件")
    }
    pub async fn upload(&self, path: &str, bytes: axum::body::Bytes) -> Result<Value> {
        let a = self.current().await?;
        let path = remote(path)?;
        self.ensure_absent(&a, path).await?;
        let boundary = format!("FlowDisk{}", Uuid::new_v4().simple());
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = if self.base.host_str() == Some("pan.baidu.com") {
            Url::parse("https://pcs.baidu.com/rest/2.0/pcs/file")?
        } else {
            self.base.join("rest/2.0/pcs/file")?
        };
        let response = self
            .client
            .post(url)
            .query(&[
                ("method", "upload"),
                ("app_id", "250528"),
                ("path", path),
                ("ondup", "fail"),
            ])
            .header(COOKIE, &a.cookie)
            .header(USER_AGENT, "netdisk")
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .timeout(Duration::from_secs(600))
            .body(body)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        ensure!(
            response.status().is_success(),
            "百度上传失败（HTTP {}）",
            response.status().as_u16()
        );
        let value: Value = response.json().await.map_err(|e| e.without_url())?;
        if value.get("error_code").is_some() {
            checked(value.clone())?;
        }
        ensure!(
            value["path"].as_str() == Some(path)
                && value["size"].as_u64() == Some(bytes.len() as u64),
            "百度未确认文件完整保存，请刷新目录后检查"
        );
        self.cache.lock().await.clear();
        Ok(json!({"ok":true}))
    }
    async fn locate(&self, a: &Account, path: &str) -> Result<Url> {
        let url = if self.base.host_str() == Some("pan.baidu.com") {
            Url::parse("https://pcs.baidu.com/rest/2.0/pcs/file")?
        } else {
            self.base.join("rest/2.0/pcs/file")?
        };
        let response = self
            .client
            .get(url)
            .query(&[
                ("method", "locatedownload"),
                ("app_id", "250528"),
                ("path", path),
                ("ver", "4.0"),
            ])
            .header(COOKIE, &a.cookie)
            .header(USER_AGENT, NETDISK_UA)
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .map_err(|e| e.without_url())?;
        ensure!(response.status().is_success(), "百度原文件取链暂时不可用");
        let value: Value = response.json().await.map_err(|e| e.without_url())?;
        value["urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v["encrypt"].as_u64().unwrap_or(0) == 0)
            .filter_map(|v| v["url"].as_str())
            .filter_map(|u| Url::parse(u).ok())
            .find(official_download)
            .context("百度未返回安全的原文件地址")
    }
    async fn download_connections(&self, a: &Account) -> usize {
        if let Ok(n) = std::env::var("BAIDU_CONNECTIONS")
            .unwrap_or_default()
            .parse::<usize>()
        {
            return n.clamp(1, 16);
        }
        // Use the account's actual membership: free accounts benefit from avoiding
        // simultaneous requests rather than amplifying account throttling.
        match self
            .api(
                a,
                "rest/2.0/membership/user/info",
                vec![("method", "query".into())],
                None,
            )
            .await
        {
            Ok(value) if value["user_info"]["is_svip"] == 1 => 8,
            Ok(value) if value["user_info"]["is_vip"] == 1 => 4,
            _ => 1,
        }
    }
    pub async fn resolve(&self, path: &str) -> Result<(Metadata, Link)> {
        let a = self.current().await?;
        let path = remote(path)?;
        let item = self.item(&a, path).await?;
        let mut entry = native_entry(&item)?;
        entry["modified"] = scalar(&item["server_mtime"]).into();
        let meta: Metadata = serde_json::from_value(entry)?;
        ensure!(!meta.is_dir, "目录不能作为文件下载");
        let (url, agent) = match self.locate(&a, path).await {
            Ok(url) => (url, NETDISK_UA),
            Err(_) => {
                let value = self
                    .api(
                        &a,
                        "api/filemetas",
                        vec![("target", json!([path]).to_string()), ("dlink", "1".into())],
                        None,
                    )
                    .await?;
                let link = value["info"]
                    .as_array()
                    .and_then(|v| v.first())
                    .and_then(|v| v["dlink"].as_str())
                    .context("百度未返回原文件下载链接，请检查账号权限")?;
                let url = Url::parse(link).context("百度下载链接不合法")?;
                ensure!(
                    official_download(&url),
                    "百度返回了非官方或不安全的下载地址"
                );
                (url, "netdisk")
            }
        };
        let concurrency = self.download_connections(&a).await;
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_str(&a.cookie)?);
        // Locate signatures bind the exact acquisition UA. Filemetas links use
        // the separate compatible download UA; do not interchange these headers.
        headers.insert(USER_AGENT, HeaderValue::from_static(agent));
        headers.insert(REFERER, HeaderValue::from_static("https://pan.baidu.com/"));
        Ok((
            meta,
            Link {
                url,
                headers,
                concurrency,
                max_range: 4 * 1024 * 1024,
            },
        ))
    }
    pub async fn import_share(&self, id: Uuid, url: &Url, passcode: &str) -> Result<Vec<String>> {
        let v = self.share_request(id, url, passcode, false).await?;
        Ok(serde_json::from_value(v["paths"].clone())?)
    }
    pub async fn inspect_share(&self, id: Uuid, url: &Url, passcode: &str) -> Result<Value> {
        self.share_request(id, url, passcode, true).await
    }
    pub async fn quota(&self) -> Result<Value> {
        let a = self.current().await?;
        let v = self.api(&a, "api/quota", vec![], None).await?;
        Ok(json!({"total_space":v["total"],"used_space":v["used"]}))
    }
    async fn share_request(
        &self,
        id: Uuid,
        url: &Url,
        passcode: &str,
        inspect: bool,
    ) -> Result<Value> {
        let a = self.current().await?;
        let share_code = url
            .path()
            .strip_prefix("/s/1")
            .filter(|v| {
                !v.is_empty()
                    && v.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            })
            .context("百度分享链接不合法")?;
        let mut map = cookie_map(&a.cookie)?;
        if !passcode.is_empty() {
            let u = Url::parse_with_params(
                self.base.join("share/verify")?.as_str(),
                [("surl", share_code), ("web", "1"), ("clienttype", "0")],
            )?;
            let v = self
                .call(
                    u,
                    Method::POST,
                    Some(&[
                        ("pwd".into(), passcode.into()),
                        ("vcode".into(), String::new()),
                        ("vcode_str".into(), String::new()),
                    ]),
                    &mut map,
                )
                .await?;
            let randsk = v["randsk"].as_str().context("百度未返回分享访问凭据")?;
            map.insert("BDCLND".into(), randsk.into());
        }
        let share_page = self.base.join(&format!("s/1{share_code}"))?;
        let response = self
            .client
            .get(share_page)
            .header(COOKIE, cookies(&map))
            .header(REFERER, "https://pan.baidu.com/disk/home")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("百度分享页面连接失败"))?;
        ensure!(
            response.status().is_success(),
            "百度分享需要登录、提取码或已失效，请检查分享"
        );
        collect(&response, &mut map);
        let text = response
            .text()
            .await
            .map_err(|_| anyhow::anyhow!("百度分享页面读取失败"))?;
        let info = share_data(&text)?;
        let shareid = scalar(&info["shareid"]);
        let owner = scalar(info.get("share_uk").unwrap_or(&info["uk"]));
        ensure!(
            shareid.bytes().all(|v| v.is_ascii_digit())
                && owner.bytes().all(|v| v.is_ascii_digit())
                && !shareid.is_empty()
                && !owner.is_empty(),
            "百度分享标识缺失或已失效"
        );
        let mut files = Vec::new();
        for page in 1..=100 {
            let u = Url::parse_with_params(
                self.base.join("share/list")?.as_str(),
                [
                    ("shareid", shareid.clone()),
                    ("uk", owner.clone()),
                    ("root", "1".into()),
                    ("page", page.to_string()),
                    ("num", "100".into()),
                    ("order", "name".into()),
                    ("desc", "0".into()),
                    ("web", "1".into()),
                    ("clienttype", "0".into()),
                ],
            )?;
            let v = self.call(u, Method::GET, None, &mut map).await?;
            let list = v["list"].as_array().context("百度分享目录缺失")?;
            files.extend(list.clone());
            ensure!(files.len() <= 1000, "分享超过 1000 项，请拆分分享后再试");
            if list.len() < 100 {
                break;
            }
            ensure!(page < 100, "分享目录过大");
        }
        ensure!(!files.is_empty(), "百度分享目录为空");
        if inspect {
            let mut pending = files;
            let mut seen = std::collections::BTreeSet::new();
            let mut bytes = 0u64;
            let mut count = 0;
            while let Some(file) = pending.pop() {
                ensure!(
                    seen.insert(scalar(&file["fs_id"])) && seen.len() <= 5000,
                    "分享过大或目录重复，请拆分分享"
                );
                if file["isdir"] == 1 {
                    let dir = file["path"].as_str().context("百度分享文件夹路径缺失")?;
                    for page in 1..=50 {
                        let u = Url::parse_with_params(
                            self.base.join("share/list")?.as_str(),
                            [
                                ("shareid", shareid.clone()),
                                ("uk", owner.clone()),
                                ("root", "0".into()),
                                ("dir", dir.into()),
                                ("page", page.to_string()),
                                ("num", "100".into()),
                                ("web", "1".into()),
                                ("clienttype", "0".into()),
                            ],
                        )?;
                        let v = self.call(u, Method::GET, None, &mut map).await?;
                        let rows = v["list"].as_array().context("百度分享目录缺失")?;
                        pending.extend(rows.clone());
                        if rows.len() < 100 {
                            break;
                        }
                        ensure!(page < 50, "分享目录过大，请拆分分享");
                    }
                } else {
                    bytes = bytes
                        .checked_add(file["size"].as_u64().context("百度分享大小缺失")?)
                        .context("分享大小超出范围")?;
                    count += 1;
                }
            }
            return Ok(json!({"required_bytes":bytes,"file_count":count}));
        }
        let destination = format!("/FlowDisk分享/分享-{id}");
        for dir in ["/FlowDisk分享".to_owned(), destination.clone()] {
            match self.item(&a, &dir).await {
                Ok(v) => ensure!(v["isdir"] == 1, "分享接收位置已被同名文件占用"),
                Err(e) if e.to_string().contains("文件不存在") => self.mkdir(&a, &dir).await?,
                Err(e) => return Err(e),
            }
        }
        let existing = self.list_page(&a, &destination, 1, 1000).await?;
        let missing: Vec<_> = files
            .iter()
            .filter(|file| {
                !existing
                    .iter()
                    .any(|saved| saved["server_filename"] == file["server_filename"])
            })
            .collect();
        for batch in missing.chunks(100) {
            let fids: Vec<_> = batch.iter().map(|f| f["fs_id"].clone()).collect();
            ensure!(
                fids.iter().all(|v| v.as_u64().is_some()),
                "百度分享文件标识不合法"
            );
            let u = Url::parse_with_params(
                self.base.join("share/transfer")?.as_str(),
                [
                    ("shareid", shareid.clone()),
                    ("from", owner.clone()),
                    ("bdstoken", a.token.clone()),
                    ("ondup", "fail".into()),
                    ("async", "0".into()),
                    ("web", "1".into()),
                    ("clienttype", "0".into()),
                ],
            )?;
            let v = self
                .call(
                    u,
                    Method::POST,
                    Some(&[
                        ("fsidlist".into(), json!(fids).to_string()),
                        ("path".into(), destination.clone()),
                    ]),
                    &mut map,
                )
                .await?;
            ensure!(
                v.get("taskid").is_none() || v["taskid"].as_u64() == Some(0),
                "百度正在异步转存，请稍后重试；不会覆盖已保存内容"
            );
        }
        self.cache.lock().await.clear();
        let saved = self.list_page(&a, &destination, 1, 1000).await?;
        ensure!(
            files.iter().all(|file| saved
                .iter()
                .any(|s| s["server_filename"] == file["server_filename"])),
            "百度转存尚未全部完成，请稍后重试"
        );
        Ok(json!({"paths":[destination]}))
    }
}
fn official_download(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(|h| {
            h == "baidu.com"
                || h.ends_with(".baidu.com")
                || h == "baidupcs.com"
                || h.ends_with(".baidupcs.com")
        })
}
/// Extract only JSON objects from the known share-page assignments; never evaluate scripts.
fn share_data(text: &str) -> Result<Value> {
    ensure!(text.len() <= 4 * 1024 * 1024, "百度分享页面过大，拒绝解析");
    for marker in ["locals.mset(", "yunData =", "yunData=", "window.yunData="] {
        let mut offset = 0;
        while let Some(at) = text[offset..].find(marker) {
            let rest = &text[offset + at + marker.len()..];
            if let Some(start) = rest.find('{') {
                let mut stream =
                    serde_json::Deserializer::from_str(&rest[start..]).into_iter::<Value>();
                if let Some(Ok(value)) = stream.next()
                    && value.get("shareid").is_some()
                {
                    return Ok(value);
                }
            }
            offset += at + marker.len();
        }
    }
    // Current web pages may put the same JSON in an inline try block.
    for (at, _) in text.match_indices('{').take(4096) {
        let mut stream = serde_json::Deserializer::from_str(&text[at..]).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next()
            && value.get("shareid").is_some()
            && (value.get("share_uk").is_some() || value.get("uk").is_some())
        {
            return Ok(value);
        }
    }
    bail!("百度分享数据不可用，请检查提取码、分享有效期和官方安全验证")
}

pub async fn qr_start(
    client: &Client,
    map: &mut BTreeMap<String, String>,
) -> Result<(Value, String)> {
    let gid = Uuid::new_v4().to_string().to_uppercase();
    let tt = (now() * 1000).to_string();
    let u = Url::parse_with_params(
        "https://passport.baidu.com/v2/api/getqrcode",
        [
            ("lp", "pc"),
            ("qrloginfrom", "pc"),
            ("gid", &gid),
            ("callback", "flowdisk_qr"),
            ("apiver", "v3"),
            ("tt", &tt),
            ("tpl", "netdisk"),
            ("_", &tt),
        ],
    )?;
    let response = client
        .get(u)
        .header(USER_AGENT, BROWSER)
        .header(REFERER, "https://pan.baidu.com/")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("百度二维码暂时无法获取"))?;
    collect(&response, map);
    let v = parse_jsonp(
        &response
            .text()
            .await
            .map_err(|_| anyhow::anyhow!("百度二维码响应无效"))?,
    )?;
    let sign = v["sign"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("百度未返回二维码票据，请稍后重试")?;
    let image = v["imgurl"].as_str().context("百度未返回二维码图片")?;
    let image = if image.starts_with("https://") {
        image.to_owned()
    } else {
        format!("https://{image}")
    };
    let image = Url::parse(&image)?;
    ensure!(
        image.scheme() == "https"
            && image.host_str() == Some("passport.baidu.com")
            && image.username().is_empty()
            && image.password().is_none()
            && image.port().is_none(),
        "百度二维码图片地址不合法"
    );
    let response = client
        .get(image)
        .header(USER_AGENT, BROWSER)
        .header(COOKIE, cookies(map))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("百度二维码图片获取失败"))?;
    ensure!(response.status().is_success(), "百度二维码图片不可用");
    let data = response
        .bytes()
        .await
        .map_err(|_| anyhow::anyhow!("百度二维码图片响应无效"))?;
    ensure!(
        data.len() < 512 * 1024 && data.starts_with(b"\x89PNG\r\n\x1a\n"),
        "百度二维码不是有效的 PNG 图片"
    );
    Ok((
        json!({"gid":gid,"sign":sign}),
        format!("data:image/png;base64,{}", STANDARD.encode(data)),
    ))
}
pub async fn qr_poll(
    client: &Client,
    state: &Value,
    map: &mut BTreeMap<String, String>,
) -> Result<(String, Option<String>)> {
    let gid = state["gid"].as_str().context("百度会话无效")?;
    let sign = state["sign"].as_str().context("百度二维码无效")?;
    let tt = (now() * 1000).to_string();
    let u = Url::parse_with_params(
        "https://passport.baidu.com/channel/unicast",
        [
            ("channel_id", sign),
            ("gid", gid),
            ("tpl", "netdisk"),
            ("_sdkFrom", "1"),
            ("callback", "flowdisk_status"),
            ("apiver", "v3"),
            ("tt", &tt),
            ("_", &tt),
        ],
    )?;
    let response = match client
        .get(u)
        .timeout(Duration::from_secs(3))
        .header(USER_AGENT, BROWSER)
        .header(REFERER, "https://pan.baidu.com/")
        .header(COOKIE, cookies(map))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Ok(("waiting".into(), None)),
        Err(_) => bail!("百度扫码状态暂时不可用"),
    };
    collect(&response, map);
    let v = parse_jsonp(
        &response
            .text()
            .await
            .map_err(|_| anyhow::anyhow!("百度扫码状态读取失败"))?,
    )?;
    if v["errno"] == 1 {
        return Ok(("waiting".into(), None));
    }
    if v["errno"] != 0 {
        return Ok(("expired".into(), None));
    }
    let channel: Value = serde_json::from_str(v["channel_v"].as_str().context("百度扫码状态缺失")?)
        .context("百度扫码状态无效")?;
    if channel["status"] == 1 {
        return Ok(("scanned".into(), None));
    }
    if channel["status"] != 0 {
        return Ok(("expired".into(), None));
    }
    let temporary = channel["v"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("百度确认登录回执缺失")?;
    let u = Url::parse_with_params(
        "https://passport.baidu.com/v3/login/main/qrbdusslogin",
        [
            ("v", tt.clone()),
            ("bduss", temporary.into()),
            ("u", "https://pan.baidu.com/disk/home".into()),
            ("loginVersion", "v4".into()),
            ("qrcode", "1".into()),
            ("tpl", "netdisk".into()),
            ("apiver", "v3".into()),
            ("tt", tt),
            ("time", now().to_string()),
            ("alg", "v3".into()),
            ("callback", "flowdisk_complete".into()),
        ],
    )?;
    let response = client
        .get(u)
        .header(USER_AGENT, BROWSER)
        .header(REFERER, "https://pan.baidu.com/")
        .header(COOKIE, cookies(map))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("百度登录凭据交换失败"))?;
    collect(&response, map);
    ensure!(response.status().is_success(), "百度未接受扫码登录确认");
    let cookie = cookies(map);
    cookie_map(&cookie)?;
    Ok(("ready".into(), Some(cookie)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsing_and_download_domains() {
        assert!(cookie_map("BDUSS=abcdefgh; STOKEN=a==").is_ok());
        assert!(cookie_map("BDUSS=abcdefgh\r\nCookie:x").is_err());
        assert!(parse_jsonp("cb({\"errno\":0})").is_ok());
        assert!(parse_jsonp("cb(alert('x'))").is_err());
        assert_eq!(
            share_data("try{locals.mset({\"shareid\":123,\"share_uk\":456,\"x\":{\"a\":1}});}")
                .unwrap()["shareid"],
            123
        );
        for url in [
            "https://evil.test/file",
            "https://baidu.com.evil.test/file",
            "http://d.pcs.baidu.com/file",
            "https://evil@d.pcs.baidu.com/file",
        ] {
            assert!(!official_download(&Url::parse(url).unwrap()));
        }
        assert!(official_download(
            &Url::parse("https://d.pcs.baidu.com/file").unwrap()
        ));
        assert!(remote("/baidu/../secret").is_err());
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        extract::{Request, State},
        response::{IntoResponse, Response},
        routing::any,
    };
    use std::sync::Arc;
    struct Fixture {
        files: Vec<Value>,
        transfers: usize,
        expired: bool,
        asynchronous: bool,
        bad_link: bool,
    }
    type Shared = Arc<Mutex<Fixture>>;
    fn file(path: &str, dir: bool, id: u64) -> Value {
        json!({"path":path,"server_filename":path.rsplit('/').next().unwrap(),"fs_id":id,"isdir":u8::from(dir),"size":if dir{0}else{3},"md5":"unit-md5","server_mtime":123})
    }
    async fn fixture(State(state): State<Shared>, request: Request) -> Response {
        let url = Url::parse(&format!("http://unit{}", request.uri())).unwrap();
        let path = url.path().to_owned();
        let q: BTreeMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let cookie = request
            .headers()
            .get(COOKIE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = to_bytes(request.into_body(), 65536).await.unwrap();
        let form =
            Url::parse(&format!("http://unit/?{}", String::from_utf8_lossy(&bytes))).unwrap();
        let form: BTreeMap<_, _> = form
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut s = state.lock().await;
        if !cookie.contains("BDUSS=unit-private-cookie") || s.expired {
            return Json(json!({"errno":-6})).into_response();
        }
        let value = match path.as_str() {
            "/rest/2.0/membership/user/info" => {
                json!({"error_code":0,"user_info":{"is_vip":0,"is_svip":0}})
            }
            "/rest/2.0/pcs/file" if q["method"] == "locatedownload" => {
                json!({"urls":[{"url":if s.bad_link{"https://evil.test/steal"}else{"https://d.pcs.baidu.com/file?sign=unit"}}]})
            }
            "/rest/2.0/pcs/file" => {
                assert_eq!(q["method"], "upload");
                assert_eq!(q["ondup"], "fail");
                assert!(String::from_utf8_lossy(&bytes).contains("abc"));
                assert!(!s.files.iter().any(|v| v["path"] == q["path"]));
                s.files.push(file(&q["path"], false, 77));
                json!({"path":q["path"],"size":3})
            }
            "/api/gettemplatevariable" => {
                json!({"errno":0,"result":{"bdstoken":"unit-private-token"}})
            }
            "/api/list" => {
                let dir = q.get("dir").unwrap();
                let list: Vec<_> = s
                    .files
                    .iter()
                    .filter(|v| {
                        let p = v["path"].as_str().unwrap().rsplit_once('/').unwrap().0;
                        (if p.is_empty() { "/" } else { p }) == dir
                    })
                    .cloned()
                    .collect();
                let n = q["num"].parse::<usize>().unwrap();
                let page = q["page"].parse::<usize>().unwrap();
                json!({"errno":0,"list":list.into_iter().skip((page-1)*n).take(n).collect::<Vec<_>>()})
            }
            "/api/filemetas" => {
                json!({"errno":0,"info":[{"dlink":if s.bad_link{"https://evil.test/steal"}else{"https://d.pcs.baidu.com/file?sign=unit"}}]})
            }
            "/api/create" => {
                let path = &form["path"];
                s.files.push(file(path, true, 100));
                json!({"errno":0})
            }
            "/api/filemanager" => {
                let list: Value = serde_json::from_str(&form["filelist"]).unwrap();
                for item in list.as_array().unwrap() {
                    match q["opera"].as_str() {
                        "rename" | "move" => {
                            let old = item["path"].as_str().unwrap();
                            let parent = if q["opera"] == "move" {
                                item["dest"].as_str().unwrap()
                            } else {
                                old.rsplit_once('/').unwrap().0
                            };
                            let name = item["newname"].as_str().unwrap();
                            let new = format!("{}/{name}", parent.trim_end_matches('/'));
                            for v in &mut s.files {
                                let p = v["path"].as_str().unwrap().to_owned();
                                if p == old || p.starts_with(&format!("{old}/")) {
                                    let path = format!("{new}{}", &p[old.len()..]);
                                    v["path"] = path.clone().into();
                                    v["server_filename"] = path.rsplit('/').next().unwrap().into();
                                }
                            }
                        }
                        "delete" => {
                            let old = item.as_str().unwrap();
                            s.files.retain(|v| {
                                v["path"] != old
                                    && !v["path"].as_str().unwrap().starts_with(&format!("{old}/"))
                            });
                        }
                        _ => panic!("unexpected operation"),
                    }
                }
                json!({"errno":0,"info":[]})
            }
            "/share/verify" => {
                if form["pwd"] == "abcd" {
                    json!({"errno":0,"randsk":"unit-share-cookie"})
                } else {
                    json!({"errno":-9})
                }
            }
            "/s/1unitSHARE" => {
                return Response::new(Body::from(
                    "<script>locals.mset({\"shareid\":12,\"share_uk\":34,\"loginstate\":1});</script>",
                ));
            }
            "/share/list" => {
                assert!(cookie.contains("BDCLND=unit-share-cookie"));
                json!({"errno":0,"list":[file("/shared.bin",false,9)]})
            }
            "/share/transfer" => {
                assert_eq!(q["ondup"], "fail");
                assert_eq!(form["fsidlist"], "[9]");
                s.transfers += 1;
                if s.asynchronous {
                    json!({"errno":0,"taskid":99})
                } else {
                    s.files
                        .push(file(&format!("{}/shared.bin", form["path"]), false, 9));
                    json!({"errno":0})
                }
            }
            _ => panic!("unexpected native Baidu endpoint {path}"),
        };
        Json(value).into_response()
    }
    #[tokio::test]
    async fn native_adapter_protocol_and_persistence() {
        let state = Arc::new(Mutex::new(Fixture {
            files: vec![file("/file.bin", false, 1)],
            transfers: 0,
            expired: false,
            asynchronous: false,
            bad_link: false,
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(fixture))
                    .with_state(state.clone()),
            )
            .into_future(),
        );
        let dir = std::env::temp_dir().join(format!("flowdisk-baidu-{}", Uuid::new_v4()));
        fs::create_dir(&dir).await.unwrap();
        let mut native = Baidu::new(&dir).unwrap();
        native.base = Url::parse(&format!("http://{address}/")).unwrap();
        native
            .connect("BDUSS=unit-private-cookie; STOKEN=unit-stoken")
            .await
            .unwrap();
        assert!(native.connected().await);
        native
            .upload("/baidu/upload.bin", axum::body::Bytes::from_static(b"abc"))
            .await
            .unwrap();
        assert!(
            native
                .upload("/baidu/upload.bin", axum::body::Bytes::from_static(b"abc"))
                .await
                .is_err()
        );
        state
            .lock()
            .await
            .files
            .retain(|v| v["path"] != "/upload.bin");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("baidu.json"))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let listed = native
            .fs(
                "/api/fs/list",
                &json!({"path":"/baidu","page":1,"per_page":100}),
            )
            .await
            .unwrap();
        assert_eq!(listed["content"][0]["name"], "file.bin");
        let meta = native
            .fs("/api/fs/get", &json!({"path":"/baidu/file.bin"}))
            .await
            .unwrap();
        assert_eq!(meta["modified"], "123");
        let (meta, link) = native.resolve("/baidu/file.bin").await.unwrap();
        assert_eq!(meta.size, 3);
        assert_eq!(link.concurrency, 1);
        assert_eq!(link.max_range, 4 * 1024 * 1024);
        assert!(
            link.headers[COOKIE]
                .to_str()
                .unwrap()
                .contains("unit-private-cookie")
        );
        state.lock().await.bad_link = true;
        assert!(native.resolve("/baidu/file.bin").await.is_err());
        state.lock().await.bad_link = false;
        assert!(native.connect("BDUSS=invalid-cookie").await.is_err());
        assert!(
            native
                .fs("/api/fs/list", &json!({"path":"/baidu","per_page":100}))
                .await
                .is_ok()
        );
        native
            .fs(
                "/api/fs/rename",
                &json!({"path":"/baidu/file.bin","name":"renamed.bin"}),
            )
            .await
            .unwrap();
        native
            .fs("/api/fs/mkdir", &json!({"path":"/baidu/folder"}))
            .await
            .unwrap();
        native
            .fs(
                "/api/fs/move",
                &json!({"src_dir":"/baidu","dst_dir":"/baidu/folder","names":["renamed.bin"]}),
            )
            .await
            .unwrap();
        assert!(
            native
                .fs("/api/fs/get", &json!({"path":"/baidu/folder/renamed.bin"}))
                .await
                .is_ok()
        );
        assert!(
            native
                .fs(
                    "/api/fs/rename",
                    &json!({"path":"/baidu/folder/renamed.bin","name":"renamed.bin"})
                )
                .await
                .is_err()
        );
        native
            .fs(
                "/api/fs/remove",
                &json!({"dir":"/baidu/folder","names":["renamed.bin"]}),
            )
            .await
            .unwrap();
        let share = Url::parse("https://pan.baidu.com/s/1unitSHARE").unwrap();
        let id = Uuid::new_v4();
        assert!(
            native
                .import_share(id, &share, "bad")
                .await
                .unwrap_err()
                .to_string()
                .contains("提取码错误")
        );
        let paths = native.import_share(id, &share, "abcd").await.unwrap();
        assert_eq!(paths, vec![format!("/FlowDisk分享/分享-{id}")]);
        native.import_share(id, &share, "abcd").await.unwrap();
        assert_eq!(state.lock().await.transfers, 1);
        state.lock().await.asynchronous = true;
        assert!(
            native
                .import_share(Uuid::new_v4(), &share, "abcd")
                .await
                .unwrap_err()
                .to_string()
                .contains("异步")
        );
        let restored = Baidu::new(&dir).unwrap();
        assert!(restored.connected().await);
        restored.disconnect().await.unwrap();
        assert!(!dir.join("baidu.json").exists());
        fs::remove_dir_all(dir).await.unwrap();
        server.abort();
    }
}
