use crate::{
    App,
    gateway::{Link, Metadata},
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, TryStreamExt};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Progress {
    pub bytes: Arc<AtomicU64>,
    pub transferred: Arc<AtomicU64>,
}
const CHUNK: u64 = 8 * 1024 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Manifest {
    size: u64,
    chunk: u64,
    modified: String,
    hash_info: serde_json::Value,
    validator: Option<String>,
}
struct Probe {
    size: u64,
    ranged: bool,
    validator: Option<String>,
}
fn range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (span, total) = value.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}
fn validator(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.starts_with("W/"))
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get(header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
}
async fn probe(
    app: &App,
    link: &Link,
    meta: &Metadata,
    cancel: &CancellationToken,
) -> Result<Probe> {
    let _permit = tokio::select! { _ = cancel.cancelled() => bail!("已暂停"), p = app.0.connections.acquire() => p? };
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("已暂停"),
        r = app.0.client.get(link.url.clone()).headers(link.headers.clone()).header(header::ACCEPT_ENCODING,"identity").header(header::RANGE,"bytes=0-0").send() => r.map_err(|e|e.without_url())?,
    };
    let validator = validator(response.headers());
    if response.status() == StatusCode::PARTIAL_CONTENT {
        let r = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(range)
            .context("分段探测缺少有效 Content-Range")?;
        ensure!(
            r.0 == 0 && r.1 == 0 && r.2 > 0 && r.2 == meta.size,
            "文件大小与网盘记录不一致，请刷新目录后重试"
        );
        ensure!(
            response.content_length().is_none_or(|n| n == 1),
            "分段探测长度不合法"
        );
        Ok(Probe {
            size: r.2,
            ranged: true,
            validator,
        })
    } else if response.status() == StatusCode::OK {
        ensure!(
            response.content_length().is_none_or(|n| n == meta.size),
            "下载内容大小与网盘记录不一致"
        );
        Ok(Probe {
            size: meta.size,
            ranged: false,
            validator,
        })
    } else if response.status() == StatusCode::RANGE_NOT_SATISFIABLE
        && meta.size == 0
        && response
            .headers()
            .get(header::CONTENT_RANGE)
            .is_some_and(|v| v == "bytes */0")
    {
        Ok(Probe {
            size: 0,
            ranged: true,
            validator,
        })
    } else {
        bail!("下载探测失败（HTTP {}）", response.status().as_u16());
    }
}

pub async fn run(
    app: &App,
    id: uuid::Uuid,
    cancel: &CancellationToken,
    progress: Progress,
) -> Result<String> {
    let job = app
        .0
        .jobs
        .lock()
        .await
        .get(&id)
        .cloned()
        .context("任务不存在")?;
    let parts = app.0.data.join("parts").join(id.to_string());
    let output_dir = app.0.downloads.join(id.to_string());
    fs::create_dir_all(&parts).await?;
    fs::create_dir_all(&output_dir).await?;
    let mut last_error = None;
    // A new resolution renews expired links and preserves each provider's headers.
    for round in 0..3 {
        if cancel.is_cancelled() {
            bail!("已暂停");
        }
        let attempt = async {
            let (meta, link) = app.0.gateway.resolve(&job.path).await?;
            let probe = probe(app, &link, &meta, cancel).await?;
            let chunk = CHUNK.max(probe.size.div_ceil(10_000));
            let manifest = Manifest {
                size: probe.size,
                chunk,
                modified: meta.modified,
                hash_info: meta.hash_info,
                validator: probe.validator,
            };
            let manifest_file = parts.join("manifest.json");
            let old = if fs::try_exists(&manifest_file).await? {
                Some(
                    serde_json::from_slice::<Manifest>(&fs::read(&manifest_file).await?)
                        .context("分段记录损坏，请移除任务后重新下载")?,
                )
            } else {
                None
            };
            if old.as_ref() != Some(&manifest) {
                fs::remove_dir_all(&parts).await?;
                fs::create_dir_all(&parts).await?;
                fs::write(&manifest_file, serde_json::to_vec(&manifest)?).await?;
                fs::File::open(&manifest_file).await?.sync_all().await?;
            }
            let count = manifest.size.div_ceil(chunk);
            let mut existing = 0;
            for index in 0..count {
                let path = parts.join(index.to_string());
                let length = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                let expected = chunk.min(manifest.size - index * chunk);
                if length > expected {
                    fs::remove_file(&path).await?;
                } else {
                    existing += length;
                }
            }
            progress
                .bytes
                .store(if probe.ranged { existing } else { 0 }, Ordering::Relaxed);
            let requested = (app.0.threads >> round).max(1);
            let concurrency = if probe.ranged {
                if link.concurrency > 0 {
                    requested.min(link.concurrency)
                } else {
                    requested
                }
            } else {
                1
            };
            {
                let mut jobs = app.0.jobs.lock().await;
                let job = jobs.get_mut(&id).context("任务不存在")?;
                job.size = Some(manifest.size);
                job.connections = concurrency.min(count.max(1) as usize);
                job.downloaded = progress.bytes.load(Ordering::Relaxed);
            }
            if probe.ranged {
                let work_cancel = cancel.child_token();
                let downloads = futures_util::stream::iter(0..count)
                    .map(|index| {
                        part(
                            app,
                            &link,
                            &manifest,
                            &parts,
                            index,
                            &work_cancel,
                            progress.clone(),
                        )
                    })
                    .buffer_unordered(concurrency);
                tokio::pin!(downloads);
                let mut first_error = None;
                // Drain workers after cancellation, so resumed tasks never race old writes.
                while let Some(result) = downloads.next().await {
                    if let Err(error) = result
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                        work_cancel.cancel();
                    }
                }
                if let Some(error) = first_error {
                    return Err(error);
                }
            } else {
                sequential(
                    app,
                    &link,
                    manifest.size,
                    &parts.join("single"),
                    cancel,
                    progress.clone(),
                )
                .await?;
            }
            if cancel.is_cancelled() {
                bail!("已暂停");
            }
            let temp = output_dir.join(".assembling");
            let mut output = fs::File::create(&temp).await?;
            let mut hasher = Sha256::new();
            let files: Vec<PathBuf> = if probe.ranged {
                (0..count).map(|i| parts.join(i.to_string())).collect()
            } else {
                vec![parts.join("single")]
            };
            let mut buffer = vec![0; 1024 * 1024];
            let mut total = 0;
            for path in files {
                let mut input = fs::File::open(path).await?;
                loop {
                    if cancel.is_cancelled() {
                        bail!("已暂停");
                    }
                    let size = input.read(&mut buffer).await?;
                    if size == 0 {
                        break;
                    }
                    output.write_all(&buffer[..size]).await?;
                    hasher.update(&buffer[..size]);
                    total += size as u64;
                }
            }
            ensure!(total == manifest.size, "文件合并长度不匹配");
            output.sync_all().await?;
            drop(output);
            fs::rename(temp, output_dir.join(&job.name)).await?;
            // Failure to clean scratch files must not turn a valid download into a failure.
            if let Err(err) = fs::remove_dir_all(&parts).await {
                eprintln!("临时分段清理失败: {err}");
            }
            progress.bytes.store(total, Ordering::Relaxed);
            Ok(format!("{:x}", hasher.finalize()))
        }
        .await;
        match attempt {
            Ok(hash) => return Ok(hash),
            Err(err) => last_error = Some(err),
        }
        tokio::select! { _ = cancel.cancelled() => bail!("已暂停"), _ = tokio::time::sleep(Duration::from_secs(2u64.pow(round + 1))) => {} }
    }
    Err(last_error.context("下载失败")?)
}

async fn part(
    app: &App,
    link: &Link,
    manifest: &Manifest,
    dir: &Path,
    index: u64,
    cancel: &CancellationToken,
    progress: Progress,
) -> Result<()> {
    let start = index * manifest.chunk;
    let end = (start + manifest.chunk).min(manifest.size) - 1;
    let path = dir.join(index.to_string());
    let expected = end - start + 1;
    let mut failures = 0u32;
    let mut progress_checkpoint = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    for _ in 0..512 {
        if cancel.is_cancelled() {
            bail!("已暂停");
        }
        let current = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        if current == expected {
            return Ok(());
        }
        ensure!(current < expected, "本地分段长度不合法");
        let request_end = if link.max_range > 0 {
            end.min(start + current + link.max_range - 1)
        } else {
            end
        };
        let wanted = request_end - (start + current) + 1;
        let _permit = tokio::select! { _ = cancel.cancelled() => bail!("已暂停"), p = app.0.connections.acquire() => p? };
        let mut request = app
            .0
            .client
            .get(link.url.clone())
            .headers(link.headers.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .header(
                header::RANGE,
                format!("bytes={}-{}", start + current, request_end),
            );
        if let Some(v) = &manifest.validator {
            request = request.header(header::IF_RANGE, v);
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("已暂停"),
            r = request.send() => r.map_err(|e|e.without_url()),
        };
        let mut delay = Duration::from_millis(500 * 2u64.pow(failures.min(5)) + index % 173);
        let result: Result<()> = async {
            let response = response?;
            if matches!(response.status().as_u16(),401 | 403 | 410) { bail!("下载链接已过期，需要刷新"); }
            if response.status() == StatusCode::TOO_MANY_REQUESTS || response.status() == StatusCode::SERVICE_UNAVAILABLE {
                if let Some(seconds) = response.headers().get(header::RETRY_AFTER).and_then(|v|v.to_str().ok()).and_then(|v|v.parse::<u64>().ok()) { delay = Duration::from_secs(seconds.clamp(1,60)); }
                bail!("网盘限制并发（HTTP {}）",response.status().as_u16());
            }
            ensure!(response.status() == StatusCode::PARTIAL_CONTENT, "服务器拒绝分段请求（HTTP {}），未写入不匹配的数据",response.status().as_u16());
            let r = response.headers().get(header::CONTENT_RANGE).and_then(|v|v.to_str().ok()).and_then(range);
            ensure!(r == Some((start + current,request_end,manifest.size)), "服务器返回了不匹配的 Content-Range");
            ensure!(response.content_length().is_none_or(|n|n == wanted), "分段响应长度不匹配");
            if let (Some(before),Some(after)) = (&manifest.validator,validator(response.headers())) { ensure!(*before == after,"文件在下载期间发生变化"); }
            let mut stream = response.bytes_stream();
            let file = OpenOptions::new().create(true).append(true).open(&path).await?;
            // Coalesce small HTTP frames into 1 MiB disk writes.
            let mut file = BufWriter::with_capacity(1024 * 1024, file);
            let mut written = current;
            let body_result: Result<()> = async {
                loop {
                    let next = tokio::select! { _ = cancel.cancelled() => bail!("已暂停"), n = stream.try_next() => n.map_err(|e|e.without_url())? };
                    let Some(bytes) = next else { break; };
                    ensure!(written + bytes.len() as u64 <= current + wanted, "分段超出预期长度");
                    for part in bytes.chunks(16*1024){
                        app.0.bandwidth.wait(part.len(),cancel).await?;
                        file.write_all(part).await?;written+=part.len() as u64;
                        progress.bytes.fetch_add(part.len() as u64,Ordering::Relaxed);
                        progress.transferred.fetch_add(part.len() as u64,Ordering::Relaxed);
                    }
                }
                Ok(())
            }.await;
            // Tokio file writes may still be in flight when the HTTP stream errors.
            // Flush before releasing this worker or a retry could read a stale length.
            file.flush().await?;
            file.get_ref().sync_data().await?;
            body_result?;
            ensure!(written == current + wanted,"网盘连接提前中断，已保存进度");
            Ok(())
        }.await;
        drop(_permit);
        match result {
            Ok(()) => {
                failures = 0;
                continue;
            }
            Err(error) => {
                let saved = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                if saved.saturating_sub(progress_checkpoint) >= 64 * 1024 {
                    progress_checkpoint = saved;
                    failures = 0;
                } else {
                    failures += 1;
                }
                // Useful progress can continue across a dropped connection; repeated
                // failures without progress remain bounded, and authorization renews the link.
                if error.to_string().contains("已过期") || failures >= 4 {
                    return Err(error.context("网盘连接中断，已有分段已保留，可重试继续下载"));
                }
            }
        }
        tokio::select! { _ = cancel.cancelled() => bail!("已暂停"), _ = tokio::time::sleep(delay) => {} }
    }
    bail!("分段重试次数已耗尽")
}
async fn sequential(
    app: &App,
    link: &Link,
    size: u64,
    path: &Path,
    cancel: &CancellationToken,
    progress: Progress,
) -> Result<()> {
    let _permit = tokio::select! { _ = cancel.cancelled() => bail!("已暂停"), p = app.0.connections.acquire() => p? };
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("已暂停"),
        r = app.0.client.get(link.url.clone()).headers(link.headers.clone()).header(header::ACCEPT_ENCODING,"identity").send() => r.map_err(|e|e.without_url())?,
    };
    ensure!(
        response.status() == StatusCode::OK,
        "单连接下载失败（HTTP {}）",
        response.status().as_u16()
    );
    ensure!(
        response.content_length().is_none_or(|n| n == size),
        "单连接下载长度不匹配"
    );
    let mut output = BufWriter::with_capacity(1024 * 1024, fs::File::create(path).await?);
    progress.bytes.store(0, Ordering::Relaxed);
    let mut stream = response.bytes_stream();
    let mut written = 0;
    loop {
        let next = tokio::select! { _ = cancel.cancelled() => { output.flush().await?; bail!("已暂停"); }, n = stream.try_next() => n.map_err(|e|e.without_url())? };
        let Some(bytes) = next else {
            break;
        };
        ensure!(
            written + bytes.len() as u64 <= size,
            "单连接内容超出预期长度"
        );
        for part in bytes.chunks(16 * 1024) {
            if let Err(error) = app.0.bandwidth.wait(part.len(), cancel).await {
                output.flush().await?;
                return Err(error);
            }
            output.write_all(part).await?;
            written += part.len() as u64;
            progress.bytes.store(written, Ordering::Relaxed);
            progress
                .transferred
                .fetch_add(part.len() as u64, Ordering::Relaxed);
        }
    }
    output.flush().await?;
    output.get_ref().sync_data().await?;
    ensure!(written == size, "单连接下载提前中断");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_range_is_strict() {
        assert_eq!(range("bytes 0-0/100"), Some((0, 0, 100)));
        assert_eq!(range("bytes 23-99/100"), Some((23, 99, 100)));
        assert!(range("bytes */100").is_none());
        assert!(range("bytes 0-99/*").is_none());
        assert!(range("items 0-0/100").is_none());
    }
}
