use crate::{ApiError, ApiResult, App, bad, control::valid_name, valid_path};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::fs;
use uuid::Uuid;

pub(crate) async fn local_path(app: &App, id: Uuid) -> ApiResult<PathBuf> {
    let job = app.0.jobs.lock().await.get(&id).cloned();
    local_path_for(app, id, job.as_ref()).await
}
async fn local_path_for(app: &App, id: Uuid, job: Option<&crate::Job>) -> ApiResult<PathBuf> {
    if let Some(job) = job {
        let path = crate::local::job_file(app, job);
        crate::local::checked_path(
            &app.0.downloads,
            &crate::local::relative(&app.0.downloads, &path)?,
            false,
        )
        .await?;
        if fs::symlink_metadata(&path).await?.is_file() {
            return Ok(path);
        }
        return Err(bad("文件尚未完成或已经删除"));
    }
    let dir = app.0.downloads.join(id.to_string());
    let meta = fs::symlink_metadata(&dir)
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "本地文件不存在".into()))?;
    if !meta.file_type().is_dir() {
        return Err(bad("本地下载目录不合法"));
    }
    let mut entries = fs::read_dir(&dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && valid_name(&name) && entry.file_type().await?.is_file() {
            return Ok(entry.path());
        }
    }
    Err(ApiError(
        StatusCode::NOT_FOUND,
        "文件尚未完成或已经删除".into(),
    ))
}
async fn local_list(State(app): State<App>) -> ApiResult<Json<Value>> {
    let _lock = app.0.control.file_lock.lock().await;
    let running = app.0.running.lock().await;
    let jobs = app.0.jobs.lock().await;
    let mut folders = fs::read_dir(&app.0.downloads).await?;
    let mut files = Vec::new();
    while let Some(entry) = folders.next_entry().await? {
        let Ok(id) = Uuid::parse_str(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if running.contains_key(&id) || !entry.file_type().await?.is_dir() {
            continue;
        }
        let Ok(path) = local_path_for(&app, id, jobs.get(&id)).await else {
            continue;
        };
        let meta = fs::metadata(&path).await?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        files.push(json!({"id":id,"name":path.file_name().unwrap().to_string_lossy(),"size":meta.len(),"modified":modified,"sha256":jobs.get(&id).and_then(|j|j.sha256.clone()),"source":jobs.get(&id).map(|j|j.path.clone()),"passwords":jobs.get(&id).map(|j|j.passwords.clone()).unwrap_or_default()}));
    }
    for job in jobs
        .values()
        .filter(|j| j.local_dir.is_some() && j.status == "completed")
    {
        if files.iter().any(|f| f["id"] == job.id.to_string()) {
            continue;
        }
        let path = crate::local::job_file(&app, job);
        if let Ok(meta) = fs::symlink_metadata(&path).await
            && meta.is_file()
        {
            files.push(json!({"id":job.id,"name":job.name,"size":meta.len(),"sha256":job.sha256,"passwords":job.passwords}));
        }
    }
    files.sort_by_key(|v| std::cmp::Reverse(v["modified"].as_u64().unwrap_or(0)));
    Ok(Json(json!(files)))
}
async fn local_file(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    method: axum::http::Method,
) -> ApiResult<Response> {
    let _guard = app.0.control.file_lock.lock().await;
    if app.0.running.lock().await.contains_key(&id) {
        return Err(bad("文件仍在下载中"));
    }
    let path = local_path(&app, id).await?;
    let name = path.file_name().unwrap().to_string_lossy();
    crate::serve_file::send(&path, &name, &headers, method == axum::http::Method::HEAD).await
}
#[derive(Deserialize)]
struct Rename {
    name: String,
}
async fn local_rename(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(input): Json<Rename>,
) -> ApiResult<Json<Value>> {
    crate::local::rename_job(&app, id, input.name).await?;
    Ok(Json(json!({"ok":true})))
}
async fn local_delete(State(app): State<App>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    crate::local::delete_job(&app, id).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct CloudOperation {
    action: String,
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    destination: String,
}
fn non_root(path: &str) -> bool {
    valid_path(path) && path.split('/').filter(|v| !v.is_empty()).count() >= 1
}
fn mount(path: &str) -> &str {
    path.trim_start_matches('/').split('/').next().unwrap_or("")
}
fn contains(parent: &str, child: &str) -> bool {
    parent == child || child.starts_with(&format!("{}/", parent.trim_end_matches('/')))
}
async fn cloud_operation(
    State(app): State<App>,
    Json(input): Json<CloudOperation>,
) -> ApiResult<Json<Value>> {
    if !non_root(&input.path)
        || input.names.len() > 100
        || !input.names.iter().all(|n| crate::control::remote_name(n))
    {
        return Err(bad("请选择有效的网盘目录和文件，每次最多操作 100 项"));
    }
    let _lock = app.0.control.file_lock.lock().await;
    let affected = if input.action == "rename" {
        vec![input.path.clone()]
    } else {
        input
            .names
            .iter()
            .map(|name| format!("{}/{name}", input.path.trim_end_matches('/')))
            .collect::<Vec<_>>()
    };
    let mut jobs = app.0.jobs.lock().await;
    if jobs.values().any(|j| {
        matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
            && affected.iter().any(|p| contains(p, &j.path))
    }) {
        return Err(bad("所选内容正在下载，请先暂停相关任务"));
    }
    let mut replacements = Vec::new();
    let (route, body) = match input.action.as_str() {
        "mkdir" => {
            if !valid_name(&input.name) {
                return Err(bad("文件夹名称不合法"));
            }
            (
                "/api/fs/mkdir",
                json!({"path":format!("{}/{}",input.path.trim_end_matches('/'),input.name)}),
            )
        }
        "rename" => {
            if input.path.split('/').filter(|p| !p.is_empty()).count() < 2
                || !valid_name(&input.name)
            {
                return Err(bad("不能重命名网盘根目录，且名称不得包含路径"));
            }
            let parent = input.path.rsplit_once('/').unwrap().0;
            let target = format!("{parent}/{}", input.name);
            replacements.push((input.path.clone(), target));
            (
                "/api/fs/rename",
                json!({"path":input.path,"name":input.name,"overwrite":false}),
            )
        }
        "move" => {
            if input.names.is_empty()
                || !non_root(&input.destination)
                || mount(&input.path) != mount(&input.destination)
            {
                return Err(bad("请选择同一网盘中的目标文件夹"));
            }
            if input.path.trim_end_matches('/') == input.destination.trim_end_matches('/')
                || affected.iter().any(|p| contains(p, &input.destination))
            {
                return Err(bad("不能移动到原目录或自身子目录"));
            }
            // Moving active or paused task sources asynchronously would invalidate their persisted paths.
            if jobs
                .values()
                .any(|j| j.status != "completed" && affected.iter().any(|p| contains(p, &j.path)))
            {
                return Err(bad("请先移除这些文件的未完成下载任务，再移动网盘文件"));
            }
            (
                "/api/fs/move",
                json!({"src_dir":input.path,"dst_dir":input.destination,"names":input.names,"overwrite":false,"skip_existing":false,"merge":false}),
            )
        }
        "delete" => {
            if input.names.is_empty() {
                return Err(bad("请选择要删除的网盘文件"));
            }
            if jobs
                .values()
                .any(|j| j.status != "completed" && affected.iter().any(|p| contains(p, &j.path)))
            {
                return Err(bad("请先移除这些文件的未完成下载任务，再删除网盘文件"));
            }
            (
                "/api/fs/remove",
                json!({"dir":input.path,"names":input.names}),
            )
        }
        _ => return Err(bad("不支持的文件管理操作")),
    };
    let result = app.0.gateway.post(route, body).await?;
    for (old, new) in replacements {
        for job in jobs.values_mut() {
            if contains(&old, &job.path) {
                job.path = format!("{new}{}", &job.path[old.len()..]);
            }
        }
    }
    app.save(&jobs).await?;
    Ok(Json(
        json!({"ok":true,"submitted":input.action=="move","result":result}),
    ))
}
#[derive(Deserialize)]
struct Batch {
    paths: Vec<String>,
    #[serde(default = "crate::local::yes")]
    create_folder: bool,
}
async fn batch_download(
    State(app): State<App>,
    Json(input): Json<Batch>,
) -> ApiResult<Json<Value>> {
    if input.paths.is_empty() || input.paths.len() > 100 {
        return Err(bad("请选择 1–100 项文件或文件夹"));
    }
    let _guard = app.0.control.file_lock.lock().await;
    let name = if input.paths.len() == 1 {
        format!(
            "下载 · {}",
            input.paths[0]
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("文件")
        )
    } else {
        format!("批量下载 · {} 项", input.paths.len())
    };
    let group = (Uuid::new_v4(), name);
    let ids =
        crate::control::enqueue_paths(&app, input.paths, Some(group), input.create_folder).await?;
    Ok(Json(json!({"jobs":ids})))
}
#[derive(Deserialize)]
struct UploadPath {
    path: String,
}
async fn cloud_upload(
    State(app): State<App>,
    Query(input): Query<UploadPath>,
    bytes: Bytes,
) -> ApiResult<Json<Value>> {
    if !valid_path(&input.path)
        || input.path.split('/').filter(|s| !s.is_empty()).count() < 2
        || !valid_name(input.path.rsplit('/').next().unwrap_or(""))
    {
        return Err(bad("请选择网盘目录和有效文件名"));
    }
    let _lock = app.0.control.file_lock.lock().await;
    if app.0.jobs.lock().await.values().any(|j| {
        j.path == input.path && matches!(j.status.as_str(), "queued" | "downloading" | "pausing")
    }) {
        return Err(bad("同名文件正在下载，请先暂停任务"));
    }
    Ok(Json(app.0.gateway.upload(&input.path, bytes).await?))
}
pub fn routes() -> Router<App> {
    Router::new()
        .route(
            "/files/upload",
            put(cloud_upload).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .merge(crate::local::routes())
        .route("/local", get(local_list))
        .route("/local/{id}", axum::routing::delete(local_delete))
        .route("/local/{id}/rename", post(local_rename))
        .route("/local/{id}/file", get(local_file))
        .route("/files/operation", post(cloud_operation))
        .route("/batch-download", post(batch_download))
}
