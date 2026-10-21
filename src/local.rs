//! Local folders use filesystem renames, never copy-and-delete or partition edits.
use crate::{ApiError, ApiResult, App, Job, bad};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::fs;
use uuid::Uuid;

pub fn yes() -> bool {
    true
}
pub fn valid_relative(path: &str) -> Result<()> {
    ensure!(
        path.len() <= 4096
            && (path.is_empty()
                || path.split('/').all(|p| crate::control::valid_name(p)
                    && p.len() <= 255
                    && !p.starts_with('.'))),
        "本地路径不合法"
    );
    Ok(())
}
pub fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)?
        .to_str()
        .context("本地文件名不是 UTF-8")?
        .replace(std::path::MAIN_SEPARATOR, "/"))
}
fn api_path(path: &str) -> ApiResult<String> {
    let path = path
        .strip_prefix('/')
        .ok_or_else(|| bad("本地路径必须从 / 开始"))?;
    valid_relative(path).map_err(|_| bad("本地路径不能包含隐藏目录、路径穿越或控制字符"))?;
    Ok(path.into())
}
pub async fn checked_path(root: &Path, path: &str, missing: bool) -> Result<PathBuf> {
    valid_relative(path)?;
    let mut full = root.to_path_buf();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        full.push(part);
        match fs::symlink_metadata(&full).await {
            Ok(meta) => ensure!(!meta.file_type().is_symlink(), "不允许操作符号链接"),
            Err(e) if missing && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(full)
}
pub fn job_file(app: &App, job: &Job) -> PathBuf {
    app.0
        .downloads
        .join(job.local_dir.clone().unwrap_or_else(|| job.id.to_string()))
        .join(&job.name)
}
pub fn transfer_file(
    app: &App,
    task: &crate::transfers::Transfer,
    f: &crate::transfers::TransferFile,
) -> PathBuf {
    f.local_path
        .as_ref()
        .map(|p| app.0.downloads.join(p))
        .unwrap_or_else(|| {
            app.0
                .downloads
                .join(
                    task.destination
                        .clone()
                        .unwrap_or_else(|| format!("transfers/{}", task.id)),
                )
                .join(&f.name)
        })
}
pub fn batch_folder(name: &str, id: Uuid) -> String {
    format!(
        "{}-{}",
        crate::safe_name(name),
        &id.simple().to_string()[..8]
    )
}
pub fn common_parent(paths: &[&str]) -> String {
    let Some(first) = paths.first() else {
        return String::new();
    };
    let mut common: Vec<_> = first
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or("")
        .split('/')
        .collect();
    for path in paths.iter().skip(1) {
        let parts: Vec<_> = path
            .rsplit_once('/')
            .map(|(p, _)| p)
            .unwrap_or("")
            .split('/')
            .collect();
        common.truncate(
            common
                .iter()
                .zip(parts)
                .take_while(|(a, b)| **a == *b)
                .count(),
        );
    }
    common.join("/")
}
fn numbered(name: &str, n: u64) -> String {
    if n == 1 {
        return name.into();
    }
    let candidate = match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
        None => format!("{name} ({n})"),
    };
    if crate::safe_name(&candidate) == candidate {
        candidate
    } else {
        crate::safe_name(&format!("({n}) {name}"))
    }
}
pub async fn available_name(
    app: &App,
    jobs: &BTreeMap<Uuid, Job>,
    dir: &str,
    name: &str,
) -> Result<String> {
    for n in 1..=10000 {
        let name = numbered(name, n);
        let path = app.0.downloads.join(dir).join(&name);
        if !fs::try_exists(&path).await? && !jobs.values().any(|j| job_file(app, j) == path) {
            return Ok(name);
        }
    }
    anyhow::bail!("同名文件过多，请选择其他文件夹")
}
/// Linux RENAME_NOREPLACE is atomic: even another process cannot be overwritten.
/// EXDEV is returned directly; there is deliberately no copying fallback.
pub async fn rename_exclusive(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from, to) = (from.to_owned(), to.to_owned());
    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};
            unsafe extern "C" {
                fn renameat2(
                    olddirfd: i32,
                    oldpath: *const std::ffi::c_char,
                    newdirfd: i32,
                    newpath: *const std::ffi::c_char,
                    flags: u32,
                ) -> i32;
            }
            let from = CString::new(from.as_os_str().as_bytes())?;
            let to = CString::new(to.as_os_str().as_bytes())?;
            // AT_FDCWD = -100, RENAME_NOREPLACE = 1, supported by Docker's Linux runtime.
            if unsafe { renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) } == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (from, to);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "请在 Linux / Docker 中使用原子移动",
            ))
        }
    })
    .await
    .map_err(std::io::Error::other)?
}
fn inside(parent: &str, path: &str) -> bool {
    parent.is_empty() || path == parent || path.starts_with(&format!("{parent}/"))
}
fn overlaps(a: &str, b: &str) -> bool {
    inside(a, b) || inside(b, a)
}
fn protected(
    app: &App,
    jobs: &BTreeMap<Uuid, Job>,
    tasks: &BTreeMap<Uuid, crate::transfers::Transfer>,
) -> Result<Vec<String>> {
    let mut paths = vec![];
    for j in jobs.values().filter(|j| j.status != "completed") {
        paths.push(relative(&app.0.downloads, &job_file(app, j))?);
    }
    for t in tasks.values() {
        if t.status != "completed" && !t.hidden {
            if t.files.is_empty() {
                let dir = t
                    .destination
                    .clone()
                    .unwrap_or_else(|| format!("transfers/{}", t.id));
                if !dir.is_empty()
                    || matches!(t.status.as_str(), "metadata" | "queued" | "downloading")
                {
                    paths.push(dir);
                } else {
                    paths.push(t.name.clone());
                }
            } else {
                for f in &t.files {
                    paths.push(relative(&app.0.downloads, &transfer_file(app, t, f))?);
                }
            }
        }
        for f in t
            .files
            .iter()
            .filter(|f| f.completed != f.length && !f.local_deleted)
        {
            paths.push(relative(&app.0.downloads, &transfer_file(app, t, f))?);
        }
    }
    Ok(paths)
}
fn ensure_free(path: &str, busy: &[String], subtree: bool) -> ApiResult<()> {
    if busy.iter().any(|p| {
        if subtree {
            overlaps(path, p)
        } else {
            inside(p, path)
        }
    }) {
        return Err(bad(
            "内容涉及未完成下载或做种，请先完成任务；BT 可停止做种并移除任务记录后再整理",
        ));
    }
    Ok(())
}
#[derive(Deserialize)]
struct Location {
    #[serde(default = "root")]
    path: String,
}
fn root() -> String {
    "/".into()
}
async fn tree(State(app): State<App>, Query(q): Query<Location>) -> ApiResult<Json<Value>> {
    let path = api_path(&q.path)?;
    let _guard = app.0.control.file_lock.lock().await;
    let dir = checked_path(&app.0.downloads, &path, false)
        .await
        .map_err(|_| bad("目录不存在或包含符号链接"))?;
    let jobs = app.0.jobs.lock().await;
    let tasks = app.0.transfers.tasks.lock().await;
    let busy = protected(&app, &jobs, &tasks)?;
    let mut entries = vec![];
    let mut children = fs::read_dir(dir).await?;
    while let Some(entry) = children.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.ends_with(".aria2") {
            continue;
        }
        let kind = entry.file_type().await?;
        if !kind.is_file() && !kind.is_dir() {
            continue;
        }
        let relative = relative(&app.0.downloads, &entry.path())?;
        if valid_relative(&relative).is_err() {
            continue;
        }
        let locked = busy.iter().any(|p| overlaps(&relative, p));
        let transfer = tasks
            .values()
            .filter(|t| {
                matches!(
                    t.status.as_str(),
                    "completed" | "paused" | "seeding" | "checking"
                )
            })
            .find_map(|t| {
                t.files
                    .iter()
                    .find(|f| {
                        !f.local_deleted
                            && f.completed == f.length
                            && transfer_file(&app, t, f) == entry.path()
                    })
                    .map(|f| (t.id, f.index))
            });
        // Finished BT files remain readable while seeding, but cannot be moved.
        if kind.is_file() && locked && transfer.is_none() {
            continue;
        }
        let seeds: Vec<_> = tasks
            .values()
            .filter(|t| t.seed_linked())
            .filter(|t| {
                t.files.iter().any(|f| {
                    !f.local_deleted && f.selected && {
                        let file = transfer_file(&app, t, f);
                        if kind.is_dir() {
                            file.starts_with(entry.path())
                        } else {
                            file == entry.path()
                        }
                    }
                })
            })
            .map(|t| t.seed_summary())
            .collect();
        let meta = entry.metadata().await?;
        let job = jobs.values().find(|j| job_file(&app, j) == entry.path());
        entries.push(json!({"path":format!("/{relative}"),"name":name,"is_dir":kind.is_dir(),"size":if kind.is_file(){Some(meta.len())}else{None},"locked":locked,"seeding":seeds,"download_url":transfer.map(|(id,index)|format!("/api/transfers/{id}/files/{index}")),"sha256":job.and_then(|j|j.sha256.clone()),"passwords":job.map(|j|j.passwords.clone()).unwrap_or_default(),"modified":meta.modified().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d|d.as_secs())}));
    }
    entries.sort_by_key(|e| {
        (
            !e["is_dir"].as_bool().unwrap_or(false),
            e["name"].as_str().unwrap_or("").to_lowercase(),
        )
    });
    Ok(Json(
        json!({"path":q.path,"entries":entries,"move_mode":"rename_no_replace"}),
    ))
}
async fn file(
    State(app): State<App>,
    Query(q): Query<Location>,
    headers: HeaderMap,
    method: Method,
) -> ApiResult<Response> {
    let path = api_path(&q.path)?;
    let _guard = app.0.control.file_lock.lock().await;
    let jobs = app.0.jobs.lock().await;
    let tasks = app.0.transfers.tasks.lock().await;
    ensure_free(&path, &protected(&app, &jobs, &tasks)?, false)?;
    let full = checked_path(&app.0.downloads, &path, false)
        .await
        .map_err(|_| bad("文件不存在或包含符号链接"))?;
    if !fs::symlink_metadata(&full).await?.is_file() {
        return Err(bad("请选择已完成的本地文件"));
    }
    let name = full
        .file_name()
        .context("文件名无效")?
        .to_string_lossy()
        .into_owned();
    crate::serve_file::send(&full, &name, &headers, method == Method::HEAD).await
}
#[derive(Deserialize)]
struct Operation {
    action: String,
    #[serde(default = "root")]
    path: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    name: String,
    #[serde(default = "root")]
    destination: String,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    moves: Vec<(String, String)>,
}
fn replace_prefix(path: &str, from: &str, to: &str) -> Option<String> {
    inside(from, path).then(|| format!("{to}{}", &path[from.len()..]))
}
async fn record_moves(
    app: &App,
    jobs: &mut BTreeMap<Uuid, Job>,
    tasks: &mut BTreeMap<Uuid, crate::transfers::Transfer>,
    moves: &[(String, String)],
) -> Result<()> {
    for (from, to) in moves {
        for job in jobs.values_mut() {
            let old = relative(&app.0.downloads, &job_file(app, job))?;
            if let Some(new) = replace_prefix(&old, from, to) {
                let path = Path::new(&new);
                job.local_dir = Some(path.parent().context("目录无效")?.to_string_lossy().into());
                job.name = path
                    .file_name()
                    .context("文件名无效")?
                    .to_string_lossy()
                    .into();
            }
        }
        for task in tasks.values_mut() {
            let updates: Vec<_> = task
                .files
                .iter()
                .map(|f| {
                    relative(&app.0.downloads, &transfer_file(app, task, f))
                        .map(|old| replace_prefix(&old, from, to))
                })
                .collect::<Result<_>>()?;
            for (f, path) in task.files.iter_mut().zip(updates) {
                if let Some(path) = path {
                    f.local_path = Some(path);
                }
            }
        }
    }
    app.save(jobs).await?;
    app.0.transfers.persist(tasks).await?;
    Ok(())
}
/// A tiny write-ahead journal repairs references after interruption between rename and JSON save.
pub async fn recover(app: &App) -> Result<()> {
    let path = app.0.data.join("local-move.json");
    if !fs::try_exists(&path).await? {
        return Ok(());
    }
    let journal: Journal = serde_json::from_slice(&fs::read(&path).await?)?;
    let mut applied = vec![];
    for (from, to) in journal.moves {
        let source = checked_path(&app.0.downloads, &from, true).await?;
        let target = checked_path(&app.0.downloads, &to, true).await?;
        if !fs::try_exists(source).await? && fs::try_exists(target).await? {
            applied.push((from, to));
        }
    }
    record_moves(
        app,
        &mut *app.0.jobs.lock().await,
        &mut *app.0.transfers.tasks.lock().await,
        &applied,
    )
    .await?;
    fs::remove_file(path).await?;
    Ok(())
}
async fn operation(State(app): State<App>, Json(input): Json<Operation>) -> ApiResult<Json<Value>> {
    let _guard = app.0.control.file_lock.lock().await;
    let running = app.0.running.lock().await;
    let mut jobs = app.0.jobs.lock().await;
    let mut tasks = app.0.transfers.tasks.lock().await;
    let busy = protected(&app, &jobs, &tasks)?;
    if input.action == "mkdir" {
        let parent = api_path(&input.path)?;
        ensure_free(&parent, &busy, false)?;
        if !crate::control::valid_name(&input.name)
            || input.name.starts_with('.')
            || input.name.len() > 200
        {
            return Err(bad("请输入有效的文件夹名称，最多 200 个 UTF-8 字节"));
        }
        let parent = checked_path(&app.0.downloads, &parent, false).await?;
        fs::create_dir(parent.join(input.name)).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                bad("同名文件或文件夹已存在")
            } else {
                e.into()
            }
        })?;
        return Ok(Json(json!({"ok":true})));
    }
    if input.paths.is_empty() || input.paths.len() > 100 {
        return Err(bad("每次请选择 1–100 个文件或文件夹"));
    }
    let sources: Vec<_> = input
        .paths
        .iter()
        .map(|p| api_path(p))
        .collect::<ApiResult<_>>()?;
    for (i, p) in sources.iter().enumerate() {
        if p.is_empty()
            || sources
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && overlaps(p, other))
        {
            return Err(bad("不能操作下载根目录，或同时选择文件夹及其内容"));
        }
        ensure_free(p, &busy, true)?;
        checked_path(&app.0.downloads, p, false).await?;
    }
    if input.action == "delete" {
        if sources.len() != 1 {
            return Err(bad("删除时请只选择一项"));
        }
        let path = app.0.downloads.join(&sources[0]);
        if fs::symlink_metadata(&path).await?.is_dir() {
            fs::remove_dir(&path)
                .await
                .map_err(|_| bad("文件夹非空，请先移出或删除其中的文件"))?;
        } else {
            fs::remove_file(&path).await?;
        }
        jobs.retain(|_, job| {
            relative(&app.0.downloads, &job_file(&app, job))
                .ok()
                .as_ref()
                != Some(&sources[0])
        });
        for task in tasks.values_mut() {
            let deleted: Vec<_> = task
                .files
                .iter()
                .map(|f| {
                    relative(&app.0.downloads, &transfer_file(&app, task, f))
                        .ok()
                        .as_ref()
                        == Some(&sources[0])
                })
                .collect();
            for (f, deleted) in task.files.iter_mut().zip(deleted) {
                if deleted {
                    f.local_deleted = true;
                }
            }
        }
        app.save(&jobs).await?;
        app.0.transfers.persist(&tasks).await?;
        return Ok(Json(json!({"ok":true})));
    }
    let destination = if input.action == "rename" {
        if sources.len() != 1
            || !crate::control::valid_name(&input.name)
            || input.name.starts_with('.')
            || input.name.len() > 200
            || crate::safe_name(&input.name) != input.name
        {
            return Err(bad("请选择一项并填写有效名称"));
        }
        Path::new(&sources[0])
            .parent()
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .into_owned()
    } else if input.action == "move" {
        api_path(&input.destination)?
    } else {
        return Err(bad("不支持的本地文件操作"));
    };
    ensure_free(&destination, &busy, false)?;
    let dest = checked_path(&app.0.downloads, &destination, false).await?;
    if !fs::symlink_metadata(&dest).await?.is_dir() {
        return Err(bad("目标必须是文件夹"));
    }
    let mut moves = vec![];
    for source in &sources {
        let name = if input.action == "rename" {
            input.name.as_str()
        } else {
            Path::new(source)
                .file_name()
                .unwrap()
                .to_str()
                .context("文件名无效")?
        };
        let target = relative(&app.0.downloads, &dest.join(name))?;
        if inside(source, &target) {
            return Err(bad("不能移动到自身或自身子目录"));
        }
        if fs::try_exists(app.0.downloads.join(&target)).await?
            || moves.iter().any(|(_, t)| *t == target)
        {
            return Err(bad("目标已有同名文件或文件夹，请更改名称或选择其他目标"));
        }
        ensure_free(&target, &busy, true)?;
        moves.push((source.clone(), target));
    }
    let journal_path = app.0.data.join("local-move.json");
    if fs::try_exists(&journal_path).await? {
        return Err(bad("上次移动记录尚未恢复，请重启服务后再整理文件"));
    }
    crate::bandwidth::private_json(
        &journal_path,
        &Journal {
            moves: moves.clone(),
        },
    )
    .await?;
    let mut applied = vec![];
    let mut failure = None;
    for (from, to) in &moves {
        match rename_exclusive(&app.0.downloads.join(from), &app.0.downloads.join(to)).await {
            Ok(()) => applied.push((from.clone(), to.clone())),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    // On a filesystem failure, completed items remain tracked and the UI reports partial success.
    record_moves(&app, &mut jobs, &mut tasks, &applied).await?;
    fs::remove_file(journal_path).await?;
    drop(running);
    if let Some(e) = failure {
        let message = if e.raw_os_error() == Some(18) {
            "跨文件系统不能使用元数据移动；未进行复制".to_string()
        } else {
            format!("移动失败：{e}")
        };
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("{message}；本次已移动 {} 项，请刷新查看", applied.len()),
        ));
    }
    Ok(Json(
        json!({"ok":true,"moved":applied.len(),"method":"rename","copied_bytes":0}),
    ))
}
pub async fn rename_job(app: &App, id: Uuid, name: String) -> ApiResult<()> {
    let full = crate::files::local_path(app, id).await?;
    let path = format!("/{}", relative(&app.0.downloads, &full)?);
    let _ = operation(
        State(app.clone()),
        Json(Operation {
            action: "rename".into(),
            path: root(),
            paths: vec![path],
            name,
            destination: root(),
        }),
    )
    .await?;
    Ok(())
}
pub async fn delete_job(app: &App, id: Uuid) -> ApiResult<()> {
    let full = crate::files::local_path(app, id).await?;
    let path = format!("/{}", relative(&app.0.downloads, &full)?);
    let _ = operation(
        State(app.clone()),
        Json(Operation {
            action: "delete".into(),
            path: root(),
            paths: vec![path],
            name: String::new(),
            destination: root(),
        }),
    )
    .await?;
    Ok(())
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/local/tree", get(tree))
        .route("/local/tree/file", get(file))
        .route("/local/operation", post(operation))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cross_filesystem_move_never_copies() {
        use std::os::unix::fs::MetadataExt;
        let target_root = Path::new("/dev/shm");
        let source_root = std::env::temp_dir();
        if !target_root.is_dir()
            || fs::metadata(target_root).await.unwrap().dev()
                == fs::metadata(&source_root).await.unwrap().dev()
        {
            return;
        }
        let name = format!("flowdisk-move-check-{}", Uuid::new_v4());
        let source = source_root.join(&name);
        let target = target_root.join(&name);
        fs::write(&source, b"original bytes").await.unwrap();
        let result = rename_exclusive(&source, &target).await;
        assert_eq!(result.unwrap_err().raw_os_error(), Some(18));
        assert_eq!(fs::read(&source).await.unwrap(), b"original bytes");
        assert!(!fs::try_exists(&target).await.unwrap());
        fs::remove_file(source).await.unwrap();
    }
}
