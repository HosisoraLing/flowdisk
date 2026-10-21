use crate::{ApiError, ApiResult, bad};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{io::SeekFrom, path::Path};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
};
use tokio_util::io::ReaderStream;

pub(crate) fn byte_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if size == 0 || value.contains(',') {
        return None;
    }
    if start.is_empty() {
        let length = end.parse::<u64>().ok()?;
        return (length > 0).then_some((size.saturating_sub(length), size - 1));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end && start < size).then_some((start, end))
}
pub async fn send(path: &Path, name: &str, request: &HeaderMap, head: bool) -> ApiResult<Response> {
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "本地文件已被移走".into()))?;
    if !metadata.file_type().is_file() {
        return Err(bad("下载目标不是普通文件"));
    }
    let mut file = fs::File::open(path).await?;
    let meta = file.metadata().await?;
    let size = meta.len();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default()
        .as_nanos();
    let etag = format!("\"{size:x}-{modified:x}\"");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    headers.insert(header::ETAG, etag.parse().unwrap());
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=download; filename*=UTF-8''{encoded}")
            .parse()
            .unwrap(),
    );
    let range = request.get(header::RANGE).filter(|_| {
        !head
            && request
                .get(header::IF_RANGE)
                .is_none_or(|v| v.as_bytes() == etag.as_bytes())
    });
    let (status, start, length) = if let Some(range) = range {
        match range.to_str().ok().and_then(|v| byte_range(v, size)) {
            Some((start, end)) => {
                headers.insert(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{size}").parse().unwrap(),
                );
                (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
            }
            None => {
                headers.insert(
                    header::CONTENT_RANGE,
                    format!("bytes */{size}").parse().unwrap(),
                );
                headers.insert(header::CONTENT_LENGTH, "0".parse().unwrap());
                return Ok(
                    (StatusCode::RANGE_NOT_SATISFIABLE, headers, Body::empty()).into_response()
                );
            }
        }
    } else {
        (StatusCode::OK, 0, size)
    };
    headers.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
    file.seek(SeekFrom::Start(start)).await?;
    let body = if head {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(length)))
    };
    Ok((status, headers, body).into_response())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_are_bounded() {
        assert_eq!(byte_range("bytes=3-", 10), Some((3, 9)));
        assert_eq!(byte_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(byte_range("bytes=0-99", 10), Some((0, 9)));
        for value in [
            "bytes=10-",
            "bytes=5-2",
            "bytes=0-1,3-4",
            "bytes=-0",
            "bytes=abc-2",
        ] {
            assert_eq!(byte_range(value, 10), None);
        }
        assert_eq!(byte_range("bytes=0-0", 0), None);
    }
}
