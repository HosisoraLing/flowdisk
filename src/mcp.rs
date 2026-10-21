//! Stateless Streamable HTTP MCP. Tools reuse the authenticated application API;
//! no arbitrary URL, filesystem or shell execution is exposed.
use crate::{App, same_secret};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
    destructive: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only,"destructiveHint":destructive,"openWorldHint":!read_only}})
}
fn tools() -> Vec<Value> {
    vec![
        tool(
            "flowdisk_status",
            "查看服务、账号和 SMB 连接状态",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "flowdisk_browse",
            "浏览网盘或 SMB 目录，每页 100 项",
            json!({"path":{"type":"string"},"page":{"type":"integer","minimum":1}}),
            &["path"],
            true,
            false,
        ),
        tool(
            "flowdisk_list_downloads",
            "列出网盘、SMB、普通链接和 BT 下载任务",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "flowdisk_download_link",
            "添加一个普通下载链接或磁力。种子内容等待用户明确选择；不能绕过账号权限",
            json!({"uri":{"type":"string"},"group_name":{"type":"string"}}),
            &["uri"],
            false,
            false,
        ),
        tool(
            "flowdisk_parse_share",
            "解析网盘分享，先浏览内容，不会创建下载任务",
            json!({"url":{"type":"string"},"passcode":{"type":"string"}}),
            &["url"],
            false,
            false,
        ),
        tool(
            "flowdisk_share_contents",
            "读取分享目录及转存容量估算",
            json!({"id":{"type":"string"}}),
            &["id"],
            true,
            false,
        ),
        tool(
            "flowdisk_prepare_share",
            "在用户确认后转存分享以浏览。必须先展示容量并取得明确同意",
            json!({"id":{"type":"string"},"confirm_transfer":{"type":"boolean"},"allow_unknown_capacity":{"type":"boolean"}}),
            &["id", "confirm_transfer"],
            false,
            false,
        ),
        tool(
            "flowdisk_download_share",
            "下载用户选中的分享文件；若含疑似广告需用户明确保留",
            json!({"id":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"maxItems":5000},"allow_ads":{"type":"boolean"}}),
            &["id", "paths"],
            false,
            false,
        ),
        tool(
            "flowdisk_download_paths",
            "批量下载已存网盘或 SMB 文件/目录",
            json!({"paths":{"type":"array","items":{"type":"string"},"maxItems":1000}}),
            &["paths"],
            false,
            false,
        ),
        tool(
            "flowdisk_select_torrent",
            "明确选择种子文件编号并开始下载",
            json!({"id":{"type":"string"},"indices":{"type":"array","items":{"type":"integer","minimum":1},"maxItems":5000}}),
            &["id", "indices"],
            false,
            false,
        ),
        tool(
            "flowdisk_task_control",
            "暂停或恢复任务。暂停 seeding 会停止做种上传",
            json!({"id":{"type":"string"},"engine":{"type":"string","enum":["native","aria2"]},"action":{"type":"string","enum":["pause","resume"]}}),
            &["id", "engine", "action"],
            false,
            false,
        ),
        tool(
            "flowdisk_statistics",
            "获取流量、速率、每日统计、任务分布及磁盘空间",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "flowdisk_settings",
            "查看或修改总下载限速、做种上传限速和停止策略。速度单位 bytes/s，0 为不限速",
            json!({"download_limit":{"type":"integer","minimum":0},"upload_limit":{"type":"integer","minimum":0},"seed_ratio":{"type":"number","minimum":0},"seed_minutes":{"type":"integer","minimum":0}}),
            &[],
            false,
            false,
        ),
    ]
}
fn id(args: &Value) -> Result<String, String> {
    let raw = args["id"].as_str().ok_or("缺少任务或分享 ID")?;
    Uuid::parse_str(raw)
        .map(|v| v.to_string())
        .map_err(|_| "ID 格式无效".into())
}
async fn api(app: &App, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = bind
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("服务端口配置无效")?;
    let host = if bind.starts_with("[::1]") {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    let base = format!("http://{host}:{port}/api{path}");
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|_| "无法连接应用 API")?;
    let mut request = client.request(method, &base).bearer_auth(&app.0.token);
    if let Some(body) = body {
        request = request.json(&body)
    }
    let response = request.send().await.map_err(|_| "应用 API 暂时无法连接")?;
    let success = response.status().is_success();
    let value: Value = response.json().await.map_err(|_| "应用 API 返回无效结果")?;
    if !success {
        return Err(value["error"]
            .as_str()
            .unwrap_or("操作失败")
            .chars()
            .take(2000)
            .collect());
    }
    Ok(value)
}
async fn call(app: &App, name: &str, args: Value) -> Result<Value, String> {
    let definition = tools()
        .into_iter()
        .find(|t| t["name"] == name)
        .ok_or("未知工具")?;
    let map = args.as_object().ok_or("arguments 必须是对象")?;
    for key in map.keys() {
        if definition["inputSchema"]["properties"].get(key).is_none() {
            return Err(format!("未知参数 {key}"));
        }
    }
    for required in definition["inputSchema"]["required"].as_array().unwrap() {
        if args.get(required.as_str().unwrap()).is_none() {
            return Err(format!("缺少参数 {required}"));
        }
    }
    let (method, path, body) = match name {
        "flowdisk_status" => {
            return Ok(
                json!({"status":api(app,Method::GET,"/status",None).await?,"accounts":api(app,Method::GET,"/accounts",None).await?,"connections":api(app,Method::GET,"/connections",None).await?}),
            );
        }
        "flowdisk_browse" => {
            let path = args["path"]
                .as_str()
                .filter(|p| crate::valid_path(p))
                .ok_or("目录路径无效")?;
            let mut url = Url::parse("http://local/browse").unwrap();
            url.query_pairs_mut()
                .append_pair("path", path)
                .append_pair("page", &args["page"].as_u64().unwrap_or(1).to_string());
            (
                Method::GET,
                format!("/browse?{}", url.query().unwrap()),
                None,
            )
        }
        "flowdisk_list_downloads" => {
            return Ok(
                json!({"native":api(app,Method::GET,"/jobs",None).await?,"links_bt":api(app,Method::GET,"/transfers",None).await?}),
            );
        }
        "flowdisk_download_link" => (Method::POST, "/transfers".into(), Some(args)),
        "flowdisk_parse_share" => (Method::POST, "/shares".into(), Some(args)),
        "flowdisk_share_contents" => (
            Method::GET,
            format!("/shares/{}/contents", id(&args)?),
            None,
        ),
        "flowdisk_prepare_share" => {
            if args["confirm_transfer"] != true {
                return Err("需要先展示网盘容量并得到用户确认".into());
            }
            (
                Method::POST,
                format!("/shares/{}/prepare", id(&args)?),
                Some(json!({"allow_unknown_capacity":args["allow_unknown_capacity"]==true})),
            )
        }
        "flowdisk_download_share" => (
            Method::POST,
            format!("/shares/{}/download", id(&args)?),
            Some(json!({"paths":args["paths"],"allow_ads":args["allow_ads"]==true})),
        ),
        "flowdisk_download_paths" => (Method::POST, "/batch-download".into(), Some(args)),
        "flowdisk_select_torrent" => (
            Method::POST,
            format!("/transfers/{}/select", id(&args)?),
            Some(json!({"indices":args["indices"]})),
        ),
        "flowdisk_task_control" => {
            let action = args["action"]
                .as_str()
                .filter(|s| ["pause", "resume"].contains(s))
                .ok_or("操作无效")?;
            let engine = match args["engine"].as_str() {
                Some("native") => "jobs",
                Some("aria2") => "transfers",
                _ => return Err("下载引擎无效".into()),
            };
            (
                Method::POST,
                format!("/{engine}/{}/{action}", id(&args)?),
                Some(json!({})),
            )
        }
        "flowdisk_statistics" => (Method::GET, "/statistics".into(), None),
        "flowdisk_settings" => {
            if map.is_empty() {
                (Method::GET, "/settings".into(), None)
            } else {
                let mut settings = api(app, Method::GET, "/settings", None).await?;
                for (k, v) in map {
                    settings[k] = v.clone();
                }
                (Method::PUT, "/settings".into(), Some(settings))
            }
        }
        _ => return Err("未知工具".into()),
    };
    api(app, method, &path, body).await
}
async fn handle(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !same_secret(token.as_bytes(), app.0.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"FlowDisk MCP\"")],
            Json(json!({"error":"请使用 APP_TOKEN 进行 Bearer 鉴权"})),
        )
            .into_response();
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let valid = origin
            .to_str()
            .ok()
            .and_then(|v| Url::parse(v).ok())
            .is_some_and(|url| {
                let host = headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let same =
                    url.as_str().trim_end_matches('/') == format!("{}://{host}", url.scheme());
                let public = std::env::var("PUBLIC_URL")
                    .ok()
                    .and_then(|v| Url::parse(&v).ok())
                    .is_some_and(|p| p.origin() == url.origin());
                same || public
            });
        if !valid {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"不允许此 Origin"})),
            )
                .into_response();
        }
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str().unwrap_or("");
    let error = |code: i64, message: &str| {
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
            .into_response()
    };
    if request["jsonrpc"] != "2.0" || method.is_empty() || request.is_array() {
        return error(-32600, "Invalid Request");
    }
    if request.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = match method {
        "initialize" => {
            let proposed = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let supported = ["2025-03-26", "2025-06-18", "2025-11-25"];
            json!({"protocolVersion":if supported.contains(&proposed){proposed}else{"2025-11-25"},"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"flowdisk","version":env!("CARGO_PKG_VERSION")},"instructions":"管理本人已授权的下载来源。转存前展示容量并取得用户确认；种子和分享按用户明确选择的范围下载。"})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            let args = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            match call(&app, name, args).await {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value).unwrap()}],"structuredContent":{"result":value},"isError":false})
                }
                Err(message) => json!({"content":[{"type":"text","text":message}],"isError":true}),
            }
        }
        _ => return error(-32601, "Method not found"),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/mcp", post(handle))
        .layer(DefaultBodyLimit::max(256 * 1024))
}
