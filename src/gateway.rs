use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, time::Duration};
use tokio::sync::Mutex;

// Keep link acquisition and download UA identical (notably for 115).
pub const USER_AGENT: &str = "pan.baidu.com";
pub struct Gateway {
    base: Url,
    pub baidu: crate::baidu::Baidu,
    pub external: crate::external::External,
    username: String,
    password: String,
    token: Mutex<Option<String>>,
    client: Client,
}
#[derive(Deserialize)]
pub struct Metadata {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    #[serde(default)]
    pub modified: String,
    #[serde(default)]
    pub hash_info: Value,
    #[serde(default)]
    pub raw_url: String,
    #[serde(default)]
    pub sign: String,
    #[serde(default)]
    pub provider: String,
}
#[derive(Clone)]
pub struct Link {
    pub url: Url,
    pub headers: HeaderMap,
    pub concurrency: usize,
    pub max_range: u64,
}
impl Gateway {
    pub fn new(data: &std::path::Path) -> Result<Self> {
        let base = Url::parse(
            &env::var("OPENLIST_URL").unwrap_or_else(|_| "http://openlist:5244".into()),
        )?;
        ensure!(
            matches!(base.scheme(), "http" | "https")
                && base.host_str().is_some()
                && base.username().is_empty()
                && base.password().is_none(),
            "OPENLIST_URL 必须是无用户名密码的 HTTP(S) 地址"
        );
        Ok(Self {
            baidu: crate::baidu::Baidu::new(data)?,
            external: crate::external::External::new(data)?,
            base,
            username: env::var("OPENLIST_USERNAME").unwrap_or_else(|_| "admin".into()),
            password: env::var("OPENLIST_PASSWORD").context("必须设置 OPENLIST_PASSWORD")?,
            token: Mutex::new(None),
            client: Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }
    #[cfg(test)]
    pub(crate) fn fixture(data: &std::path::Path, base: Url) -> Self {
        Self {
            base,
            baidu: crate::baidu::Baidu::new(data).unwrap(),
            external: crate::external::External::new(data).unwrap(),
            username: "admin".into(),
            password: "fixture".into(),
            token: Mutex::new(None),
            client: Client::new(),
        }
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let source = body["path"]
            .as_str()
            .or_else(|| body["src_dir"].as_str())
            .or_else(|| body["dir"].as_str())
            .unwrap_or("");
        if path.starts_with("/api/fs/") && crate::external::owns(source) {
            return self.external.fs(self, path, &body).await;
        }
        if path.starts_with("/api/fs/")
            && crate::baidu::owns(source)
            && self.baidu.connected().await
        {
            return self.baidu.fs(path, &body).await;
        }
        let root =
            path == "/api/fs/list" && source == "/" && body["page"].as_u64().unwrap_or(1) == 1;
        let result = self.request(reqwest::Method::POST, path, Some(body)).await;
        if root && self.baidu.connected().await {
            let mut result = result.unwrap_or_else(|_| json!({"content":[],"total":0}));
            if let Some(items) = result["content"].as_array_mut()
                && !items.iter().any(|v| v["name"] == "baidu")
            {
                items.push(json!({"name":"baidu","is_dir":true,"size":0}));
                result["total"] = json!(result["total"].as_u64().unwrap_or(0) + 1);
            }
            return Ok(result);
        }
        result
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, path, None).await
    }
    pub(crate) async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let token = {
                let mut cached = self.token.lock().await;
                if cached.is_none() {
                    let response = self
                        .client
                        .post(self.base.join("/api/auth/login")?)
                        .json(&json!({"username":self.username,"password":self.password}))
                        .send()
                        .await
                        .map_err(|e| e.without_url())
                        .context("无法连接 OpenList")?
                        .json::<Value>()
                        .await
                        .map_err(|e| e.without_url())
                        .context("OpenList 登录响应不合法")?;
                    ensure!(
                        response["code"].as_u64() == Some(200),
                        "OpenList 登录失败，请检查服务端账号密码配置"
                    );
                    *cached = Some(
                        response["data"]["token"]
                            .as_str()
                            .context("OpenList 未返回令牌")?
                            .into(),
                    );
                }
                cached.clone().unwrap()
            };
            let mut request = self
                .client
                .request(method.clone(), self.base.join(path)?)
                .header("Authorization", &token);
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .map_err(|e| e.without_url())
                .context("OpenList 请求失败")?;
            let http_status = response.status();
            let result: Value = response
                .json()
                .await
                .map_err(|e| e.without_url())
                .context("OpenList 响应不合法")?;
            let code = result["code"].as_u64().unwrap_or(500);
            if (code == 401 || http_status == 401) && attempt == 0 {
                let mut cached = self.token.lock().await;
                if cached.as_ref() == Some(&token) {
                    *cached = None;
                }
                continue;
            }
            // Do not expose upstream URLs, cookies or provider tokens via error messages.
            ensure!(
                code == 200 && http_status.is_success(),
                "网盘网关请求失败（接口 {path}，状态 {code}）；请检查 OpenList 中的挂载状态和授权"
            );
            return Ok(result["data"].clone());
        }
        bail!("OpenList 登录已失效")
    }
    pub async fn upload(&self, path: &str, bytes: axum::body::Bytes) -> Result<Value> {
        if crate::baidu::owns(path) && self.baidu.connected().await {
            return self.baidu.upload(path, bytes).await;
        }
        ensure!(
            !crate::external::owns(path),
            "分享内容只读，请在自己的网盘目录上传文件"
        );
        let parent = path.rsplit_once('/').context("上传路径无效")?.0;
        ensure!(self.metadata(parent).await?.is_dir, "上传目标必须是目录");
        let token = self.token.lock().await.clone().context("网关未登录")?;
        let encoded: String = path
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let response = self
            .client
            .put(self.base.join("/api/fs/put")?)
            .header("Authorization", token)
            .header("File-Path", encoded)
            .header("Overwrite", "false")
            .header("As-Task", "false")
            .header("Content-Type", "application/octet-stream")
            .timeout(Duration::from_secs(600))
            .body(bytes)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        ensure!(response.status().is_success(), "网盘上传请求失败");
        let result: Value = response.json().await.map_err(|e| e.without_url())?;
        ensure!(
            result["code"].as_u64() == Some(200),
            "网盘上传失败，请检查权限、空间或是否存在同名文件"
        );
        Ok(json!({"ok":true}))
    }
    pub async fn metadata(&self, path: &str) -> Result<Metadata> {
        serde_json::from_value(
            self.post("/api/fs/get", json!({"path":path,"password":""}))
                .await?,
        )
        .context("文件元数据不合法")
    }
    pub async fn resolve(&self, path: &str) -> Result<(Metadata, Link)> {
        if crate::external::owns(path) {
            return self.external.resolve(self, path).await;
        }
        if crate::baidu::owns(path) && self.baidu.connected().await {
            return self.baidu.resolve(path).await;
        }
        let meta = self.metadata(path).await?;
        ensure!(!meta.is_dir, "目录不能作为文件下载");
        let raw = self.base.join(&meta.raw_url)?;
        // Quark/MustProxy and explicitly proxied mounts must retain the gateway path.
        let thunder = matches!(
            meta.provider.as_str(),
            "Thunder" | "ThunderExpert" | "ThunderBrowser" | "ThunderBrowserExpert"
        );
        // Thunder CDN links work with the driver's download UA. The gateway's
        // proxy can fail with EOF for these same files; use the signed direct link.
        if !thunder && !meta.raw_url.is_empty() && raw.path().starts_with("/p/") {
            return Ok((
                meta,
                Link {
                    url: raw,
                    headers: HeaderMap::new(),
                    concurrency: 0,
                    max_range: 0,
                },
            ));
        }
        let data = self.post("/api/fs/link", json!({"path":path})).await?;
        let link_url = data["url"].as_str().unwrap_or("");
        if link_url.is_empty() {
            let mut proxy = self.base.clone();
            proxy.set_path(&format!("/p{path}"));
            proxy.query_pairs_mut().append_pair("sign", &meta.sign);
            return Ok((
                meta,
                Link {
                    url: proxy,
                    headers: HeaderMap::new(),
                    concurrency: 0,
                    max_range: 0,
                },
            ));
        }
        let url = self.base.join(link_url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "下载链接必须为 HTTP(S)"
        );
        let mut headers = HeaderMap::new();
        if let Some(map) = data["header"].as_object() {
            for (key, values) in map {
                let key = HeaderName::from_bytes(key.as_bytes())?;
                // Transfer framing and range are controlled by our downloader.
                if [
                    "host",
                    "content-length",
                    "range",
                    "if-range",
                    "accept-encoding",
                    "connection",
                    "transfer-encoding",
                ]
                .contains(&key.as_str())
                {
                    continue;
                }
                if let Some(values) = values.as_array() {
                    for value in values {
                        headers.append(
                            key.clone(),
                            HeaderValue::from_str(value.as_str().context("网盘请求头格式不合法")?)?,
                        );
                    }
                } else if let Some(value) = values.as_str() {
                    headers.insert(key, HeaderValue::from_str(value)?);
                }
            }
        }
        let concurrency = data["concurrency"].as_u64().unwrap_or(0).min(32) as usize;
        Ok((
            meta,
            Link {
                url,
                headers,
                concurrency,
                max_range: data["max_range"].as_u64().unwrap_or(0),
            },
        ))
    }
}
