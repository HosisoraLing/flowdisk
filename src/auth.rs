//! Direct provider authentication. SMS is an official form in the user's browser;
//! QR tickets and credentials are exchanged by Rust. No browser process or input relay.
use crate::{ApiError, ApiResult, App, bad, now};
use anyhow::{Result, ensure};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::Mutex;
use uuid::Uuid;

const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/132.0.0.0 Safari/537.36";
const DRIVE: &str = "https://drive-pc.quark.cn/1/clouddrive";
pub struct Auth {
    client: reqwest::Client,
    thunder_base: Url,
    sms_sent: Mutex<BTreeMap<String, u64>>,
    sessions: Mutex<BTreeMap<Uuid, Session>>,
    credentials: Mutex<BTreeMap<String, Value>>,
}
struct Session {
    id: Uuid,
    provider: String,
    mode: String,
    status: String,
    message: String,
    expires: u64,
    last_poll: u64,
    public: Value,
    private: Value,
    credential: Option<Value>,
    cookies: BTreeMap<String, String>,
}
impl Session {
    fn view(&self) -> Value {
        let mut v = self.public.clone();
        for (k, val) in [
            ("id", json!(self.id)),
            ("provider", json!(self.provider)),
            ("mode", json!(self.mode)),
            ("purpose", json!("login")),
            ("status", json!(self.status)),
            ("message", json!(self.message)),
            ("expires_at", json!(self.expires * 1000)),
        ] {
            v[k] = val;
        }
        v
    }
}
pub fn methods(key: &str) -> Vec<&'static str> {
    match key {
        "quark" => vec!["sms", "qr", "credential"],
        "onedrive" | "google" => vec!["oauth", "credential"],
        "115" => vec!["qr", "credential"],
        "thunder" => vec!["sms", "qr", "credential"],
        "baidu" if oauth_config().is_ok() => vec!["qr", "cookie", "oauth", "credential"],
        "baidu" => vec!["qr", "cookie", "credential"],
        _ => vec!["credential"],
    }
}
fn thunder_token_request(private: &Value) -> Value {
    // This device authorization flow is a public-client request. The official
    // endpoint rejects a browser UA carrying client_secret (permission_denied/7).
    json!({"client_id":"Xp6vsxz_7IYVw2BB","grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":private["device_code"]})
}
fn thunder_qr_url(value: &Value) -> ApiResult<Url> {
    let code = value["user_code"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.contains(['\r', '\n']))
        .ok_or_else(|| bad("迅雷未返回扫码确认码"))?;
    // The Android client's default /__/auth/device/ UI returns HTTP 404.
    // The official account center consumes the same client_id/scope/user_code.
    let page = Url::parse_with_params(
        "https://i.xunlei.com/center/account/personal/qrcode-login/",
        [
            ("client_id", "Xp6vsxz_7IYVw2BB"),
            ("scope", "profile offline pan user"),
            ("user_code", code),
        ],
    )
    .unwrap();
    Ok(Url::parse_with_params(
        "https://xluser-ssl.xunlei.com/xluser.core.login/v3/qrlogin",
        [("redirect_uri", page.as_str())],
    )
    .unwrap())
}
fn thunder_phone(value: &str) -> ApiResult<String> {
    let value = value.trim();
    let number = value.strip_prefix("+86").unwrap_or(value);
    if number.len() != 11
        || !number.bytes().all(|c| c.is_ascii_digit())
        || !number.starts_with('1')
        || !matches!(number.as_bytes()[1], b'3'..=b'9')
    {
        return Err(bad("请输入有效的中国大陆 11 位手机号"));
    }
    Ok(format!("+86{number}"))
}
fn thunder_error(value: &Value) -> ApiResult<()> {
    match value["error"].as_str().unwrap_or("") {
        "" => Ok(()),
        "resource_exhausted" | "too_many_requests" => Err(bad("迅雷请求过于频繁，请稍后重试")),
        "invalid_verification_code" => Err(bad("短信验证码错误，请检查后重试")),
        "expired_verification_code" => Err(bad("短信验证码已过期，请重新获取")),
        "captcha_invalid" | "captcha_required" => Err(bad("迅雷安全验证已过期，请重新获取验证码")),
        "user_not_found" | "not_found" => Err(bad("该手机号未注册迅雷账号，请先在迅雷 App 注册")),
        "invalid_argument" => Err(bad("手机号或验证码无效，请检查后重试")),
        _ => Err(bad("迅雷暂时拒绝登录，请稍后重试或使用扫码")),
    }
}
fn thunder_credential(token: &str, device: &Value) -> Value {
    json!({"driver":"ThunderExpert","addition":{"root_folder_id":"","login_type":"refresh_token","refresh_token":token,"sign_type":"algorithms","client_id":"Xp6vsxz_7IYVw2BB","client_secret":"Xp6vsy4tN9toTVdMSpomVdXpRmES","device_id":device}})
}
fn oauth_config() -> Result<(String, String, String)> {
    let id = std::env::var("BAIDU_CLIENT_ID")?;
    let secret = std::env::var("BAIDU_CLIENT_SECRET")?;
    let base = Url::parse(&std::env::var("PUBLIC_URL")?)?;
    ensure!(
        !id.is_empty()
            && !secret.is_empty()
            && matches!(base.scheme(), "http" | "https")
            && base.username().is_empty()
            && base.password().is_none(),
        "百度 OAuth 配置无效"
    );
    Ok((id, secret, base.join("/oauth/baidu/callback")?.to_string()))
}
fn qr_svg(content: &str) -> Result<String> {
    Ok(qrcode::QrCode::new(content)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build())
}
fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}
fn parse_cookie(raw: &str) -> BTreeMap<String, String> {
    raw.split(';')
        .filter_map(|s| {
            s.trim()
                .split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect()
}
fn addition(provider: &str, cookie: &str) -> Value {
    json!({"driver":if provider=="quark"{"Quark"}else{"115 Cloud"},"addition":{"cookie":cookie,"root_folder_id":"0","qrcode_source":"web","page_size":100,"limit_rate":2}})
}
impl Auth {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(UA)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(30))
                .build()?,
            thunder_base: Url::parse("https://xluser-ssl.xunlei.com/")?,
            sms_sent: Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(BTreeMap::new()),
            credentials: Mutex::new(BTreeMap::new()),
        })
    }
    async fn http(
        &self,
        url: &str,
        method: Method,
        body: Option<Value>,
        cookies: &mut BTreeMap<String, String>,
    ) -> ApiResult<Value> {
        // Only hard-coded official endpoints enter here; share URLs are parsed separately.
        let mut r = self
            .client
            .request(method, url)
            .header("Referer", "https://pan.quark.cn/");
        if !cookies.is_empty() {
            r = r.header("Cookie", cookie_header(cookies));
        }
        if let Some(body) = body {
            r = r.json(&body);
        }
        let resp = r.send().await.map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                "官方网盘暂时无法连接，请稍后重试".into(),
            )
        })?;
        for value in resp.headers().get_all("set-cookie") {
            if let Ok(value) = value.to_str()
                && let Some((name, value)) = value.split(';').next().unwrap_or("").split_once('=')
            {
                cookies.insert(name.into(), value.into());
            }
        }
        if !resp.status().is_success() {
            return Err(bad("官方网盘拒绝请求，请稍后重试或重新授权"));
        }
        resp.json()
            .await
            .map_err(|_| bad("官方网盘返回了无效数据，请稍后重试"))
    }
    pub async fn request(&self, path: &str, method: Method, body: Value) -> ApiResult<Value> {
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        if parts == ["sessions"] && method == Method::POST {
            return self.start(&body).await;
        }
        if parts.first() == Some(&"sessions") && parts.len() >= 2 {
            let id = Uuid::parse_str(parts[1]).map_err(|_| bad("登录会话无效"))?;
            let mut all = self.sessions.lock().await;
            if method == Method::DELETE {
                all.remove(&id);
                return Ok(json!({"ok":true}));
            }
            let s = all
                .get_mut(&id)
                .ok_or_else(|| bad("登录会话已结束，请重新登录"))?;
            if now() >= s.expires {
                s.credential = None;
                s.private = json!({});
                s.cookies.clear();
                s.status = "expired".into();
                s.message = "登录已过期，请重新开始".into();
                return Ok(s.view());
            }
            if parts.get(2) == Some(&"credentials") {
                return s.credential.clone().ok_or_else(|| bad("尚未完成网盘授权"));
            }
            if parts.get(2) == Some(&"action") && method == Method::POST {
                self.action(s, &body).await?;
            } else if method == Method::GET {
                self.poll(s).await?;
            }
            return Ok(s.view());
        }
        if parts.first() == Some(&"providers") && parts.len() >= 2 {
            let key = parts[1];
            if method == Method::DELETE {
                self.credentials.lock().await.remove(key);
                self.sessions.lock().await.retain(|_, s| s.provider != key);
                return Ok(json!({"ok":true}));
            }
            if parts.get(2) == Some(&"credentials") {
                self.credentials.lock().await.insert(key.into(), body);
                return Ok(json!({"ok":true}));
            }
            if parts.get(2) == Some(&"share") {
                return self.share(key, &body).await;
            }
        }
        Err(bad("登录接口不存在"))
    }
    pub async fn reject(&self, id: Uuid) {
        if let Some(s) = self.sessions.lock().await.get_mut(&id) {
            s.credential = None;
            s.status = if ["credential", "cookie"].contains(&s.mode.as_str()) {
                "waiting"
            } else {
                "failed"
            }
            .into();
            s.message = "授权验证失败，请修改授权信息或重新登录".into();
        }
    }
    async fn start(&self, body: &Value) -> ApiResult<Value> {
        let key = body["provider"].as_str().unwrap_or("");
        let mode = body["mode"].as_str().unwrap_or("sms");
        if !methods(key).contains(&mode) {
            return Err(bad("此网盘暂不支持这个登录方式，请使用界面中可用的方式"));
        }
        let mut s = Session {
            id: Uuid::new_v4(),
            provider: key.into(),
            mode: mode.into(),
            status: "waiting".into(),
            message: String::new(),
            expires: now() + 600,
            last_poll: 0,
            public: json!({}),
            private: json!({}),
            credential: None,
            cookies: BTreeMap::new(),
        };
        match (key, mode) {
            ("quark", "sms") => {
                s.public["form_url"] = json!(
                    "https://uop.quark.cn/cas/custom/login?custom_login_type=mobile&client_id=532&display=pc&origin=*"
                );
                s.public["message_origin"] = json!("https://uop.quark.cn");
                s.message = "请输入手机号和短信验证码；如需滑块，请在表单中完成".into();
            }
            ("quark", "qr") => {
                let url = Url::parse_with_params(
                    "https://uop.quark.cn/cas/ajax/getTokenForQrcodeLogin",
                    [
                        ("client_id", "532"),
                        ("v", "1.2"),
                        ("request_id", &Uuid::new_v4().to_string()),
                    ],
                )
                .unwrap();
                let v = self
                    .http(url.as_str(), Method::GET, None, &mut s.cookies)
                    .await?;
                let token = v["data"]["members"]["token"]
                    .as_str()
                    .ok_or_else(|| bad("夸克二维码获取失败，请重试"))?;
                let qr = Url::parse_with_params(
                    "https://su.quark.cn/4_eMHBJ",
                    [
                        ("token", token),
                        ("client_id", "532"),
                        ("ssb", "weblogin"),
                        ("uc_param_str", ""),
                        (
                            "uc_biz_str",
                            "S:custom|OPT:SAREA@0|OPT:IMMERSIVE@1|OPT:BACK_BTN_STYLE@0",
                        ),
                    ],
                )
                .unwrap();
                s.public["qr_svg"] = qr_svg(qr.as_str())?.into();
                s.private["token"] = token.into();
                s.expires = now() + 300;
                s.message = "请用夸克 App 扫码并确认登录".into();
            }
            ("115", "qr") => {
                let v = self
                    .http(
                        "https://qrcodeapi.115.com/api/1.0/web/1.0/token",
                        Method::GET,
                        None,
                        &mut BTreeMap::new(),
                    )
                    .await?;
                let qr = v["data"]["qrcode"]
                    .as_str()
                    .ok_or_else(|| bad("115 二维码获取失败，请重试"))?;
                s.public["qr_svg"] = qr_svg(qr)?.into();
                s.private = v["data"].clone();
                s.expires = now() + 300;
                s.message = "请用 115 App 扫码并确认登录".into();
            }
            ("thunder", "sms") => {
                s.private = json!({"device_id":Uuid::new_v4().simple().to_string()});
                s.expires = now() + 600;
                s.message = "输入中国大陆手机号，点击获取验证码，再填写短信验证码登录。".into();
            }
            ("thunder", "qr") => {
                let device = Uuid::new_v4().simple().to_string();
                let response = self.client.post("https://xluser-ssl.xunlei.com/v1/auth/device/code")
                    .header("X-Device-Id", &device)
                    .json(&json!({"client_id":"Xp6vsxz_7IYVw2BB","scope":"profile offline pan user","device_id":device}))
                    .send().await.map_err(|e|anyhow::Error::from(e.without_url()))?;
                let value: Value = response
                    .json()
                    .await
                    .map_err(|e| anyhow::Error::from(e.without_url()))?;
                let code = value["device_code"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad("迅雷暂时无法生成二维码，请稍后重试"))?;
                let wrapped = thunder_qr_url(&value)?;
                s.private = json!({"device_code":code,"device_id":device,"interval":value["interval"].as_u64().unwrap_or(2).clamp(2,30)});
                s.expires = now() + value["expires_in"].as_u64().unwrap_or(120).clamp(30, 300);
                s.public["qr_svg"] = qr_svg(wrapped.as_str())?.into();
                s.message = "请使用迅雷 App 扫码，并在手机上确认登录".into();
            }
            ("baidu", "qr") => {
                let (state, image) = crate::baidu::qr_start(&self.client, &mut s.cookies).await?;
                s.private = state;
                s.public["qr_image"] = image.into();
                s.expires = now() + 180;
                s.message = "请用百度网盘 App 扫码，并在手机上确认登录".into();
            }
            ("baidu", "cookie") => {
                s.message="粘贴你本人百度网盘的完整 Cookie（含 BDUSS、STOKEN）；一般使用扫码即可，无需填写".into();
            }
            ("baidu", "oauth") => {
                let (id, _, callback) =
                    oauth_config().map_err(|_| bad("请先配置百度应用 ID、密钥和 PUBLIC_URL"))?;
                let u = Url::parse_with_params(
                    "https://openapi.baidu.com/oauth/2.0/authorize",
                    [
                        ("client_id", id),
                        ("response_type", "code".into()),
                        ("scope", "basic,netdisk".into()),
                        ("redirect_uri", callback),
                        ("state", s.id.to_string()),
                    ],
                )
                .unwrap();
                s.public["authorize_url"] = u.to_string().into();
                s.message = "在百度官方授权页选择短信或扫码登录，然后允许访问网盘".into();
            }
            ("onedrive" | "google", "oauth") => {
                if let Ok((id, _, callback)) = crate::external::oauth_config(key) {
                    let (endpoint, scope) = crate::external::oauth_endpoints(key);
                    let verifier =
                        format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
                    use base64::Engine;
                    use sha2::{Digest, Sha256};
                    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(Sha256::digest(verifier.as_bytes()));
                    s.private["code_verifier"] = verifier.into();
                    let mut u = Url::parse_with_params(
                        endpoint,
                        [
                            ("client_id", id),
                            ("redirect_uri", callback),
                            ("response_type", "code".into()),
                            ("scope", scope.into()),
                            ("state", s.id.to_string()),
                            ("code_challenge", challenge),
                            ("code_challenge_method", "S256".into()),
                        ],
                    )
                    .unwrap();
                    if key == "google" {
                        u.query_pairs_mut()
                            .append_pair("access_type", "offline")
                            .append_pair("prompt", "consent");
                    }
                    s.public["authorize_url"] = u.to_string().into();
                    s.message = "在网盘官方页面登录并授权，完成后工作台会自动连接".into();
                } else {
                    s.public["setup_required"] = true.into();
                    s.public["helper_url"] = "https://api.oplist.org/".into();
                    s.message = "尚未配置自建 OAuth 应用。可用 OpenList 授权助手获取刷新令牌，再切换已有授权连接；或由部署者按 README 配置官方授权".into();
                }
            }
            (_, "credential") => {
                s.message = match key {
                    "baidu" => "粘贴百度刷新令牌即可连接；可通过 OpenList 授权助手获取",
                    "onedrive" | "google" => {
                        "粘贴 OpenList 授权助手的刷新令牌；也可以选择自建应用并填写对应客户端信息"
                    }
                    "thunder" => {
                        "粘贴迅雷刷新令牌；如令牌来自不同客户端，请一并填写客户端 ID 和密钥"
                    }
                    _ => "粘贴已有账号 Cookie 即可连接",
                }
                .into();
            }
            _ => return Err(bad("尚未接通此登录方式")),
        }
        let result = s.view();
        let mut all = self.sessions.lock().await;
        all.retain(|_, s| s.expires > now());
        if all.len() >= 16 {
            return Err(bad("登录会话过多，请关闭旧会话后重试"));
        }
        all.insert(s.id, s);
        Ok(result)
    }
    async fn thunder_http(&self, s: &Session, route: &str, mut body: Value) -> ApiResult<Value> {
        body["client_id"] = "Xp6vsxz_7IYVw2BB".into();
        let mut request = self
            .client
            .post(self.thunder_base.join(route).unwrap())
            .header("X-Device-Id", s.private["device_id"].as_str().unwrap_or(""));
        if let Some(token) = s.private["captcha_token"].as_str() {
            request = request.header("X-Captcha-Token", token);
        }
        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|_| bad("无法连接迅雷登录服务，请稍后重试"))?;
        let success = response.status().is_success();
        let value: Value = response
            .json()
            .await
            .map_err(|_| bad("迅雷登录响应无效，请稍后重试"))?;
        if !success && value["error"].as_str().unwrap_or("").is_empty() {
            return Err(bad("迅雷拒绝登录请求，请稍后重试"));
        }
        Ok(value)
    }
    async fn thunder_captcha(&self, s: &mut Session, action: &str) -> ApiResult<bool> {
        let value=self.thunder_http(s,"v1/shield/captcha/init",json!({"device_id":s.private["device_id"],"captcha_token":s.private["captcha_token"].as_str().unwrap_or(""),"action":action,"meta":{"phone_number":s.private["phone"]}})).await?;
        thunder_error(&value)?;
        if let Some(token) = value["captcha_token"].as_str() {
            s.private["captcha_token"] = token.into();
        }
        if let Some(url) = value["url"].as_str().filter(|v| !v.is_empty()) {
            let url = Url::parse(url).map_err(|_| bad("迅雷验证地址无效"))?;
            if url.scheme() != "https"
                || !url
                    .host_str()
                    .is_some_and(|h| h == "i.xunlei.com" || h == "captcha.xunlei.com")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(bad("迅雷需要官方安全验证，请改用 App 扫码"));
            }
            s.public["verify_url"] = url.as_str().into();
            s.status = "verification_required".into();
            s.message = "请先完成下方迅雷官方安全验证，再重试刚才的操作。".into();
            return Ok(false);
        }
        if s.private["captcha_token"].as_str().unwrap_or("").is_empty() {
            return Err(bad("迅雷未返回安全验证凭据，请稍后重试"));
        }
        s.public.as_object_mut().unwrap().remove("verify_url");
        Ok(true)
    }
    async fn thunder_sms(&self, s: &mut Session, body: &Value) -> ApiResult<()> {
        match body["action"].as_str().unwrap_or("") {
            "send" => {
                let phone = thunder_phone(body["phone"].as_str().unwrap_or(""))?;
                let mut sent = self.sms_sent.lock().await;
                sent.retain(|_, time| now() < *time + 60);
                if sent.contains_key(&phone) {
                    return Err(bad("验证码请求过于频繁，请 60 秒后再试"));
                }
                if sent.len() >= 1024 {
                    return Err(bad("短信请求过多，请稍后重试"));
                }
                s.private["phone"] = phone.clone().into();
                s.private
                    .as_object_mut()
                    .unwrap()
                    .remove("verification_token");
                if !self
                    .thunder_captcha(s, "POST:/v1/auth/verification")
                    .await?
                {
                    return Ok(());
                }
                sent.insert(phone.clone(), now());
                drop(sent);
                s.public["next_send_at"] = ((now() + 60) * 1000).into();
                let value = self
                    .thunder_http(
                        s,
                        "v1/auth/verification",
                        json!({"phone_number":phone,"target":"ANY","usage":"SIGN_IN"}),
                    )
                    .await?;
                thunder_error(&value)?;
                let id = value["verification_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad("迅雷未返回短信验证编号，请稍后重试"))?;
                s.private["verification_id"] = id.into();
                s.public["verification_sent"] = true.into();
                s.status = "code_sent".into();
                s.message = "验证码已发送，请填写短信验证码并登录。".into();
            }
            "verify" => {
                let code = body["code"].as_str().unwrap_or("");
                if !(4..=8).contains(&code.len()) || !code.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(bad("请输入 4 至 8 位短信验证码"));
                }
                if s.private["verification_id"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err(bad("请先获取验证码"));
                }
                if s.private["verification_token"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
                {
                    let value=self.thunder_http(s,"v1/auth/verification/verify",json!({"verification_id":s.private["verification_id"],"verification_code":code})).await?;
                    thunder_error(&value)?;
                    let token = value["verification_token"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| bad("迅雷未返回短信验证凭据，请重新获取验证码"))?;
                    s.private["verification_token"] = token.into();
                }
                if !self.thunder_captcha(s, "POST:/v1/auth/signin").await? {
                    return Ok(());
                }
                let value=self.thunder_http(s,"v1/auth/signin",json!({"username":s.private["phone"],"verification_code":code,"verification_token":s.private["verification_token"]})).await?;
                thunder_error(&value)?;
                let token = value["refresh_token"]
                    .as_str()
                    .filter(|s| s.len() >= 8)
                    .ok_or_else(|| bad("迅雷未返回网盘授权，请重新登录"))?;
                s.credential = Some(thunder_credential(token, &s.private["device_id"]));
                s.status = "ready".into();
                s.message = "短信验证成功，正在连接迅雷云盘。".into();
            }
            "challenge" => {
                let token = body["captcha_token"].as_str().unwrap_or("");
                if !s.public["verify_url"].is_string()
                    || !(16..=4096).contains(&token.len())
                    || token.contains(['\r', '\n'])
                {
                    return Err(bad("官方验证回执无效，请重新验证"));
                }
                s.private["captcha_token"] = token.into();
                s.public.as_object_mut().unwrap().remove("verify_url");
                s.status = "waiting".into();
                s.message = "已收到官方验证回执，请重试刚才的操作。".into();
            }
            _ => return Err(bad("请选择获取验证码或短信登录")),
        }
        Ok(())
    }
    async fn action(&self, s: &mut Session, body: &Value) -> ApiResult<()> {
        if s.status == "ready" && !["credential", "cookie"].contains(&s.mode.as_str()) {
            return Ok(());
        }
        if s.provider == "thunder" && s.mode == "sms" {
            let result = self.thunder_sms(s, body).await;
            if let Err(error) = &result {
                s.message = error.1.clone();
            }
            return result;
        } else if s.provider == "quark" && s.mode == "sms" {
            let ticket = body["ticket"].as_str().unwrap_or("");
            if ticket.len() != 32 || !ticket.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err(bad("官方登录回执无效，请重新登录"));
            }
            self.quark_ticket(s, ticket, "mobile").await?;
        } else if s.mode == "cookie" && s.provider == "baidu" {
            let raw = body["secret"].as_str().unwrap_or("");
            let map = crate::baidu::cookie_map(raw)?;
            s.credential = Some(
                json!({"driver":"BaiduNative","addition":{"cookie":crate::baidu::cookies(&map)}}),
            );
            s.status = "ready".into();
            s.message = "正在验证百度网盘连接".into();
        } else if s.mode == "credential" {
            let secret = body["secret"].as_str().unwrap_or("").trim();
            if secret.len() < 8 || secret.len() > 12000 || secret.contains(['\r', '\n']) {
                return Err(bad("授权信息格式不正确"));
            }
            s.credential = Some(match s.provider.as_str() {
                "quark" | "115" => addition(&s.provider, secret),
                "onedrive" | "google" => crate::external::credential(&s.provider, secret, body).map_err(|_| bad("请选择正确的授权来源；自建应用需要客户端 ID 和密钥，OneDrive 还需要原授权回调地址"))?,
                "baidu" => {
                    json!({"driver":"BaiduNetdisk","addition":{"root_folder_path":"/","refresh_token":secret,"use_online_api":true,"download_api":"official","custom_crack_ua":"netdisk"}})
                }
                "thunder" => {
                    let mut v = json!({"driver":"ThunderExpert","addition":{"root_folder_id":"","login_type":"refresh_token","refresh_token":secret,"sign_type":"algorithms","device_id":Uuid::new_v4().simple().to_string()}});
                    for key in ["client_id", "client_secret", "device_id"] {
                        if let Some(value) = body[key].as_str().filter(|s| !s.is_empty()) {
                            if value.len() > 500 || value.contains(['\r', '\n']) {
                                return Err(bad("客户端配置格式无效"));
                            }
                            v["addition"][key] = value.into();
                        }
                    }
                    v
                }
                _ => return Err(bad("不支持的账号")),
            });
            s.status = "ready".into();
            s.message = "正在验证账号连接".into();
        } else {
            return Err(bad("请按当前登录方式完成授权"));
        }
        Ok(())
    }
    async fn poll(&self, s: &mut Session) -> ApiResult<()> {
        if s.mode != "qr" || s.status == "ready" || now() < s.last_poll + 2 {
            return Ok(());
        }
        if s.provider == "thunder"
            && now() < s.last_poll + s.private["interval"].as_u64().unwrap_or(2)
        {
            return Ok(());
        }
        s.last_poll = now();
        if s.provider == "thunder" {
            let response = self
                .client
                .post("https://xluser-ssl.xunlei.com/v1/auth/token")
                .header("X-Device-Id", s.private["device_id"].as_str().unwrap_or(""))
                .json(&thunder_token_request(&s.private))
                .send()
                .await
                .map_err(|e| anyhow::Error::from(e.without_url()))?;
            let value: Value = response
                .json()
                .await
                .map_err(|e| anyhow::Error::from(e.without_url()))?;
            match value["error"].as_str().unwrap_or("") {
                "authorization_pending" => {}
                "slow_down" => {
                    s.private["interval"] = (s.private["interval"].as_u64().unwrap_or(2) + 2)
                        .min(30)
                        .into();
                }
                "expired_token" | "access_denied" => {
                    s.status = "expired".into();
                    s.message = "二维码已过期或被取消，请刷新重试".into();
                }
                "" => {
                    let token = value["refresh_token"]
                        .as_str()
                        .filter(|s| s.len() >= 8)
                        .ok_or_else(|| bad("迅雷未返回账号授权，请刷新二维码重试"))?;
                    s.credential = Some(thunder_credential(token, &s.private["device_id"]));
                    s.status = "ready".into();
                    s.message = "扫码成功，正在验证迅雷云盘连接".into();
                }
                _ => return Err(bad("迅雷扫码验证失败，请刷新重试")),
            }
        } else if s.provider == "baidu" {
            let (status, cookie) =
                crate::baidu::qr_poll(&self.client, &s.private, &mut s.cookies).await?;
            s.status = status;
            s.message = match s.status.as_str() {
                "ready" => "正在验证百度网盘连接",
                "scanned" => "已扫码，请在手机上确认登录",
                "expired" => "百度二维码已过期，请刷新",
                _ => "请用百度网盘 App 扫码确认登录",
            }
            .into();
            if let Some(cookie) = cookie {
                s.credential = Some(json!({"driver":"BaiduNative","addition":{"cookie":cookie}}));
            }
        } else if s.provider == "quark" {
            let url = Url::parse_with_params(
                "https://uop.quark.cn/cas/ajax/getServiceTicketByQrcodeToken",
                [
                    ("client_id", "532"),
                    ("v", "1.2"),
                    ("token", s.private["token"].as_str().unwrap_or("")),
                    ("request_id", &Uuid::new_v4().to_string()),
                ],
            )
            .unwrap();
            let v = self
                .http(url.as_str(), Method::GET, None, &mut s.cookies)
                .await?;
            if let Some(ticket) = v["data"]["members"]["service_ticket"].as_str() {
                self.quark_ticket(s, ticket, "scan").await?;
            } else {
                match v["status"].as_i64() {
                    Some(50004002) => {
                        s.expires = now();
                        s.status = "expired".into();
                        s.message = "二维码已过期，请刷新".into();
                    }
                    Some(50004004) => {
                        s.status = "cancelled".into();
                        s.message = "已取消登录，请重新开始".into();
                    }
                    Some(50004001) => {}
                    _ => {
                        s.message = "等待扫码确认；若二维码失效请刷新".into();
                    }
                }
            }
        } else if s.provider == "115" {
            let url = Url::parse_with_params(
                "https://qrcodeapi.115.com/get/status/",
                [
                    ("uid", s.private["uid"].as_str().unwrap_or("").to_owned()),
                    ("sign", s.private["sign"].as_str().unwrap_or("").into()),
                    ("time", s.private["time"].to_string()),
                    ("_", now().to_string()),
                ],
            )
            .unwrap();
            // 115 keeps this request open while no scan event is available.
            // Bound each long poll so closing or changing login remains responsive.
            let response = match self
                .client
                .get(url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) if error.is_timeout() => return Ok(()),
                Err(_) => return Err(bad("115 扫码状态暂时无法连接，请稍后重试")),
            };
            let v: Value = response
                .json()
                .await
                .map_err(|_| bad("115 扫码状态响应无效"))?;
            match v["data"]["status"].as_i64() {
                Some(1) => {
                    s.status = "scanned".into();
                    s.message = "已扫码，请在手机上确认".into();
                }
                Some(2) => {
                    let resp = self
                        .client
                        .post("https://passportapi.115.com/app/1.0/web/1.0/login/qrcode/")
                        .form(&[
                            ("account", s.private["uid"].as_str().unwrap_or("")),
                            ("app", "web"),
                        ])
                        .send()
                        .await
                        .map_err(|_| bad("115 登录交换失败，请重试"))?;
                    let v: Value = resp.json().await.map_err(|_| bad("115 登录响应无效"))?;
                    let map = v["data"]["cookie"]
                        .as_object()
                        .ok_or_else(|| bad("115 尚未返回授权 Cookie，请重新扫码"))?;
                    let cookie = map
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| format!("{k}={v}")))
                        .collect::<Vec<_>>()
                        .join("; ");
                    if !cookie.contains("SEID=") {
                        return Err(bad("115 授权不完整，请重新扫码"));
                    }
                    s.credential = Some(addition("115", &cookie));
                    s.status = "ready".into();
                    s.message = "正在验证网盘连接".into();
                }
                Some(-1) => {
                    s.expires = now();
                    s.status = "expired".into();
                    s.message = "二维码已过期，请刷新".into();
                }
                Some(-2) => {
                    s.status = "cancelled".into();
                    s.message = "已取消登录".into();
                }
                Some(0) => {}
                _ => return Err(bad("115 暂未返回扫码状态，请稍后重试")),
            }
        }
        Ok(())
    }
    async fn quark_ticket(&self, s: &mut Session, ticket: &str, kind: &str) -> ApiResult<()> {
        let url = Url::parse_with_params(
            "https://pan.quark.cn/account/info",
            [("st", ticket), ("lw", kind)],
        )
        .unwrap();
        self.http(url.as_str(), Method::GET, None, &mut s.cookies)
            .await?;
        let v = self
            .http(
                &format!("{DRIVE}/file/sort?pr=ucpro&fr=pc&pdir_fid=0&_page=1&_size=1"),
                Method::GET,
                None,
                &mut s.cookies,
            )
            .await?;
        if v["status"] != 200 || !s.cookies.contains_key("__pus") {
            return Err(bad("夸克授权未生效，请重新登录"));
        }
        s.credential = Some(addition("quark", &cookie_header(&s.cookies)));
        s.status = "ready".into();
        s.message = "正在验证网盘连接".into();
        Ok(())
    }
    async fn share(&self, key: &str, body: &Value) -> ApiResult<Value> {
        let url =
            Url::parse(body["url"].as_str().unwrap_or("")).map_err(|_| bad("分享地址无效"))?;
        let credential = self
            .credentials
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| bad("请先登录网盘"))?;
        let cookie = credential["cookie"].as_str().unwrap_or("");
        let pass = body["passcode"].as_str().unwrap_or("");
        let code = url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        if key == "115" {
            if cookie.is_empty() {
                return Err(bad("115 登录授权已失效，请重新扫码"));
            }
            return Ok(
                json!({"share_mount":{"driver":"115 Share","addition":{"cookie":cookie,"share_code":code,"receive_code":pass,"root_folder_id":"0","qrcode_source":"web","page_size":100,"limit_rate":1}}}),
            );
        }
        if key != "quark" {
            return Err(bad(
                "此网盘的分享转存接口尚未接通，请先在官方网盘保存分享，再从网盘文件中下载",
            ));
        }
        if cookie.is_empty() {
            return Err(bad("请先登录夸克网盘"));
        }
        let mut cookies = parse_cookie(cookie);
        let v = self
            .http(
                &format!("{DRIVE}/share/sharepage/token?pr=ucpro&fr=pc"),
                Method::POST,
                Some(json!({"pwd_id":code,"passcode":pass})),
                &mut cookies,
            )
            .await?;
        let token = v["data"]["stoken"]
            .as_str()
            .ok_or_else(|| bad("分享已失效或提取码错误"))?;
        let frag = url
            .fragment()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        let parent = if frag.len() == 32 && frag.bytes().all(|c| c.is_ascii_hexdigit()) {
            frag
        } else {
            "0"
        };
        let mut list = Vec::new();
        for page in 1..=50 {
            let u = Url::parse_with_params(
                &format!("{DRIVE}/share/sharepage/detail"),
                [
                    ("pr", "ucpro".into()),
                    ("fr", "pc".into()),
                    ("pwd_id", code.into()),
                    ("stoken", token.into()),
                    ("pdir_fid", parent.into()),
                    ("_page", page.to_string()),
                    ("_size", "100".into()),
                    ("_fetch_total", "1".into()),
                ],
            )
            .unwrap();
            let v = self
                .http(u.as_str(), Method::GET, None, &mut cookies)
                .await?;
            if v["status"] != 200 {
                return Err(bad("无法读取分享目录，请检查提取码和权限"));
            }
            let batch = v["data"]["list"]
                .as_array()
                .ok_or_else(|| bad("分享目录响应无效"))?;
            list.extend(batch.clone());
            if batch.len() < 100 {
                break;
            }
            if page == 50 {
                return Err(bad("分享目录过大，请使用子目录链接"));
            }
        }
        if list.is_empty() {
            return Err(bad("分享目录为空"));
        }
        if body["inspect_only"] == true {
            let mut pending = list;
            let mut seen = std::collections::BTreeSet::new();
            let mut bytes = 0u64;
            let mut count = 0;
            while let Some(file) = pending.pop() {
                let fid = file["fid"]
                    .as_str()
                    .ok_or_else(|| bad("夸克分享文件标识缺失"))?;
                if !seen.insert(fid.to_owned()) || seen.len() > 5000 {
                    return Err(bad("分享超过 5000 项或目录重复，请拆分分享"));
                }
                if file["dir"] == true {
                    for page in 1..=50 {
                        let u = Url::parse_with_params(
                            &format!("{DRIVE}/share/sharepage/detail"),
                            [
                                ("pr", "ucpro".into()),
                                ("fr", "pc".into()),
                                ("pwd_id", code.into()),
                                ("stoken", token.into()),
                                ("pdir_fid", fid.into()),
                                ("_page", page.to_string()),
                                ("_size", "100".into()),
                            ],
                        )
                        .unwrap();
                        let v = self
                            .http(u.as_str(), Method::GET, None, &mut cookies)
                            .await?;
                        if v["status"] != 200 {
                            return Err(bad("夸克分享子目录读取失败"));
                        }
                        let rows = v["data"]["list"]
                            .as_array()
                            .ok_or_else(|| bad("夸克分享目录缺失"))?;
                        pending.extend(rows.clone());
                        if rows.len() < 100 {
                            break;
                        }
                        if page == 50 {
                            return Err(bad("分享目录过大，请拆分分享"));
                        }
                    }
                } else {
                    bytes = bytes
                        .checked_add(
                            file["size"]
                                .as_u64()
                                .or(file["file_size"].as_u64())
                                .ok_or_else(|| bad("夸克分享文件大小缺失"))?,
                        )
                        .ok_or_else(|| bad("分享大小超出范围"))?;
                    count += 1;
                }
            }
            let usage=self.http(&format!("{DRIVE}/member?pr=ucpro&fr=pc&fetch_subscribe=false&_ch=home&fetch_identity=false"),Method::GET,None,&mut cookies).await.unwrap_or(Value::Null);
            return Ok(
                json!({"required_bytes":bytes,"file_count":count,"total_space":usage["data"]["total_capacity"],"used_space":usage["data"]["use_capacity"]}),
            );
        }
        let id = body["id"].as_str().ok_or_else(|| bad("分享任务无效"))?;
        let id = Uuid::parse_str(id).map_err(|_| bad("分享任务无效"))?;
        let name = format!("分享-{id}");
        let root_id = credential["root_folder_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("0");
        let root = self
            .quark_folder(root_id, "FlowDisk分享", &mut cookies)
            .await?;
        let (target, existed) = self.quark_folder_status(&root, &name, &mut cookies).await?;
        // A per-request folder makes retries safe after a crash. Existing content is reused.
        let existing = self.quark_list(&target, &mut cookies).await?;
        if existed && !existing.is_empty() {
            let complete = list.iter().all(|source| {
                existing
                    .iter()
                    .any(|saved| saved["file_name"] == source["file_name"])
            });
            if !complete {
                return Err(bad(
                    "上次转存尚未完整完成，请在网盘检查接收目录，清空该目录后重试",
                ));
            }
        }
        if !existed || existing.is_empty() {
            let v=self.http(&format!("{DRIVE}/share/sharepage/save?pr=ucpro&fr=pc"),Method::POST,Some(json!({"fid_list":list.iter().map(|v|v["fid"].clone()).collect::<Vec<_>>(),"fid_token_list":list.iter().map(|v|v["share_fid_token"].clone()).collect::<Vec<_>>(),"to_pdir_fid":target,"pwd_id":code,"stoken":token,"pdir_fid":parent,"scene":"link"})),&mut cookies).await?;
            if v["status"] != 200 {
                return Err(bad(
                    "转存失败，请检查空间、分享权限或在官方网盘完成安全验证后重试",
                ));
            }
            if let Some(task) = v["data"]["task_id"].as_str() {
                let mut done = false;
                for i in 0..60 {
                    let u = Url::parse_with_params(
                        &format!("{DRIVE}/task"),
                        [
                            ("pr", "ucpro".into()),
                            ("fr", "pc".into()),
                            ("task_id", task.to_owned()),
                            ("retry_index", i.to_string()),
                        ],
                    )
                    .unwrap();
                    let v = self
                        .http(u.as_str(), Method::GET, None, &mut cookies)
                        .await?;
                    match v["data"]["status"].as_i64() {
                        Some(2) => {
                            done = true;
                            break;
                        }
                        Some(3) => return Err(bad("网盘转存任务失败")),
                        _ => tokio::time::sleep(Duration::from_millis(1500)).await,
                    }
                }
                if !done {
                    return Err(bad("转存等待超时，请在官方网盘检查后重试"));
                }
            }
        }
        Ok(json!({"paths":[format!("/FlowDisk分享/{name}")]}))
    }
    async fn quark_list(
        &self,
        parent: &str,
        cookies: &mut BTreeMap<String, String>,
    ) -> ApiResult<Vec<Value>> {
        let mut all = Vec::new();
        for page in 1..=100 {
            let u = Url::parse_with_params(
                &format!("{DRIVE}/file/sort"),
                [
                    ("pr", "ucpro".into()),
                    ("fr", "pc".into()),
                    ("pdir_fid", parent.to_owned()),
                    ("_page", page.to_string()),
                    ("_size", "100".into()),
                ],
            )
            .unwrap();
            let v = self.http(u.as_str(), Method::GET, None, cookies).await?;
            if v["status"] != 200 {
                return Err(bad("无法读取夸克目录，请重新登录或完成官方验证"));
            }
            let list = v["data"]["list"]
                .as_array()
                .ok_or_else(|| bad("夸克目录响应无效"))?;
            all.extend(list.clone());
            if list.len() < 100 {
                return Ok(all);
            }
        }
        Err(bad("目录项目过多，请选择较小的目录"))
    }
    async fn quark_folder(
        &self,
        parent: &str,
        name: &str,
        cookies: &mut BTreeMap<String, String>,
    ) -> ApiResult<String> {
        Ok(self.quark_folder_status(parent, name, cookies).await?.0)
    }
    async fn quark_folder_status(
        &self,
        parent: &str,
        name: &str,
        cookies: &mut BTreeMap<String, String>,
    ) -> ApiResult<(String, bool)> {
        if let Some(f) = self
            .quark_list(parent, cookies)
            .await?
            .iter()
            .find(|f| f["dir"] == true && f["file_name"] == name)
        {
            return Ok((
                f["fid"]
                    .as_str()
                    .ok_or_else(|| bad("夸克目录 ID 无效"))?
                    .into(),
                true,
            ));
        }
        let v = self
            .http(
                &format!("{DRIVE}/file?pr=ucpro&fr=pc"),
                Method::POST,
                Some(
                    json!({"pdir_fid":parent,"file_name":name,"dir_init_lock":false,"dir_path":""}),
                ),
                cookies,
            )
            .await?;
        Ok((
            v["data"]["fid"]
                .as_str()
                .ok_or_else(|| bad("无法创建分享接收目录"))?
                .into(),
            false,
        ))
    }
}
#[derive(Deserialize)]
pub struct Callback {
    state: Uuid,
    code: Option<String>,
    error: Option<String>,
}
pub async fn baidu_callback(State(app): State<App>, Query(q): Query<Callback>) -> Response {
    async fn complete(app: &App, q: Callback) -> ApiResult<()> {
        let mut all = app.0.control.auth_sessions().await;
        let s = all
            .get_mut(&q.state)
            .ok_or_else(|| bad("授权会话不存在或已经结束"))?;
        if s.provider != "baidu" || s.mode != "oauth" || s.status != "waiting" || s.expires <= now()
        {
            return Err(bad("授权会话已过期，请返回工作台重试"));
        }
        if q.error.is_some() {
            s.status = "cancelled".into();
            return Err(bad("已取消百度授权"));
        }
        let code = q.code.ok_or_else(|| bad("缺少授权回执"))?;
        let (id, secret, callback) = oauth_config().map_err(|_| bad("百度 OAuth 配置不完整"))?;
        let resp = app
            .0
            .control
            .auth_client()
            .post("https://openapi.baidu.com/oauth/2.0/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &id),
                ("client_secret", &secret),
                ("redirect_uri", &callback),
                ("code", &code),
            ])
            .send()
            .await
            .map_err(|_| bad("百度授权交换失败，请重试"))?;
        let v: Value = resp.json().await.map_err(|_| bad("百度授权响应无效"))?;
        let refresh = v["refresh_token"]
            .as_str()
            .ok_or_else(|| bad("未取得百度网盘授权，请确认应用已开通网盘权限"))?;
        s.credential = Some(
            json!({"driver":"BaiduNetdisk","addition":{"root_folder_path":"/","refresh_token":refresh,"client_id":id,"client_secret":secret,"use_online_api":false,"download_api":"official","custom_crack_ua":"netdisk"}}),
        );
        s.status = "ready".into();
        s.message = "授权成功，等待网盘连接".into();
        Ok(())
    }
    match complete(&app,q).await {Ok(())=>Html("<!doctype html><meta charset=utf-8><title>授权完成</title><p>百度授权已完成，可以关闭此页并返回工作台。</p><a href=/ >返回工作台</a>").into_response(),Err(e)=>e.into_response()}
}
pub async fn external_callback(
    State(app): State<App>,
    axum::extract::Path(provider): axum::extract::Path<String>,
    Query(q): Query<Callback>,
) -> Response {
    async fn complete(app: &App, provider: &str, q: Callback) -> ApiResult<()> {
        if !["onedrive", "google"].contains(&provider) {
            return Err(bad("不支持的授权回调"));
        }
        let mut all = app.0.control.auth_sessions().await;
        let s = all
            .get_mut(&q.state)
            .ok_or_else(|| bad("授权会话不存在或已经结束"))?;
        if s.provider != provider
            || s.mode != "oauth"
            || s.status != "waiting"
            || s.expires <= now()
        {
            return Err(bad("授权会话已过期或网盘不匹配，请返回工作台重试"));
        }
        if q.error.is_some() {
            s.status = "cancelled".into();
            return Err(bad("已取消授权"));
        }
        let code = q.code.ok_or_else(|| bad("缺少授权回执"))?;
        let (id, secret, callback) =
            crate::external::oauth_config(provider).map_err(|_| bad("OAuth 配置不完整"))?;
        let token_url = crate::external::token_endpoint(provider);
        let response = app
            .0
            .control
            .auth_client()
            .post(token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &id),
                ("client_secret", &secret),
                ("redirect_uri", &callback),
                ("code", &code),
                (
                    "code_verifier",
                    s.private["code_verifier"].as_str().unwrap_or(""),
                ),
            ])
            .send()
            .await
            .map_err(|_| bad("官方授权交换失败，请检查服务器网络后重新授权"))?;
        let success = response.status().is_success();
        let v: Value = response.json().await.map_err(|_| bad("官方授权响应无效"))?;
        let refresh = v["refresh_token"]
            .as_str()
            .filter(|v| success && !v.is_empty())
            .ok_or_else(|| bad("未取得刷新令牌，请检查应用权限并重新同意授权"))?;
        s.credential = Some(crate::external::credential(provider, refresh, &json!({"source":"custom","client_id":id,"client_secret":secret,"redirect_uri":callback})).map_err(|_| bad("授权配置无效"))?);
        s.status = "ready".into();
        s.message = "授权成功，等待网盘连接".into();
        Ok(())
    }
    match complete(&app, &provider, q).await {
        Ok(()) => Html("<!doctype html><meta charset=utf-8><title>授权完成</title><p>授权已完成，可以关闭此页并返回工作台。</p><a href=/>返回工作台</a>").into_response(),
        Err(e) => e.into_response(),
    }
}
// Callback access remains server-only; neither secrets nor cookies leave the backend.
impl crate::control::Control {
    async fn auth_sessions(&self) -> tokio::sync::MutexGuard<'_, BTreeMap<Uuid, Session>> {
        self.native_auth().sessions.lock().await
    }
    fn auth_client(&self) -> &reqwest::Client {
        &self.native_auth().client
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_and_cookie_roundtrip() {
        assert_eq!(methods("quark")[0], "sms");
        assert!(!methods("115").contains(&"sms"));
        assert!(methods("thunder").contains(&"qr"));
        let request = thunder_token_request(&json!({"device_code":"private-device-ticket"}));
        assert!(request.get("client_secret").is_none());
        assert_eq!(request["device_code"], "private-device-ticket");
        let c = parse_cookie("__pus=abc==; __puus=xyz");
        assert_eq!(parse_cookie(&cookie_header(&c)), c);
        assert!(qr_svg("test").unwrap().contains("<svg"));
    }
    #[tokio::test]
    async fn native_session_redacts_and_expires() {
        let auth = Auth::new().unwrap();
        let s = auth
            .request(
                "/sessions",
                Method::POST,
                json!({"provider":"quark","mode":"sms"}),
            )
            .await
            .unwrap_or_else(|_| panic!("SMS form should not send network requests"));
        let id = Uuid::parse_str(s["id"].as_str().unwrap()).unwrap();
        {
            let mut all = auth.sessions.lock().await;
            let s = all.get_mut(&id).unwrap();
            s.private = json!({"token":"private-ticket"});
            s.credential = Some(json!({"cookie":"private-cookie"}));
            assert!(!s.view().to_string().contains("private-"));
            s.expires = now();
        }
        let s = auth
            .request(&format!("/sessions/{id}"), Method::GET, json!({}))
            .await
            .unwrap_or_else(|_| panic!("expired status"));
        assert_eq!(s["status"], "expired");
        assert!(auth.sessions.lock().await[&id].credential.is_none());
        assert!(
            auth.request(
                "/sessions",
                Method::POST,
                json!({"provider":"115","mode":"sms"})
            )
            .await
            .is_err()
        );
    }
}

#[cfg(test)]
mod thunder_tests {
    use super::*;
    use axum::{
        Json, Router,
        body::to_bytes,
        extract::{Request, State},
        routing::any,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Default)]
    struct Fixture {
        sent: AtomicUsize,
        challenge: AtomicUsize,
    }
    async fn endpoint(State(state): State<Arc<Fixture>>, request: Request) -> Json<Value> {
        let path = request.uri().path().to_string();
        assert!(!request.headers()["X-Device-Id"].is_empty());
        let bytes = to_bytes(request.into_body(), 16384).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["client_id"], "Xp6vsxz_7IYVw2BB");
        assert!(value.get("client_secret").is_none());
        Json(match path.as_str() {
            "/v1/shield/captcha/init" => {
                if state.challenge.load(Ordering::Relaxed) == 1 {
                    json!({"captcha_token":"unit-captcha-token","url":"https://i.xunlei.com/unit-captcha"})
                } else {
                    json!({"captcha_token":"unit-captcha-token"})
                }
            }
            "/v1/auth/verification" => {
                state.sent.fetch_add(1, Ordering::Relaxed);
                assert_eq!(value["usage"], "SIGN_IN");
                assert_eq!(value["target"], "ANY");
                json!({"verification_id":"unit-verification-id","is_user":true})
            }
            "/v1/auth/verification/verify" => {
                if value["verification_code"] == "123456" {
                    json!({"verification_token":"unit-verification-token"})
                } else {
                    json!({"error":"invalid_verification_code","error_code":4014})
                }
            }
            "/v1/auth/signin" => {
                assert_eq!(value["verification_token"], "unit-verification-token");
                assert!(value["username"].as_str().unwrap().starts_with("+86"));
                json!({"refresh_token":"unit-refresh-token-private"})
            }
            _ => panic!("Unexpected Thunder test endpoint"),
        })
    }
    #[test]
    fn official_qr_page_and_phone_validation() {
        let url=thunder_qr_url(&json!({"user_code":"test-user-code","verification_uri_complete":"https://xluser-ssl.xunlei.com/__/auth/device/"})).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().collect();
        let page = Url::parse(&query["redirect_uri"]).unwrap();
        assert_eq!(page.host_str(), Some("i.xunlei.com"));
        assert_eq!(page.path(), "/center/account/personal/qrcode-login/");
        assert_eq!(
            page.query_pairs()
                .find(|(k, _)| k == "user_code")
                .unwrap()
                .1,
            "test-user-code"
        );
        assert!(thunder_qr_url(&json!({})).is_err());
        for phone in ["", "00000000000", "1380000000", "13800000000\nCookie: x"] {
            assert!(thunder_phone(phone).is_err());
        }
        assert_eq!(thunder_phone("13800000000").unwrap(), "+8613800000000");
    }
    #[tokio::test]
    async fn sms_protocol_cooldown_challenge_and_secret_redaction() {
        let state = Arc::new(Fixture::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(endpoint))
                    .with_state(state.clone()),
            )
            .into_future(),
        );
        let mut auth = Auth::new().unwrap();
        auth.thunder_base = Url::parse(&format!("http://{addr}/")).unwrap();
        let start = auth
            .request(
                "/sessions",
                Method::POST,
                json!({"provider":"thunder","mode":"sms"}),
            )
            .await
            .unwrap();
        assert_eq!(state.sent.load(Ordering::Relaxed), 0);
        let id = start["id"].as_str().unwrap();
        let route = format!("/sessions/{id}/action");
        assert!(
            auth.request(
                &route,
                Method::POST,
                json!({"action":"verify","code":"123456"})
            )
            .await
            .is_err()
        );
        let sent = auth
            .request(
                &route,
                Method::POST,
                json!({"action":"send","phone":"13800000000"}),
            )
            .await
            .unwrap();
        assert_eq!(sent["status"], "code_sent");
        assert_eq!(state.sent.load(Ordering::Relaxed), 1);
        assert!(!sent.to_string().contains("13800000000"));
        assert!(!sent.to_string().contains("unit-captcha"));
        assert!(
            auth.request(
                &route,
                Method::POST,
                json!({"action":"send","phone":"13800000000"})
            )
            .await
            .is_err()
        );
        assert_eq!(state.sent.load(Ordering::Relaxed), 1);
        assert!(
            auth.request(
                &route,
                Method::POST,
                json!({"action":"verify","code":"000000"})
            )
            .await
            .is_err()
        );
        let ready = auth
            .request(
                &route,
                Method::POST,
                json!({"action":"verify","code":"123456"}),
            )
            .await
            .unwrap();
        assert_eq!(ready["status"], "ready");
        assert!(!ready.to_string().contains("unit-refresh-token"));
        let credential = auth
            .request(
                &format!("/sessions/{id}/credentials"),
                Method::GET,
                json!({}),
            )
            .await
            .unwrap();
        assert_eq!(credential["driver"], "ThunderExpert");
        assert_eq!(
            credential["addition"]["refresh_token"],
            "unit-refresh-token-private"
        );
        let next = auth
            .request(
                "/sessions",
                Method::POST,
                json!({"provider":"thunder","mode":"sms"}),
            )
            .await
            .unwrap();
        let route = format!("/sessions/{}/action", next["id"].as_str().unwrap());
        assert!(
            auth.request(
                &route,
                Method::POST,
                json!({"action":"send","phone":"13800000000"})
            )
            .await
            .is_err()
        );
        state.challenge.store(1, Ordering::Relaxed);
        let challenge = auth
            .request(
                &route,
                Method::POST,
                json!({"action":"send","phone":"13900000000"}),
            )
            .await
            .unwrap();
        assert_eq!(challenge["status"], "verification_required");
        assert_eq!(state.sent.load(Ordering::Relaxed), 1);
        auth.request(
            &route,
            Method::POST,
            json!({"action":"challenge","captcha_token":"unit-official-challenge-receipt"}),
        )
        .await
        .unwrap();
        state.challenge.store(0, Ordering::Relaxed);
        let sent = auth
            .request(
                &route,
                Method::POST,
                json!({"action":"send","phone":"13900000000"}),
            )
            .await
            .unwrap();
        assert_eq!(sent["status"], "code_sent");
        assert_eq!(state.sent.load(Ordering::Relaxed), 2);
        auth.request(
            &format!("/sessions/{}", next["id"].as_str().unwrap()),
            Method::DELETE,
            json!({}),
        )
        .await
        .unwrap();
        assert!(
            auth.request(
                &route,
                Method::POST,
                json!({"action":"verify","code":"123456"})
            )
            .await
            .is_err()
        );
        server.abort();
    }
}
