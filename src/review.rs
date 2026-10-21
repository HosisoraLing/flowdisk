use crate::{ApiResult, App, bad, control, valid_path};
use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    #[serde(default)]
    pub ad_reason: Option<String>,
    #[serde(default)]
    pub passwords: Vec<String>,
}
pub fn inside(root: &str, path: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}
pub fn media(name: &str) -> (&'static str, &'static str) {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "jpg" | "jpeg" => ("image", "image/jpeg"),
        "png" => ("image", "image/png"),
        "gif" => ("image", "image/gif"),
        "webp" => ("image", "image/webp"),
        "avif" => ("image", "image/avif"),
        "bmp" => ("image", "image/bmp"),
        "mp3" => ("audio", "audio/mpeg"),
        "wav" => ("audio", "audio/wav"),
        "flac" => ("audio", "audio/flac"),
        "ogg" | "oga" => ("audio", "audio/ogg"),
        "m4a" => ("audio", "audio/mp4"),
        "aac" => ("audio", "audio/aac"),
        "mp4" | "m4v" => ("video", "video/mp4"),
        "webm" => ("video", "video/webm"),
        "mov" => ("video", "video/quicktime"),
        "mkv" => ("video", "video/x-matroska"),
        "txt" | "md" | "markdown" | "json" | "csv" | "log" | "srt" | "ass" | "lrc" | "ini"
        | "toml" | "yaml" | "yml" | "xml" | "html" | "htm" | "svg" | "nfo" | "url" => {
            ("text", "text/plain; charset=utf-8")
        }
        _ => ("other", "application/octet-stream"),
    }
}
fn archive(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".001",
    ]
    .iter()
    .any(|s| n.ends_with(s))
}
fn password(name: &str) -> Option<String> {
    let (_, tail) = ["解压密码", "压缩密码", "解压码"]
        .iter()
        .find_map(|key| name.split_once(*key))?;
    let tail = tail.trim_start_matches(|c: char| c.is_whitespace() || "：:=是为【[（(".contains(c));
    let tail = [".txt", ".TXT", ".nfo"]
        .iter()
        .find_map(|ext| tail.strip_suffix(ext))
        .unwrap_or(tail);
    let value = tail.trim_end_matches(|c: char| c.is_whitespace() || "】]）)".contains(c));
    (!value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}
fn ad(name: &str, is_dir: bool, size: u64) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if [
        "广告",
        "推广",
        "加群",
        "关注公众号",
        "扫码关注",
        "备用网址",
        "福利网址",
        "赞助链接",
    ]
    .iter()
    .any(|word| name.contains(word))
    {
        Some("名称包含推广或引流用语".into())
    } else if !is_dir
        && size <= 32 * 1024
        && [".url", ".website", ".lnk"]
            .iter()
            .any(|ext| lower.ends_with(ext))
    {
        Some("小体积网址或快捷方式，可能用于引流".into())
    } else {
        None
    }
}
pub fn annotate(entries: &mut [Entry]) {
    let ads: Vec<_> = entries
        .iter()
        .filter_map(|e| ad(&e.name, e.is_dir, e.size).map(|reason| (e.path.clone(), reason)))
        .collect();
    let hints: Vec<_> = entries
        .iter()
        .filter(|e| e.is_dir || e.size == 0)
        .filter_map(|e| {
            let value = password(&e.name)?;
            let scope = if e.is_dir
                && entries
                    .iter()
                    .any(|a| !a.is_dir && archive(&a.name) && inside(&e.path, &a.path))
            {
                e.path.clone()
            } else {
                e.path.rsplit_once('/')?.0.to_owned()
            };
            Some((scope, value))
        })
        .collect();
    for entry in entries {
        entry.ad_reason = ads
            .iter()
            .filter(|(scope, _)| inside(scope, &entry.path))
            .max_by_key(|(scope, _)| scope.len())
            .map(|(_, reason)| reason.clone());
        entry.passwords.clear();
        if !entry.is_dir && archive(&entry.name) {
            let nearest = hints
                .iter()
                .filter(|(scope, _)| inside(scope, &entry.path))
                .map(|(scope, _)| scope.len())
                .max();
            entry.passwords = hints
                .iter()
                .filter(|(scope, _)| Some(scope.len()) == nearest && inside(scope, &entry.path))
                .map(|(_, p)| p.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        }
    }
}
pub async fn collect(app: &App, roots: &[String]) -> ApiResult<Vec<Entry>> {
    let mut all = BTreeMap::<String, Entry>::new();
    let mut pending = Vec::new();
    for root in roots {
        if !valid_path(root) || root == "/" {
            return Err(bad("分享接收路径无效"));
        }
        let meta = app.0.gateway.metadata(root).await?;
        if meta.is_dir {
            pending.push(root.clone());
        }
        all.insert(
            root.clone(),
            Entry {
                path: root.clone(),
                name: meta.name,
                size: meta.size,
                is_dir: meta.is_dir,
                ad_reason: None,
                passwords: vec![],
            },
        );
    }
    let mut visited = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        for page in 1..=50 {
            let result = app
                .0
                .gateway
                .post(
                    "/api/fs/list",
                    json!({"path":dir,"page":page,"per_page":100,"refresh":true,"password":""}),
                )
                .await?;
            let rows = result["content"].as_array().cloned().unwrap_or_default();
            for row in &rows {
                let name = row["name"].as_str().context("网盘文件名缺失")?;
                if !control::remote_name(name) {
                    return Err(bad("网盘文件名不合法"));
                }
                let path = format!("{}/{name}", dir.trim_end_matches('/'));
                if !valid_path(&path) {
                    return Err(bad("分享目录路径过长或不合法"));
                }
                let is_dir = row["is_dir"] == true;
                if is_dir {
                    pending.push(path.clone());
                }
                all.insert(
                    path.clone(),
                    Entry {
                        path,
                        name: name.into(),
                        size: row["size"].as_u64().unwrap_or(0),
                        is_dir,
                        ad_reason: None,
                        passwords: vec![],
                    },
                );
                if all.len() > 5000 {
                    return Err(bad("分享超过 5000 项，请拆分分享"));
                }
            }
            if rows.len() < 100 || page * 100 >= result["total"].as_u64().unwrap_or(u64::MAX) {
                break;
            }
            if page == 50 {
                return Err(bad("分享目录过大，请拆分分享"));
            }
        }
    }
    let mut entries: Vec<_> = all.into_values().collect();
    let files: Vec<_> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| (e.path.clone(), e.size))
        .collect();
    for dir in entries.iter_mut().filter(|e| e.is_dir) {
        dir.size = files
            .iter()
            .filter(|(path, _)| inside(&dir.path, path))
            .map(|(_, size)| *size)
            .try_fold(0u64, |sum, n| sum.checked_add(n))
            .context("分享总大小超出范围")?;
    }
    annotate(&mut entries);
    Ok(entries)
}
async fn contents(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let all = app.0.control.shares.lock().await;
    let s = all.get(&id).ok_or_else(|| bad("分享任务不存在"))?;
    Ok(Json(
        json!({"entries":s.entries,"roots":s.roots,"capacity":s.capacity,"status":s.status,"message":s.message}),
    ))
}
#[derive(Deserialize)]
struct Choice {
    paths: Vec<String>,
    #[serde(default)]
    allow_ads: bool,
    #[serde(default = "crate::local::yes")]
    create_folder: bool,
}
async fn download(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(choice): Json<Choice>,
) -> ApiResult<Json<Value>> {
    if choice.paths.is_empty() || choice.paths.len() > 1000 {
        return Err(bad("请选择 1 至 1000 项内容"));
    }
    let _files = app.0.control.file_lock.lock().await;
    let s = app
        .0
        .control
        .shares
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| bad("分享任务不存在"))?;
    if !["ready", "queued"].contains(&s.status.as_str()) {
        return Err(bad("分享尚未准备好，请先完成转存或重新解析"));
    }
    if choice.paths.iter().any(|p| {
        !valid_path(p)
            || !s.entries.iter().any(|e| e.path == *p)
            || !s.roots.iter().any(|r| inside(r, p))
    }) {
        return Err(bad("所选内容不属于此分享"));
    }
    let chosen: Vec<_> = s
        .entries
        .iter()
        .filter(|e| !e.is_dir && choice.paths.iter().any(|p| inside(p, &e.path)))
        .collect();
    if chosen.is_empty() || chosen.len() > 1000 {
        return Err(bad("请选择 1 至 1000 个文件"));
    }
    if chosen.iter().any(|e| e.ad_reason.is_some()) && !choice.allow_ads {
        return Err(bad("所选文件包含疑似广告，请确认或取消选择这些文件"));
    }
    let name = format!(
        "{} · 分享下载",
        control::PROVIDERS
            .iter()
            .find(|(p, _)| *p == s.provider)
            .map(|(_, n)| *n)
            .unwrap_or("网盘")
    );
    // Enqueue the reviewed snapshot, not newly appeared files inside a folder.
    let paths = chosen.iter().map(|e| e.path.clone()).collect();
    let ids = control::enqueue_paths(&app, paths, Some((id, name)), choice.create_folder).await?;
    {
        let mut jobs = app.0.jobs.lock().await;
        for job in jobs.values_mut().filter(|j| ids.contains(&j.id)) {
            if let Some(entry) = chosen.iter().find(|e| e.path == job.path) {
                job.passwords = entry.passwords.clone();
            }
        }
        app.save(&jobs).await?;
    }
    let mut all = app.0.control.shares.lock().await;
    if let Some(s) = all.get_mut(&id) {
        s.status = "queued".into();
        s.message = format!("已按选择创建 {} 个下载任务", ids.len());
        for id in &ids {
            if !s.jobs.contains(id) {
                s.jobs.push(*id);
            }
        }
    }
    app.0.control.save(&all).await?;
    Ok(Json(json!({"jobs":ids})))
}
#[derive(Deserialize)]
struct Approval {
    #[serde(default)]
    allow_unknown_capacity: bool,
}
async fn prepare(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(input): Json<Approval>,
) -> ApiResult<Json<Value>> {
    let provider = {
        let mut all = app.0.control.shares.lock().await;
        let s = all.get_mut(&id).ok_or_else(|| bad("分享任务不存在"))?;
        if !["awaiting_transfer", "insufficient_capacity"].contains(&s.status.as_str()) {
            return Err(bad("当前分享不需要确认转存"));
        }
        if s.capacity["enough"] == false {
            return Err(bad("网盘剩余空间不足，请先管理文件并刷新容量"));
        }
        if s.capacity["known"] != true && !input.allow_unknown_capacity {
            return Err(bad("容量未能读取，请刷新或明确确认继续"));
        }
        s.transfer_approved = true;
        s.status = "waiting_login".into();
        let provider = s.provider.clone();
        app.0.control.save(&all).await?;
        provider
    };
    control::spawn_pending(&app, Some(&provider)).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct PreviewPath {
    path: String,
}
async fn share_preview(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Query(q): Query<PreviewPath>,
    headers: HeaderMap,
    method: axum::http::Method,
) -> ApiResult<Response> {
    {
        let all = app.0.control.shares.lock().await;
        let s = all.get(&id).ok_or_else(|| bad("分享任务不存在"))?;
        if !["ready", "queued"].contains(&s.status.as_str())
            || !s.entries.iter().any(|e| e.path == q.path && !e.is_dir)
            || !s.roots.iter().any(|r| inside(r, &q.path))
        {
            return Err(bad("预览文件不属于已准备的分享"));
        }
    }
    preview(&app, &q.path, &headers, method == axum::http::Method::HEAD).await
}
async fn cloud_preview(
    State(app): State<App>,
    Query(q): Query<PreviewPath>,
    headers: HeaderMap,
    method: axum::http::Method,
) -> ApiResult<Response> {
    preview(&app, &q.path, &headers, method == axum::http::Method::HEAD).await
}
async fn preview(app: &App, path: &str, headers: &HeaderMap, head: bool) -> ApiResult<Response> {
    if !valid_path(path) || path.split('/').filter(|p| !p.is_empty()).count() < 2 {
        return Err(bad("预览路径不合法"));
    }
    let cached = app
        .0
        .jobs
        .lock()
        .await
        .values()
        .find(|job| job.path == path && job.status == "completed")
        .map(|job| (crate::local::job_file(app, job), job.name.clone()));
    if let Some((local, name)) = cached {
        crate::local::checked_path(
            &app.0.downloads,
            &crate::local::relative(&app.0.downloads, &local)?,
            true,
        )
        .await?;
        if let Ok(meta) = tokio::fs::symlink_metadata(&local).await
            && meta.file_type().is_file()
        {
            let (kind, mime) = media(&name);
            if kind == "other" {
                return Err(bad("此格式暂不支持在线预览，请下载原文件"));
            }
            if kind == "image" && meta.len() > 24 * 1024 * 1024 {
                return Err(bad("图片超过 24 MB，请下载原文件查看"));
            }
            if meta.len() == 0 {
                return Ok(([(header::CONTENT_TYPE, mime)], Body::empty()).into_response());
            }
            let mut request = headers.clone();
            if kind == "text" {
                let max = meta.len().min(256 * 1024);
                let span = request
                    .get(header::RANGE)
                    .and_then(|h| h.to_str().ok())
                    .map(|s| crate::serve_file::byte_range(s, meta.len()))
                    .unwrap_or(Some((0, max - 1)));
                let Some((start, end)) = span else {
                    return Ok((
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, format!("bytes */{}", meta.len()))],
                    )
                        .into_response());
                };
                if start >= max {
                    return Err(bad("文本预览仅提供前 256 KB"));
                }
                request.insert(
                    header::RANGE,
                    format!("bytes={start}-{}", end.min(max - 1))
                        .parse()
                        .unwrap(),
                );
                request.remove(header::IF_RANGE);
            }
            let mut response = crate::serve_file::send(&local, &name, &request, head).await?;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, mime.parse().unwrap());
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, "inline".parse().unwrap());
            return Ok(response);
        }
    }
    let (meta, link) = app.0.gateway.resolve(path).await?;
    let (kind, mime) = media(&meta.name);
    if kind == "other" {
        return Err(bad("此格式暂不支持在线预览，请下载原文件"));
    }
    if kind == "image" && meta.size > 24 * 1024 * 1024 {
        return Err(bad("图片超过 24 MB，请下载原文件查看"));
    }
    if meta.size == 0 {
        return Ok(([(header::CONTENT_TYPE, mime)], Body::empty()).into_response());
    }
    let max = if kind == "text" {
        256 * 1024
    } else {
        meta.size
    };
    let requested = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range = if let Some(value) = requested {
        match crate::serve_file::byte_range(value, meta.size) {
            Some((start, end)) => {
                if kind == "text" && start >= max {
                    return Err(bad("文本预览仅提供前 256 KB"));
                }
                Some((start, end.min(max - 1)))
            }
            None => {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{}", meta.size))],
                )
                    .into_response());
            }
        }
    } else if kind == "text" {
        Some((0, meta.size.min(max) - 1))
    } else {
        None
    };
    if head {
        return Ok((
            [
                (header::CONTENT_TYPE, mime),
                (header::ACCEPT_RANGES, "bytes"),
                (header::CONTENT_LENGTH, &meta.size.min(max).to_string()),
            ],
            Body::empty(),
        )
            .into_response());
    }
    let mut req = app
        .0
        .client
        .get(link.url)
        .headers(link.headers)
        .header(header::ACCEPT_ENCODING, "identity");
    // Some Thunder CDN files close a plain GET. Explicit byte ranges work for
    // image requests too, even when the browser itself did not send a Range.
    let upstream_span = range.unwrap_or((0, meta.size - 1));
    req = req.header(
        header::RANGE,
        format!("bytes={}-{}", upstream_span.0, upstream_span.1),
    );
    let first = req.try_clone().unwrap().send().await;
    let upstream = match first {
        Ok(response) => Ok(response),
        Err(_) => req.send().await,
    }
    .map_err(|e| anyhow::Error::new(e.without_url()))?;
    let status = upstream.status();
    if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
        return Err(bad(&format!(
            "网盘预览暂不可用（HTTP {}）",
            status.as_u16()
        )));
    }
    {
        let (start, end) = upstream_span;
        if status != StatusCode::PARTIAL_CONTENT && start != 0 {
            return Err(bad("网盘暂不支持跳转预览"));
        }
        if status == StatusCode::PARTIAL_CONTENT {
            let expected = format!("bytes {start}-{end}/{}", meta.size);
            if upstream
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                != Some(&expected)
            {
                return Err(bad("网盘预览分段响应不正确"));
            }
        }
    }
    let length = range.map_or(meta.size, |(s, e)| e - s + 1);
    let mut outgoing = HeaderMap::new();
    outgoing.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    outgoing.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    outgoing.insert(header::CONTENT_DISPOSITION, "inline".parse().unwrap());
    outgoing.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
    let status = if let Some((start, end)) = range {
        outgoing.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", meta.size)
                .parse()
                .unwrap(),
        );
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    // Stop at the promised range even if an upstream ignores the limit. Dropping
    // the browser request drops this stream and closes the upstream connection.
    let mut remaining = length;
    let stream = upstream
        .bytes_stream()
        .map(move |result| {
            result.map_err(|e| e.without_url()).map(|bytes| {
                let take = (remaining as usize).min(bytes.len());
                remaining -= take as u64;
                bytes.slice(..take)
            })
        })
        .take_while(|result| {
            std::future::ready(match result {
                Ok(bytes) => !bytes.is_empty(),
                Err(_) => true,
            })
        });
    Ok((status, outgoing, Body::from_stream(stream)).into_response())
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/shares/{id}/contents", get(contents))
        .route(
            "/shares/{id}/download",
            post(download).layer(DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route("/shares/{id}/prepare", post(prepare))
        .route("/shares/{id}/preview", get(share_preview))
        .route("/preview", get(cloud_preview))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn annotations_preserve_names_and_scope_passwords_and_ads() {
        let mut entries = vec![
            Entry {
                path: "/s/解压密码：abc123.txt".into(),
                name: "解压密码：abc123.txt".into(),
                size: 0,
                is_dir: false,
                ad_reason: None,
                passwords: vec![],
            },
            Entry {
                path: "/s/资料.zip".into(),
                name: "资料.zip".into(),
                size: 512,
                is_dir: false,
                ad_reason: None,
                passwords: vec![],
            },
            Entry {
                path: "/other/独立.rar".into(),
                name: "独立.rar".into(),
                size: 512,
                is_dir: false,
                ad_reason: None,
                passwords: vec![],
            },
            Entry {
                path: "/s/推广".into(),
                name: "推广".into(),
                size: 0,
                is_dir: true,
                ad_reason: None,
                passwords: vec![],
            },
            Entry {
                path: "/s/推广/说明.txt".into(),
                name: "说明.txt".into(),
                size: 128,
                is_dir: false,
                ad_reason: None,
                passwords: vec![],
            },
        ];
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        annotate(&mut entries);
        assert_eq!(
            entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
            names
        );
        assert_eq!(entries[1].passwords, vec!["abc123"]);
        assert!(entries[2].passwords.is_empty());
        assert!(entries[4].ad_reason.is_some());
        assert!(entries[1].ad_reason.is_none());
        assert!(ad("说明.txt", false, 0).is_none());
        assert!(ad("网站.url", false, 48).is_some());
        assert_eq!(
            media("untrusted.html"),
            ("text", "text/plain; charset=utf-8")
        );
        assert_eq!(media("untrusted.svg").0, "text");
        assert!(!inside("/s", "/s-other/file"));
    }
}
