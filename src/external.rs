//! Microsoft/Google OAuth configuration and read-only shared-item views.
//! Account CRUD stays in OpenList; shared files are read without copying them.
use crate::gateway::{Gateway, Link, Metadata};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, sync::Mutex};
use uuid::Uuid;

pub fn oauth_endpoints(provider: &str) -> (&'static str, &'static str) {
    if provider == "onedrive" {
        (
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            "offline_access Files.ReadWrite.All",
        )
    } else {
        (
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://www.googleapis.com/auth/drive",
        )
    }
}
pub fn token_endpoint(provider: &str) -> &'static str {
    if provider == "onedrive" {
        "https://login.microsoftonline.com/common/oauth2/v2.0/token"
    } else {
        "https://oauth2.googleapis.com/token"
    }
}
pub fn oauth_config(provider: &str) -> Result<(String, String, String)> {
    ensure!(["onedrive", "google"].contains(&provider), "不支持的网盘");
    let prefix = if provider == "onedrive" {
        "ONEDRIVE"
    } else {
        "GOOGLE"
    };
    OAuthConfig {
        client_id: std::env::var(format!("{prefix}_CLIENT_ID"))?,
        client_secret: std::env::var(format!("{prefix}_CLIENT_SECRET"))?,
        public_url: std::env::var("PUBLIC_URL")?,
    }
    .resolve(provider)
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub public_url: String,
}
impl OAuthConfig {
    pub fn validate(&self, provider: &str) -> Result<()> {
        ensure!(
            ["google", "onedrive"].contains(&provider)
                && !self.client_id.trim().is_empty()
                && self.client_id.len() <= 2000
                && !self.client_id.contains(['\r', '\n']),
            "请填写有效的应用客户端 ID"
        );
        if !self.public_url.is_empty() {
            self.resolve(provider)?;
        } else if provider == "google" {
            ensure!(
                !self.client_secret.is_empty()
                    && self.client_secret.len() <= 2000
                    && !self.client_secret.contains(['\r', '\n']),
                "请填写 Google 桌面应用客户端密钥"
            );
        }
        Ok(())
    }
    pub fn resolve(&self, provider: &str) -> Result<(String, String, String)> {
        ensure!(["onedrive", "google"].contains(&provider), "不支持的网盘");
        let base = Url::parse(&self.public_url)?;
        ensure!(
            !self.client_id.trim().is_empty()
                && !self.client_secret.trim().is_empty()
                && self.client_id.len() <= 2000
                && self.client_secret.len() <= 2000
                && !self.client_id.contains(['\r', '\n'])
                && !self.client_secret.contains(['\r', '\n'])
                && self.public_url.len() <= 2000
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && (base.scheme() == "https"
                    || (base.scheme() == "http"
                        && matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))),
            "官方授权需要 HTTPS PUBLIC_URL（本机 localhost 除外）"
        );
        if provider == "google" {
            ensure!(
                base.host_str().is_some_and(|host| host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .map_or(true, |ip| ip.is_loopback())),
                "Google 网页授权需要域名，不能使用局域网 IP 作为回调地址"
            );
        }
        Ok((
            self.client_id.clone(),
            self.client_secret.clone(),
            base.join(&format!("/oauth/{provider}/callback"))?
                .to_string(),
        ))
    }
}
pub fn credential(provider: &str, token: &str, body: &Value) -> Result<Value> {
    ensure!(["onedrive", "google"].contains(&provider), "不支持的网盘");
    ensure!(
        token.len() >= 8
            && token.len() <= 12000
            && !token.chars().any(char::is_whitespace)
            && !token.contains('\\')
            && !token.starts_with(['.', '/']),
        "刷新令牌格式无效"
    );
    let custom = match body["source"].as_str().unwrap_or("openlist") {
        "openlist" => false,
        "custom" => true,
        _ => bail!("授权来源无效"),
    };
    // With online refresh disabled, the Google driver treats existing paths as
    // service-account files. Only Google's token prefix may contain slashes.
    if provider == "google" && custom && token.contains('/') {
        ensure!(
            token.starts_with("1//") && !token[3..].contains('/') && !token.contains(".."),
            "请填写 Google 刷新令牌，不能填写文件路径"
        );
    }
    let mut addition = if provider == "onedrive" {
        json!({"root_folder_path":"/", "region":"global", "refresh_token":token,
            "use_online_api":!custom, "api_url_address":"https://api.oplist.org/onedrive/renewapi", "redirect_uri":"https://api.oplist.org/onedrive/callback"})
    } else {
        json!({"root_folder_id":"root", "refresh_token":token, "use_online_api":!custom,
            "api_url_address":"https://api.oplist.org/googleui/renewapi"})
    };
    if custom {
        for key in ["client_id", "client_secret"] {
            let v = body[key].as_str().context("缺少客户端信息")?;
            ensure!(
                !v.is_empty() && v.len() <= 2000 && !v.contains(['\r', '\n']),
                "客户端信息无效"
            );
            addition[key] = v.into();
        }
        if provider == "onedrive" {
            let uri = Url::parse(body["redirect_uri"].as_str().context("缺少原授权回调")?)?;
            ensure!(
                matches!(uri.scheme(), "http" | "https")
                    && uri.username().is_empty()
                    && uri.password().is_none(),
                "回调地址无效"
            );
            addition["redirect_uri"] = uri.to_string().into();
        }
    }
    Ok(
        json!({"driver":if provider=="onedrive"{"Onedrive"}else{"GoogleDrive"}, "addition":addition}),
    )
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-!.".contains(&c))
}
pub fn google_id(url: &Url) -> Result<String> {
    let p: Vec<_> = url
        .path_segments()
        .context("无效链接")?
        .filter(|s| !s.is_empty())
        .collect();
    let id = if p.first() == Some(&"file") && p.get(1) == Some(&"d") {
        p.get(2).map(|v| v.to_string())
    } else if let Some(i) = p.iter().position(|s| *s == "folders") {
        p.get(i + 1).map(|v| v.to_string())
    } else if matches!(url.path(), "/open" | "/uc") {
        url.query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
    } else {
        None
    }
    .context("请输入 Google Drive 文件或文件夹分享链接")?;
    ensure!(valid_id(&id), "文件标识无效");
    Ok(id)
}
pub fn share_provider(url: &Url) -> Result<Option<&'static str>> {
    match url.host_str().unwrap_or("") {
        "drive.google.com" => {
            google_id(url)?;
            Ok(Some("google"))
        }
        "1drv.ms" | "onedrive.live.com" => {
            ensure!(
                url.path() != "/"
                    || (url.host_str() == Some("onedrive.live.com")
                        && url
                            .query_pairs()
                            .any(|(k, v)| ["id", "resid"].contains(&k.as_ref()) && !v.is_empty())),
                "请输入 OneDrive 分享链接"
            );
            Ok(Some("onedrive"))
        }
        host if host.ends_with(".sharepoint.com") && host != ".sharepoint.com" => {
            ensure!(
                url.path().starts_with("/:"),
                "请输入 SharePoint 的共享文件或文件夹链接"
            );
            Ok(Some("onedrive"))
        }
        _ => Ok(None),
    }
}
pub fn owns(path: &str) -> bool {
    path.starts_with("/google分享/") || path.starts_with("/onedrive分享/")
}
#[derive(Clone, Serialize, Deserialize)]
struct Node {
    provider: String,
    storage: i64,
    id: String,
    drive: String,
    resource_key: String,
    name: String,
    size: u64,
    is_dir: bool,
    mime: String,
    modified: String,
}
impl Node {
    fn public(&self) -> Value {
        json!({"name":self.name,"size":self.size,"is_dir":self.is_dir,"modified":self.modified,
            "provider":if self.provider=="google"{"GoogleDrive"}else{"Onedrive"}})
    }
}
struct Token {
    value: String,
    expires: u64,
    refresh: String,
}
pub struct External {
    client: Client,
    path: PathBuf,
    #[cfg(test)]
    test_base: Option<Url>,
    nodes: Mutex<BTreeMap<String, Node>>,
    tokens: Mutex<BTreeMap<i64, Token>>,
}
impl External {
    pub fn new(data: &Path) -> Result<Self> {
        let path = data.join("external-shares.json");
        let nodes = match std::fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw).context("分享索引损坏")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            #[cfg(test)]
            test_base: None,
            path,
            nodes: Mutex::new(nodes),
            tokens: Mutex::new(BTreeMap::new()),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(40))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
    fn request_url(&self, url: Url) -> Url {
        #[cfg(test)]
        if let Some(base) = &self.test_base {
            let mut target = base.clone();
            target.set_path(url.path());
            target.set_query(url.query());
            return target;
        }
        url
    }
    async fn save(&self, nodes: &BTreeMap<String, Node>) -> Result<()> {
        let temp = self.path.with_extension("tmp");
        fs::write(&temp, serde_json::to_vec(nodes)?).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        fs::rename(temp, &self.path).await?;
        Ok(())
    }
    async fn node(&self, path: &str) -> Result<Node> {
        self.nodes
            .lock()
            .await
            .get(path)
            .cloned()
            .context("分享文件索引不存在，请重新解析链接")
    }
    async fn storage(&self, gateway: &Gateway, id: i64) -> Result<Value> {
        let all = gateway
            .get("/api/admin/storage/list?page=1&per_page=0")
            .await?;
        all["content"]
            .as_array()
            .context("账号列表不可用")?
            .iter()
            .find(|s| {
                s["id"].as_i64() == Some(id) && s["disabled"] != true && s["status"] == "work"
            })
            .cloned()
            .context("请先重新登录这个网盘")
    }
    async fn token(&self, gateway: &Gateway, storage: &Value, provider: &str) -> Result<String> {
        let id = storage["id"].as_i64().context("账号标识无效")?;
        let addition: Value =
            serde_json::from_str(storage["addition"].as_str().context("缺少账号凭据")?)?;
        let refresh = addition["refresh_token"].as_str().context("缺少刷新令牌")?;
        let mut tokens = self.tokens.lock().await;
        if let Some(t) = tokens.get(&id)
            && t.expires > crate::now() + 60
            && t.refresh == refresh
        {
            return Ok(t.value.clone());
        }
        // OpenList serializes its typed addition and drops unknown fields on refresh.
        let public_client = provider == "onedrive"
            && (addition["flowdisk_public_client"] == true
                || addition["api_url_address"]
                    .as_str()
                    .and_then(|v| Url::parse(v).ok())
                    .is_some_and(|u| u.path() == "/internal/onedrive/renew"));
        let response = if addition["use_online_api"] == true && !public_client {
            let endpoint = if provider == "google" {
                "https://api.oplist.org/googleui/renewapi"
            } else {
                "https://api.oplist.org/onedrive/renewapi"
            };
            self.client
                .get(self.request_url(Url::parse(endpoint)?))
                .query(&[
                    ("refresh_ui", refresh),
                    ("server_use", "true"),
                    (
                        "driver_txt",
                        if provider == "google" {
                            "googleui_go"
                        } else {
                            "onedrive_pr"
                        },
                    ),
                ])
                .send()
                .await
        } else {
            let mut form = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", addition["client_id"].as_str().unwrap_or("")),
            ];
            for key in ["client_secret", "redirect_uri"] {
                if let Some(value) = addition[key].as_str().filter(|v| !v.is_empty()) {
                    form.push((key, value));
                }
            }
            self.client
                .post(self.request_url(Url::parse(token_endpoint(provider))?))
                .form(&form)
                .send()
                .await
        }
        .map_err(|_| anyhow::anyhow!("授权刷新失败，请检查服务器能否访问官方服务"))?;
        let success = response.status().is_success();
        let v: Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("授权响应无效"))?;
        let token = v["access_token"]
            .as_str()
            .filter(|s| success && !s.is_empty())
            .context("授权失效，请重新登录网盘")?
            .to_owned();
        let refreshed = v["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(refresh)
            .to_owned();
        if refreshed != refresh {
            // Merge against the current storage: retain all settings and avoid
            // overwriting a token rotated by the gateway concurrently.
            let mut current = self.storage(gateway, id).await?;
            let mut a: Value = serde_json::from_str(current["addition"].as_str().unwrap_or("{}"))?;
            if a["refresh_token"] == refresh {
                a["refresh_token"] = refreshed.clone().into();
                current["addition"] = serde_json::to_string(&a)?.into();
                gateway
                    .request(Method::POST, "/api/admin/storage/update", Some(current))
                    .await?;
            }
        }
        tokens.insert(
            id,
            Token {
                value: token.clone(),
                refresh: refreshed,
                expires: crate::now() + v["expires_in"].as_u64().unwrap_or(3600).min(3600),
            },
        );
        Ok(token)
    }
    async fn read(&self, url: Url, token: &str, node: Option<&Node>) -> Result<Value> {
        let graph_share = url.host_str() == Some("graph.microsoft.com")
            && url.path().starts_with("/v1.0/shares/");
        let mut request = self.client.get(self.request_url(url)).bearer_auth(token);
        if graph_share {
            request = request.header("Prefer", "redeemSharingLinkIfNecessary");
        }
        if let Some(n) = node
            && n.provider == "google"
            && !n.resource_key.is_empty()
        {
            request = request.header(
                "X-Goog-Drive-Resource-Keys",
                format!("{}/{}", n.id, n.resource_key),
            );
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("无法访问分享，请检查服务器网络"))?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "读取分享失败（HTTP {}），请检查链接权限、下载限制或重新登录",
            status.as_u16()
        );
        response
            .json()
            .await
            .map_err(|e| e.without_url())
            .context("分享响应无效")
    }
    fn from_value(provider: &str, storage: i64, v: &Value, drive: &str) -> Result<Node> {
        let id = v["id"].as_str().context("分享缺少文件标识")?;
        ensure!(valid_id(id), "文件标识无效");
        let size = v["size"]
            .as_u64()
            .or_else(|| v["size"].as_str().and_then(|v| v.parse().ok()))
            .unwrap_or(0);
        let mime = v["mimeType"].as_str().unwrap_or("");
        Ok(Node {
            provider: provider.into(),
            storage,
            id: id.into(),
            drive: v["parentReference"]["driveId"]
                .as_str()
                .unwrap_or(drive)
                .into(),
            resource_key: v["resourceKey"].as_str().unwrap_or("").into(),
            name: crate::safe_name(v["name"].as_str().context("分享缺少文件名")?),
            size,
            is_dir: if provider == "google" {
                mime == "application/vnd.google-apps.folder"
            } else {
                v["folder"].is_object()
            },
            mime: mime.into(),
            modified: v[if provider == "google" {
                "modifiedTime"
            } else {
                "lastModifiedDateTime"
            }]
            .as_str()
            .unwrap_or("")
            .into(),
        })
    }
    pub async fn prepare(
        &self,
        gateway: &Gateway,
        storage: &Value,
        share: Uuid,
        raw: &str,
    ) -> Result<Vec<String>> {
        let url = Url::parse(raw)?;
        let provider = share_provider(&url)?.context("不支持的分享链接")?;
        let token = self.token(gateway, storage, provider).await?;
        let id = storage["id"].as_i64().context("账号标识无效")?;
        let v = if provider == "google" {
            let file = google_id(&url)?;
            let key = url
                .query_pairs()
                .find(|(k, _)| k == "resourcekey")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let node = Node {
                provider: provider.into(),
                storage: id,
                id: file.clone(),
                resource_key: key,
                drive: String::new(),
                name: String::new(),
                size: 0,
                is_dir: false,
                mime: String::new(),
                modified: String::new(),
            };
            let api = Url::parse_with_params(
                &format!("https://www.googleapis.com/drive/v3/files/{file}"),
                [
                    ("supportsAllDrives", "true"),
                    ("fields", "id,name,mimeType,size,modifiedTime,resourceKey"),
                ],
            )?;
            let mut v = self.read(api, &token, Some(&node)).await?;
            if v["resourceKey"].as_str().unwrap_or("").is_empty() {
                v["resourceKey"] = node.resource_key.into();
            }
            v
        } else {
            let encoded = URL_SAFE_NO_PAD.encode(raw);
            let v = self
                .read(
                    Url::parse(&format!(
                        "https://graph.microsoft.com/v1.0/shares/u!{encoded}/driveItem"
                    ))?,
                    &token,
                    None,
                )
                .await?;
            if v["remoteItem"].is_object() {
                v["remoteItem"].clone()
            } else {
                v
            }
        };
        let node = Self::from_value(provider, id, &v, "")?;
        if provider == "onedrive" {
            ensure!(valid_id(&node.drive), "分享未返回有效网盘标识");
        }
        let root = format!("/{provider}分享/{share}/{}", node.name);
        let mut nodes = self.nodes.lock().await;
        nodes.insert(root.clone(), node);
        self.save(&nodes).await?;
        Ok(vec![root])
    }
    pub async fn fs(&self, gateway: &Gateway, route: &str, body: &Value) -> Result<Value> {
        let path = body["path"].as_str().context("缺少分享路径")?;
        let node = self.node(path).await?;
        if route == "/api/fs/get" {
            return Ok(node.public());
        }
        ensure!(
            route == "/api/fs/list",
            "分享内容只读，请在自己的网盘目录管理文件"
        );
        ensure!(node.is_dir, "文件不是目录");
        let storage = self.storage(gateway, node.storage).await?;
        let token = self.token(gateway, &storage, &node.provider).await?;
        let mut next = if node.provider == "google" {
            Url::parse_with_params(
                "https://www.googleapis.com/drive/v3/files",
                [
                    ("q", format!("'{}' in parents and trashed = false", node.id)),
                    ("pageSize", "1000".into()),
                    ("supportsAllDrives", "true".into()),
                    ("includeItemsFromAllDrives", "true".into()),
                    (
                        "fields",
                        "nextPageToken,files(id,name,mimeType,size,modifiedTime,resourceKey)"
                            .into(),
                    ),
                ],
            )?
        } else {
            Url::parse(&format!(
                "https://graph.microsoft.com/v1.0/drives/{}/items/{}/children?$top=200",
                node.drive, node.id
            ))?
        };
        let initial = next.clone();
        let mut children = Vec::new();
        for page in 0..50 {
            let v = self.read(next.clone(), &token, Some(&node)).await?;
            for file in v[if node.provider == "google" {
                "files"
            } else {
                "value"
            }]
            .as_array()
            .context("分享目录响应无效")?
            {
                children.push(Self::from_value(
                    &node.provider,
                    node.storage,
                    file,
                    &node.drive,
                )?);
            }
            ensure!(children.len() <= 5000, "分享超过 5000 项，请使用子目录链接");
            if node.provider == "google" {
                let Some(cursor) = v["nextPageToken"].as_str().filter(|v| !v.is_empty()) else {
                    break;
                };
                next = initial.clone();
                next.query_pairs_mut().append_pair("pageToken", cursor);
            } else {
                let Some(link) = v["@odata.nextLink"].as_str() else {
                    break;
                };
                next = Url::parse(link)?;
                ensure!(
                    next.scheme() == "https"
                        && next.host_str() == Some("graph.microsoft.com")
                        && next.port().is_none()
                        && next.username().is_empty()
                        && next.password().is_none(),
                    "分页地址无效"
                );
            }
            ensure!(page < 49, "分享目录过大，请使用子目录链接");
        }
        // Drive permits duplicate names. Give only collisions stable virtual
        // display names; source names remain untouched.
        let mut counts = BTreeMap::new();
        for n in &children {
            *counts.entry(n.name.clone()).or_insert(0) += 1;
        }
        for n in &mut children {
            if counts[&n.name] > 1 {
                n.name = format!("{} ({})", n.name, n.id);
            }
        }
        let content: Vec<_> = children.iter().map(Node::public).collect();
        {
            let mut nodes = self.nodes.lock().await;
            for n in children {
                nodes.insert(format!("{path}/{}", n.name), n);
            }
            self.save(&nodes).await?;
        }
        let page = body["page"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = body["per_page"].as_u64().unwrap_or(0) as usize;
        let total = content.len();
        let content = if limit == 0 {
            content
        } else {
            content
                .into_iter()
                .skip(page.saturating_sub(1).saturating_mul(limit))
                .take(limit)
                .collect()
        };
        Ok(json!({"content":content,"total":total}))
    }
    pub async fn resolve(&self, gateway: &Gateway, path: &str) -> Result<(Metadata, Link)> {
        let node = self.node(path).await?;
        ensure!(!node.is_dir, "目录不能作为文件下载");
        ensure!(
            !node.mime.starts_with("application/vnd.google-apps."),
            "Google 在线文档或快捷方式请先在官方网盘导出为普通文件，再下载"
        );
        let storage = self.storage(gateway, node.storage).await?;
        let token = self.token(gateway, &storage, &node.provider).await?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let url = if node.provider == "google" {
            if !node.resource_key.is_empty() {
                headers.insert(
                    "X-Goog-Drive-Resource-Keys",
                    HeaderValue::from_str(&format!("{}/{}", node.id, node.resource_key))?,
                );
            }
            Url::parse(&format!(
                "https://www.googleapis.com/drive/v3/files/{}?alt=media&supportsAllDrives=true",
                node.id
            ))?
        } else {
            Url::parse(&format!(
                "https://graph.microsoft.com/v1.0/drives/{}/items/{}/content",
                node.drive, node.id
            ))?
        };
        Ok((
            serde_json::from_value(node.public())?,
            Link {
                url,
                headers,
                concurrency: 0,
                max_range: 0,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::to_bytes,
        extract::{Request, State},
        routing::any,
    };
    use std::sync::Arc;
    #[test]
    fn share_urls_and_credentials() {
        for (url, expected) in [
            (
                "https://drive.google.com/file/d/file_123/view?resourcekey=key",
                "google",
            ),
            (
                "https://drive.google.com/drive/u/0/folders/folder-123",
                "google",
            ),
            ("https://drive.google.com/open?id=file123", "google"),
            ("https://1drv.ms/f/s!abcd", "onedrive"),
            ("https://onedrive.live.com/?id=abc&cid=123", "onedrive"),
            (
                "https://example-my.sharepoint.com/:f:/g/personal/example/token",
                "onedrive",
            ),
        ] {
            assert_eq!(crate::control::parse_share(url).unwrap().0, expected);
        }
        for url in [
            "https://drive.google.com.evil.test/file/d/abc/view",
            "https://drive.google.com/drive/folders/a%27b",
            "https://drive.google.com/drive/my-drive",
            "https://onedrive.live.com@evil.test/f/abc",
            "https://tenant.sharepoint.com.evil.test/:f:/a",
            "https://drive.google.com:8443/file/d/a/view",
        ] {
            assert!(crate::control::parse_share(url).is_err(), "{url}");
        }
        let v = credential("google", "1//normal-google_refresh", &json!({})).unwrap();
        assert_eq!(v["driver"], "GoogleDrive");
        assert_eq!(v["addition"]["root_folder_id"], "root");
        assert!(credential("google", "/etc/passwd", &json!({})).is_err());
        assert!(credential("google", "../private.json", &json!({})).is_err());
        assert!(credential("google", "fake-token", &json!({"source":"custom"})).is_err());
        let v=credential("onedrive", "fake-token", &json!({"source":"custom","client_id":"app","client_secret":"secret","redirect_uri":"https://example.test/callback"})).unwrap();
        assert_eq!(v["addition"]["use_online_api"], false);
    }
    struct Fixture {
        storages: Vec<Value>,
        refreshes: usize,
        hostile: bool,
    }
    async fn endpoint(State(state): State<Arc<Mutex<Fixture>>>, req: Request) -> Json<Value> {
        let path = req.uri().path().to_owned();
        let query = req.uri().query().unwrap_or("").to_owned();
        let mut state = state.lock().await;
        if path == "/api/auth/login" {
            return Json(json!({"code":200,"data":{"token":"gateway-test"}}));
        }
        if path.starts_with("/api/") {
            assert_eq!(req.headers()["authorization"], "gateway-test");
            if path == "/api/admin/storage/list" {
                return Json(json!({"code":200,"data":{"content":state.storages}}));
            }
            if path == "/api/admin/storage/update" {
                let raw = to_bytes(req.into_body(), 1_000_000).await.unwrap();
                let storage: Value = serde_json::from_slice(&raw).unwrap();
                let i = state
                    .storages
                    .iter()
                    .position(|s| s["id"] == storage["id"])
                    .unwrap();
                state.storages[i] = storage;
                return Json(json!({"code":200,"data":{}}));
            }
            panic!("Unexpected gateway mutation: {path}");
        }
        if path.ends_with("token") {
            let raw = to_bytes(req.into_body(), 65536).await.unwrap();
            let form: BTreeMap<_, _> =
                Url::parse(&format!("http://local/?{}", String::from_utf8_lossy(&raw)))
                    .unwrap()
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
            assert_eq!(form["client_id"], "device-test-client");
            assert!(!form.contains_key("client_secret") && !form.contains_key("redirect_uri"));
            state.refreshes += 1;
            return Json(
                json!({"access_token":"private-access","refresh_token":"private-rotated","expires_in":3600}),
            );
        }
        if path.ends_with("renewapi") {
            state.refreshes += 1;
            return Json(
                json!({"access_token":"private-access","refresh_token":"private-rotated","expires_in":3600}),
            );
        }
        assert_eq!(req.headers()["authorization"], "Bearer private-access");
        let v = match path.as_str() {
            "/drive/v3/files/single" => {
                json!({"id":"single","name":"single.txt","mimeType":"text/plain","size":"7","resourceKey":"single-key"})
            }
            "/drive/v3/files/folder" => {
                assert_eq!(
                    req.headers()["x-goog-drive-resource-keys"],
                    "folder/resource"
                );
                json!({"id":"folder","name":"资料","mimeType":"application/vnd.google-apps.folder","resourceKey":"resource"})
            }
            "/drive/v3/files" if query.contains("pageToken") => {
                json!({"files":[{"id":"file-b","name":"a.txt","size":"22","mimeType":"text/plain","resourceKey":"b-key"}]})
            }
            "/drive/v3/files" => {
                assert_eq!(
                    req.headers()["x-goog-drive-resource-keys"],
                    "folder/resource"
                );
                json!({"nextPageToken":"next","files":[{"id":"file-a","name":"a.txt","size":"11","mimeType":"text/plain"}]})
            }
            p if p.starts_with("/v1.0/shares/u!") => {
                json!({"id":"shared","name":"共享","folder":{},"parentReference":{"driveId":"drive-id"}})
            }
            "/v1.0/drives/drive-id/items/shared/children" if query.contains("skiptoken") => {
                json!({"value":[{"id":"file2","name":"b.bin","size":2}]})
            }
            "/v1.0/drives/drive-id/items/shared/children" => {
                json!({"value":[{"id":"file1","name":"a.bin","size":1}],"@odata.nextLink":if state.hostile{"https://evil.test/steal"}else{"https://graph.microsoft.com/v1.0/drives/drive-id/items/shared/children?skiptoken=next"}})
            }
            _ => panic!("Unexpected provider request: {path}"),
        };
        Json(v)
    }
    #[tokio::test]
    async fn shared_items_pagination_range_headers_rotation_and_restart() {
        let data = std::env::temp_dir().join(format!("flowdisk-external-{}", Uuid::new_v4()));
        fs::create_dir(&data).await.unwrap();
        let mut storages = Vec::new();
        for (id, provider) in [(1, "google"), (2, "onedrive")] {
            let mut c = credential(provider, "private-refresh", &json!({})).unwrap();
            if provider == "onedrive" {
                c["addition"]["api_url_address"] =
                    "http://downloader:8080/internal/onedrive/renew?key=fixture".into();
                c["addition"]["client_id"] = "device-test-client".into();
                c["addition"]["client_secret"] = "".into();
                c["addition"]["redirect_uri"] = "".into();
            }
            storages.push(json!({"id":id,"driver":c["driver"],"addition":serde_json::to_string(&c["addition"]).unwrap(),"status":"work","disabled":false,"mount_path":format!("/{provider}")}));
        }
        let state = Arc::new(Mutex::new(Fixture {
            storages,
            refreshes: 0,
            hostile: false,
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(endpoint))
                    .with_state(state.clone()),
            )
            .into_future(),
        );
        let gateway = Gateway::fixture(&data, base.clone());
        let mut external = External::new(&data).unwrap();
        external.test_base = Some(base);
        let storage = state.lock().await.storages[0].clone();
        let roots = external
            .prepare(
                &gateway,
                &storage,
                Uuid::nil(),
                "https://drive.google.com/drive/folders/folder?resourcekey=resource",
            )
            .await
            .unwrap();
        let listed = external
            .fs(
                &gateway,
                "/api/fs/list",
                &json!({"path":roots[0],"page":1,"per_page":0}),
            )
            .await
            .unwrap();
        assert_eq!(listed["total"], 2);
        assert_eq!(listed["content"][0]["name"], "a.txt (file-a)");
        let path = format!("{}/a.txt (file-b)", roots[0]);
        let (metadata, link) = external.resolve(&gateway, &path).await.unwrap();
        assert_eq!(metadata.size, 22);
        assert_eq!(link.headers["authorization"], "Bearer private-access");
        assert_eq!(link.headers["x-goog-drive-resource-keys"], "file-b/b-key");
        assert!(link.url.query().unwrap().contains("alt=media"));
        assert!(!listed.to_string().contains("private-"));
        assert_eq!(state.lock().await.refreshes, 1);
        assert!(
            external
                .fs(&gateway, "/api/fs/remove", &json!({"path":roots[0]}))
                .await
                .is_err()
        );
        let reloaded = External::new(&data).unwrap();
        assert_eq!(reloaded.node(&path).await.unwrap().size, 22);
        let storage = state.lock().await.storages[0].clone();
        let single = external
            .prepare(
                &gateway,
                &storage,
                Uuid::new_v4(),
                "https://drive.google.com/file/d/single/view",
            )
            .await
            .unwrap();
        let (metadata, link) = external.resolve(&gateway, &single[0]).await.unwrap();
        assert_eq!(metadata.size, 7);
        assert!(!metadata.is_dir);
        assert_eq!(
            link.headers["x-goog-drive-resource-keys"],
            "single/single-key"
        );
        let storage = state.lock().await.storages[1].clone();
        let roots = external
            .prepare(
                &gateway,
                &storage,
                Uuid::new_v4(),
                "https://1drv.ms/f/s!shared",
            )
            .await
            .unwrap();
        let listed = external
            .fs(&gateway, "/api/fs/list", &json!({"path":roots[0]}))
            .await
            .unwrap();
        assert_eq!(listed["total"], 2);
        let (_, link) = external
            .resolve(&gateway, &format!("{}/b.bin", roots[0]))
            .await
            .unwrap();
        assert!(link.url.path().ends_with("/file2/content"));
        state.lock().await.hostile = true;
        assert!(
            external
                .fs(&gateway, "/api/fs/list", &json!({"path":roots[0]}))
                .await
                .is_err()
        );
        assert_eq!(state.lock().await.refreshes, 2);
        let stored: Value =
            serde_json::from_str(state.lock().await.storages[1]["addition"].as_str().unwrap())
                .unwrap();
        assert_eq!(stored["refresh_token"], "private-rotated");
        server.abort();
        fs::remove_dir_all(data).await.unwrap();
    }
}
