//! Import shares through the signed-in account's official Thunder endpoints.
use crate::gateway::Gateway;
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, time::Duration};
use tokio::fs;
use uuid::Uuid;

struct Thunder {
    client: Client,
    api: Url,
    auth: Url,
    addition: Value,
    token: Value,
    captcha: String,
}
fn checked(v: &Value) -> Result<()> {
    if v["error"]
        .as_str()
        .is_none_or(|s| s.is_empty() || s == "success")
        && v["error_code"].as_i64().unwrap_or(0) == 0
    {
        return Ok(());
    }
    let message = match v["error"].as_str().unwrap_or("") {
        "invalid_grant" | "unauthenticated" | "invalid_token" => "迅雷登录已失效，请重新登录",
        "pass_code_invalid" | "invalid_pass_code" | "invalid_password" => "迅雷分享提取码错误",
        "not_found" | "share_not_found" => "迅雷分享或文件不存在，可能已失效",
        "captcha_invalid" | "captcha_required" | "review_panel" => {
            "请先在迅雷官方网盘完成安全验证后重试"
        }
        "resource_exhausted" | "too_many_requests" => "迅雷空间不足或操作过于频繁，请检查后重试",
        _ => "迅雷拒绝此操作，请检查分享状态、提取码、空间和权限",
    };
    // Do not expose upstream bodies, which may contain account or captcha tokens.
    bail!(
        "{message}（代码 {}）",
        v["error_code"].as_i64().unwrap_or(0)
    )
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("迅雷响应缺少必要字段：{name}"))
}
fn complete(source: &[Value], saved: &[Value]) -> bool {
    source.len() == saved.len()
        && source.iter().all(|s| {
            saved.iter().any(|v| {
                v["name"] == s["name"]
                    && v["kind"] == s["kind"]
                    && (s["kind"] == "drive#folder" || v["size"] == s["size"])
            })
        })
}
impl Thunder {
    fn new(addition: Value) -> Result<Self> {
        ensure!(
            addition["login_type"] == "refresh_token",
            "请先使用手机验证码或扫码重新登录迅雷"
        );
        Ok(Self {
            client: Client::builder()
                .user_agent(field(&addition, "user_agent")?)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(30))
                .build()?,
            api: Url::parse("https://api-pan.xunlei.com/drive/v1/")?,
            auth: Url::parse("https://xluser-ssl.xunlei.com/")?,
            captcha: addition["captcha_token"].as_str().unwrap_or("").into(),
            addition,
            token: json!({}),
        })
    }
    async fn http(&self, method: Method, url: Url, body: Option<&Value>) -> Result<Value> {
        let mut r = self
            .client
            .request(method, url)
            .header("X-Device-Id", field(&self.addition, "device_id")?)
            .header("X-Client-Id", field(&self.addition, "client_id")?)
            .header("X-Client-Version", field(&self.addition, "client_version")?)
            .header("X-Captcha-Token", &self.captcha);
        if let Some(access) = self.token["access_token"].as_str() {
            r = r.bearer_auth(access);
        }
        if let Some(body) = body {
            r = r.json(body);
        }
        let response = r
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("无法连接迅雷官方网盘")?;
        let status = response.status();
        let v: Value = response
            .json()
            .await
            .map_err(|e| e.without_url())
            .context("迅雷返回了无效响应")?;
        if status.is_success() || v.get("error").is_some() {
            return Ok(v);
        }
        bail!("迅雷接口暂时不可用（HTTP {}）", status.as_u16())
    }
    async fn refresh(&mut self) -> Result<()> {
        let v = self.http(Method::POST, self.auth.join("v1/auth/token")?, Some(&json!({
            "grant_type":"refresh_token","refresh_token":field(&self.addition,"refresh_token")?,
            "client_id":field(&self.addition,"client_id")?,"client_secret":field(&self.addition,"client_secret")?
        }))).await?;
        checked(&v)?;
        field(&v, "access_token")?;
        self.addition["refresh_token"] = field(&v, "refresh_token")?.into();
        self.token = v;
        Ok(())
    }
    async fn captcha(&mut self, action: &str) -> Result<()> {
        let timestamp = (crate::now() * 1000).to_string();
        let mut sign = format!(
            "{}{}{}{}{timestamp}",
            field(&self.addition, "client_id")?,
            field(&self.addition, "client_version")?,
            field(&self.addition, "package_name")?,
            field(&self.addition, "device_id")?
        );
        ensure!(
            self.addition["sign_type"] == "algorithms",
            "请重新使用手机验证码登录迅雷以更新接口配置"
        );
        for salt in field(&self.addition, "algorithms")?.split(',') {
            sign = format!("{:x}", md5::compute(format!("{sign}{salt}")));
        }
        let v = self.http(Method::POST, self.auth.join("v1/shield/captcha/init")?, Some(&json!({
            "action":action,"client_id":self.addition["client_id"],"device_id":self.addition["device_id"],
            "captcha_token":self.captcha,"meta":{"client_version":self.addition["client_version"],
            "package_name":self.addition["package_name"],"timestamp":timestamp,"captcha_sign":format!("1.{sign}"),
            "user_id":self.token["sub"].as_str().or(self.token["user_id"].as_str()).unwrap_or("")}
        }))).await?;
        checked(&v)?;
        ensure!(
            v["url"].as_str().is_none_or(str::is_empty),
            "请先在迅雷官方网盘完成安全验证后重试"
        );
        self.captcha = field(&v, "captcha_token")?.into();
        Ok(())
    }
    async fn request(
        &mut self,
        method: Method,
        route: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut url = self.api.join(route)?;
        url.query_pairs_mut().extend_pairs(query);
        for attempt in 0..2 {
            let v = self
                .http(method.clone(), url.clone(), body.as_ref())
                .await?;
            if attempt == 0 && (v["error_code"] == 9 || v["error"] == "captcha_invalid") {
                self.captcha(&format!("{method}:{}", url.path())).await?;
                continue;
            }
            checked(&v)?;
            return Ok(v);
        }
        unreachable!()
    }
    async fn list(&mut self, parent: &str) -> Result<Vec<Value>> {
        self.pages(
            "files",
            vec![
                ("parent_id", parent.into()),
                (
                    "space",
                    self.addition["space"].as_str().unwrap_or("").into(),
                ),
                ("filters", r#"{"trashed":{"eq":false}}"#.into()),
            ],
        )
        .await
    }
    async fn pages(&mut self, route: &str, mut query: Vec<(&str, String)>) -> Result<Vec<Value>> {
        query.push(("limit", "100".into()));
        query.push(("page_token", String::new()));
        let mut all = Vec::new();
        let mut seen = BTreeSet::new();
        for _ in 0..50 {
            let v = self.request(Method::GET, route, &query, None).await?;
            if let Some(status) = v["share_status"].as_str() {
                ensure!(
                    status == "OK",
                    "迅雷分享不可用（已失效、提取码错误或访问受限）"
                );
            }
            let rows = v["files"].as_array().context("迅雷目录响应无效")?;
            for row in rows {
                ensure!(
                    crate::control::remote_name(field(row, "name")?),
                    "迅雷文件名不合法"
                );
                field(row, "id")?;
            }
            all.extend(rows.clone());
            let next = v["next_page_token"].as_str().unwrap_or("");
            if next.is_empty() {
                return Ok(all);
            }
            ensure!(seen.insert(next.to_owned()), "迅雷分页响应重复，已停止转存");
            query.last_mut().unwrap().1 = next.into();
        }
        bail!("迅雷目录过大，请拆分分享任务")
    }
    async fn folder(&mut self, parent: &str, name: &str) -> Result<String> {
        let rows = self.list(parent).await?;
        let found: Vec<_> = rows.iter().filter(|f| f["name"] == name).collect();
        ensure!(
            found.len() <= 1,
            "迅雷接收位置存在重复名称，请在官方网盘整理后重试"
        );
        if let Some(row) = found.first() {
            ensure!(row["kind"] == "drive#folder", "迅雷接收目录被同名文件占用");
            return Ok(field(row, "id")?.into());
        }
        let v = self.request(Method::POST, "files", &[], Some(json!({"kind":"drive#folder","name":name,"parent_id":parent,"space":self.addition["space"].as_str().unwrap_or("")}))).await?;
        Ok(field(v.get("file").unwrap_or(&v), "id")?.into())
    }
    async fn import(&mut self, data: &Path, id: Uuid, url: &str, pass: &str) -> Result<Value> {
        self.execute(data, id, url, pass, false).await
    }
    async fn execute(
        &mut self,
        data: &Path,
        id: Uuid,
        url: &str,
        pass: &str,
        inspect: bool,
    ) -> Result<Value> {
        let (provider, parsed, _) = crate::control::parse_share(url)?;
        ensure!(provider == "thunder", "请输入迅雷官方分享链接");
        let url = Url::parse(&parsed)?;
        let share = url.path().trim_end_matches('/').rsplit('/').next().unwrap();
        let v = self
            .request(
                Method::GET,
                "share",
                &[
                    ("share_id", share.into()),
                    ("pass_code", pass.into()),
                    ("limit", "100".into()),
                ],
                None,
            )
            .await?;
        ensure!(
            v["share_status"] == "OK",
            "迅雷分享不可用（已失效、提取码错误或访问受限）"
        );
        let token = field(&v, "pass_code_token")?.to_owned();
        let source = self
            .pages(
                "share/detail",
                vec![
                    ("share_id", share.into()),
                    ("parent_id", String::new()),
                    ("pass_code_token", token.clone()),
                ],
            )
            .await?;
        ensure!(!source.is_empty(), "迅雷分享为空");
        let names: BTreeSet<_> = source.iter().map(|f| f["name"].as_str()).collect();
        ensure!(
            names.len() == source.len(),
            "分享含重复文件名，请拆分分享后重试"
        );
        if inspect {
            let mut pending = source;
            let mut seen = BTreeSet::new();
            let mut bytes = 0u64;
            let mut count = 0;
            while let Some(file) = pending.pop() {
                ensure!(
                    seen.insert(field(&file, "id")?.to_owned()),
                    "迅雷分享目录出现循环或重复标识"
                );
                ensure!(seen.len() <= 5000, "分享超过 5000 项，请拆分分享");
                if file["kind"] == "drive#folder" {
                    pending.extend(
                        self.pages(
                            "share/detail",
                            vec![
                                ("share_id", share.into()),
                                ("parent_id", field(&file, "id")?.into()),
                                ("pass_code_token", token.clone()),
                            ],
                        )
                        .await?,
                    );
                } else {
                    let size = file["size"]
                        .as_u64()
                        .or_else(|| file["size"].as_str().and_then(|s| s.parse().ok()))
                        .context("迅雷分享文件大小缺失")?;
                    bytes = bytes.checked_add(size).context("分享大小超出范围")?;
                    count += 1;
                }
            }
            let usage = self
                .request(Method::GET, "about", &[], None)
                .await
                .unwrap_or(Value::Null);
            let number = |v: &Value| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            };
            return Ok(
                json!({"required_bytes":bytes,"file_count":count,"total_space":number(&usage["quota"]["limit"]),"used_space":number(&usage["quota"]["usage"])}),
            );
        }
        let root = self.addition["root_folder_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let root = self.folder(&root, "FlowDisk分享").await?;
        let name = format!("分享-{id}");
        let target = self.folder(&root, &name).await?;
        let marker = data.join(format!("thunder-share-{id}.json"));
        let saved = self.list(&target).await?;
        let previous = if fs::try_exists(&marker).await? {
            Some(
                serde_json::from_slice::<Value>(&fs::read(&marker).await?)
                    .context("迅雷转存记录损坏，拒绝重复转存")?,
            )
        } else {
            None
        };
        if !complete(&source, &saved) || previous.as_ref().is_some_and(|v| v["ready"] != true) {
            let state = if let Some(state) = previous {
                state
            } else {
                ensure!(
                    saved.is_empty(),
                    "上次迅雷转存不完整，请检查接收目录，避免重复保存"
                );
                // Commit intent BEFORE the remote mutation. A lost response must not duplicate a transfer.
                write_marker(&marker, &json!({"submitted":true})).await?;
                let state = self.request(Method::POST, "share/restore", &[], Some(json!({
                    "share_id":share,"pass_code_token":token,"file_ids":source.iter().map(|f|f["id"].clone()).collect::<Vec<_>>(),
                    "parent_id":target,"specify_parent_id":true,"ancestor_ids":[],"folder_type":"NORMAL"
                }))).await?;
                write_marker(&marker, &state).await?;
                state
            };
            ensure!(
                state["submitted"] != true,
                "上次迅雷转存响应丢失，请在官方网盘检查接收目录；为避免重复转存，已停止自动提交"
            );
            if let Some(task) = state["task_id"].as_str().or(state["task"]["id"].as_str()) {
                ensure!(
                    task.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                    "迅雷转存任务编号无效"
                );
                for i in 0..120 {
                    let v = self
                        .request(Method::GET, &format!("tasks/{task}"), &[], None)
                        .await?;
                    let phase = v["phase"]
                        .as_str()
                        .or(v["task"]["phase"].as_str())
                        .unwrap_or("");
                    ensure!(
                        phase != "PHASE_TYPE_ERROR",
                        "迅雷转存任务失败，请检查空间和权限"
                    );
                    if phase == "PHASE_TYPE_COMPLETE" {
                        break;
                    }
                    ensure!(i < 119, "迅雷仍在转存，请稍后重试，将继续等待原任务");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            let mut ready = false;
            for _ in 0..10 {
                if complete(&source, &self.list(&target).await?) {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            ensure!(
                ready,
                "迅雷转存尚未完整完成，请检查接收目录后重试；不会重复提交转存"
            );
            write_marker(&marker, &json!({"ready":true})).await?;
        }
        Ok(json!({"paths":[format!("/FlowDisk分享/{name}")]}))
    }
}

async fn write_marker(path: &Path, value: &Value) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value)?).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    fs::File::open(&tmp).await?.sync_all().await?;
    fs::rename(tmp, path).await?;
    Ok(())
}
pub async fn import_share(
    gateway: &Gateway,
    data: &Path,
    storage: &Value,
    id: Uuid,
    url: &str,
    pass: &str,
    inspect: bool,
) -> Result<Value> {
    let addition: Value =
        serde_json::from_str(storage["addition"].as_str().context("迅雷账号配置缺失")?)?;
    let mut thunder = Thunder::new(addition)?;
    thunder.refresh().await?;
    // Persist rotated refresh tokens even if a later share operation fails.
    let mut updated = storage.clone();
    updated["addition"] = serde_json::to_string(&thunder.addition)?.into();
    gateway.post("/api/admin/storage/update", updated).await?;
    if inspect {
        thunder.execute(data, id, url, pass, true).await
    } else {
        thunder.import(data, id, url, pass).await
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
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct Fixture {
        root: bool,
        target: bool,
        restores: usize,
        fail_task: bool,
        challenge: bool,
        captcha: usize,
    }
    fn source() -> Vec<Value> {
        vec![
            json!({"id":"folder","name":"目录","kind":"drive#folder","size":"0"}),
            json!({"id":"file","name":"文件.bin","kind":"drive#file","size":"123"}),
        ]
    }
    async fn endpoint(State(state): State<Arc<Mutex<Fixture>>>, req: Request) -> Json<Value> {
        let path = req.uri().path().to_owned();
        let method = req.method().clone();
        let url = Url::parse(&format!("http://local{}", req.uri())).unwrap();
        let q: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(req.headers()["x-device-id"], "unit-device");
        let mut state = state.lock().await;
        if path.starts_with("/drive/") {
            assert_eq!(req.headers()["authorization"], "Bearer private-access");
        }
        let body = to_bytes(req.into_body(), 32768).await.unwrap();
        let body: Value = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        Json(match (method.as_str(), path.as_str()) {
            ("POST", "/v1/auth/token") => {
                assert_eq!(body["refresh_token"], "private-refresh");
                json!({"access_token":"private-access","refresh_token":"private-rotated","sub":"unit-user"})
            }
            ("POST", "/v1/shield/captcha/init") => {
                state.captcha += 1;
                assert_eq!(body["action"], "GET:/drive/v1/share");
                assert!(
                    body["meta"]["captcha_sign"]
                        .as_str()
                        .unwrap()
                        .starts_with("1.")
                );
                if state.challenge {
                    json!({"captcha_token":"private-captcha","url":"https://i.xunlei.com/check"})
                } else {
                    json!({"captcha_token":"private-captcha"})
                }
            }
            ("GET", "/drive/v1/share") => {
                if state.captcha == 0 {
                    json!({"error":"captcha_invalid","error_code":9})
                } else {
                    assert_eq!(q["pass_code"], "abcd");
                    json!({"share_status":"OK","pass_code_token":"private-pass-token"})
                }
            }
            ("GET", "/drive/v1/share/detail") => {
                assert_eq!(q["pass_code_token"], "private-pass-token");
                if q["parent_id"] == "folder" {
                    json!({"files":[{"id":"nested","name":"nested.bin","kind":"drive#file","size":"7"}]})
                } else if q["page_token"].is_empty() {
                    json!({"files":[source()[0]],"next_page_token":"page2"})
                } else {
                    json!({"files":[source()[1]]})
                }
            }
            ("GET", "/drive/v1/files") => {
                let rows = match q["parent_id"].as_ref() {
                    "" if state.root => {
                        vec![json!({"id":"root","name":"FlowDisk分享","kind":"drive#folder"})]
                    }
                    "root" if state.target => vec![
                        json!({"id":"target","name":format!("分享-{}",Uuid::nil()),"kind":"drive#folder"}),
                    ],
                    "target" if state.restores > 0 => source(),
                    _ => vec![],
                };
                json!({"files":rows})
            }
            ("POST", "/drive/v1/files") => {
                if body["parent_id"] == "" {
                    state.root = true;
                    json!({"file":{"id":"root"},"task":null})
                } else {
                    assert_eq!(body["parent_id"], "root");
                    state.target = true;
                    json!({"file":{"id":"target"},"task":null})
                }
            }
            ("POST", "/drive/v1/share/restore") => {
                state.restores += 1;
                assert_eq!(body["file_ids"], json!(["folder", "file"]));
                assert_eq!(body["parent_id"], "target");
                assert_eq!(body["specify_parent_id"], true);
                json!({"task_id":"task1"})
            }
            ("GET", "/drive/v1/tasks/task1") => {
                json!({"phase":if state.fail_task {"PHASE_TYPE_ERROR"}else{"PHASE_TYPE_COMPLETE"}})
            }
            ("GET", "/drive/v1/about") => json!({"quota":{"limit":"1000","usage":"900"}}),
            _ => panic!("Unexpected endpoint {method} {path}"),
        })
    }
    #[tokio::test]
    async fn share_protocol_pagination_retry_and_private_credentials() {
        let state = Arc::new(Mutex::new(Fixture::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(endpoint))
                    .with_state(state.clone()),
            )
            .into_future(),
        );
        let mut client = Thunder::new(json!({"login_type":"refresh_token","sign_type":"algorithms","user_agent":"unit-test", "device_id":"unit-device","client_id":"unit-client","client_secret":"unit-secret","client_version":"1","package_name":"unit","algorithms":"salt","refresh_token":"private-refresh"})).unwrap();
        client.auth = Url::parse(&base).unwrap();
        client.api = Url::parse(&format!("{base}drive/v1/")).unwrap();
        client.refresh().await.unwrap();
        assert_eq!(client.addition["refresh_token"], "private-rotated");
        let data = std::env::temp_dir().join(format!("flowdisk-thunder-{}", Uuid::new_v4()));
        fs::create_dir(&data).await.unwrap();
        let url = "https://pan.xunlei.com/s/test";
        state.lock().await.challenge = true;
        assert!(
            client
                .import(&data, Uuid::nil(), url, "abcd")
                .await
                .unwrap_err()
                .to_string()
                .contains("安全验证")
        );
        assert_eq!(state.lock().await.restores, 0);
        state.lock().await.challenge = false;
        let estimate = client
            .execute(&data, Uuid::nil(), url, "abcd", true)
            .await
            .unwrap();
        assert_eq!(estimate["required_bytes"], 130);
        assert_eq!(estimate["file_count"], 2);
        assert_eq!(estimate["total_space"], 1000);
        assert_eq!(estimate["used_space"], 900);
        assert_eq!(state.lock().await.restores, 0);
        assert!(!state.lock().await.root);
        state.lock().await.fail_task = true;
        // A destination folder may already exist while its copy task is still failing.
        assert!(
            client
                .import(&data, Uuid::nil(), url, "abcd")
                .await
                .unwrap_err()
                .to_string()
                .contains("转存任务失败")
        );
        assert_eq!(state.lock().await.restores, 1);
        assert!(
            client
                .import(&data, Uuid::nil(), url, "abcd")
                .await
                .is_err()
        );
        assert_eq!(state.lock().await.restores, 1);
        state.lock().await.fail_task = false;
        let result = client
            .import(&data, Uuid::nil(), url, "abcd")
            .await
            .unwrap();
        assert!(
            result["paths"][0]
                .as_str()
                .unwrap()
                .starts_with("/FlowDisk分享/分享-")
        );
        assert!(!result.to_string().contains("private-"));
        client
            .import(&data, Uuid::nil(), url, "abcd")
            .await
            .unwrap();
        assert_eq!(state.lock().await.restores, 1);
        write_marker(
            &data.join(format!("thunder-share-{}.json", Uuid::nil())),
            &json!({"submitted":true}),
        )
        .await
        .unwrap();
        assert!(
            client
                .import(&data, Uuid::nil(), url, "abcd")
                .await
                .unwrap_err()
                .to_string()
                .contains("响应丢失")
        );
        assert_eq!(state.lock().await.restores, 1);
        assert!(
            client
                .import(&data, Uuid::nil(), "https://evil.invalid/s/test", "abcd")
                .await
                .is_err()
        );
        let err = checked(
            &json!({"error":"unknown","error_code":42,"error_description":"private-access"}),
        )
        .unwrap_err();
        assert!(!err.to_string().contains("private-"));
        fs::remove_dir_all(data).await.unwrap();
        server.abort();
    }
}
