//! Public GitHub releases; payloads use the existing resumable link engine.
use crate::{ApiResult, App, bad};
use axum::{Json, Router, extract::State, routing::post};
use futures_util::TryStreamExt;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Deserialize)]
struct Inspect {
    url: String,
    #[serde(default = "first_page")]
    page: u32,
}
fn first_page() -> u32 {
    1
}
struct Repository {
    owner: String,
    name: String,
    release: String,
}
impl Repository {
    fn parse(raw: &str) -> ApiResult<Self> {
        let url = Url::parse(raw.trim()).map_err(|_| bad("请输入 GitHub 仓库或 Release 地址"))?;
        if raw.len() > 4096
            || url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(bad("请使用 https://github.com 下的公开仓库或 Release 地址"));
        }
        let parts: Vec<_> = url
            .path()
            .trim_end_matches('/')
            .split('/')
            .skip(1)
            .collect();
        let valid = |part: &str| {
            !part.is_empty()
                && part.len() <= 100
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        let name = parts.get(1).copied().unwrap_or("");
        let name = name.strip_suffix(".git").unwrap_or(name);
        if !parts.first().is_some_and(|p| valid(p)) || !valid(name) {
            return Err(bad("请输入包含所有者和仓库名的 GitHub 地址"));
        }
        let release = match parts.get(2..) {
            Some([] | ["releases"]) => String::new(),
            Some(["releases", "latest"]) => "latest".into(),
            Some(["releases", "tag", tag]) if !tag.is_empty() => format!("tags/{tag}"),
            _ => return Err(bad("请粘贴仓库首页、Releases 列表或具体 Release 地址")),
        };
        Ok(Self {
            owner: parts[0].into(),
            name: name.into(),
            release,
        })
    }
    fn api(&self, base: &Url, suffix: &str) -> Url {
        let mut url = base.clone();
        url.set_path(&format!("/repos/{}/{}/{suffix}", self.owner, self.name));
        url
    }
}
fn download_url(raw: &Value, host: &str) -> Option<String> {
    let url = Url::parse(raw.as_str()?).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}
fn release(value: &Value, repo: &Repository) -> Value {
    let tag = value["tag_name"].as_str().unwrap_or("");
    let mut files = Vec::new();
    if let Some(assets) = value["assets"].as_array() {
        for asset in assets {
            if asset["state"] != "uploaded" {
                continue;
            }
            if let (Some(name), Some(url)) = (
                asset["name"].as_str().filter(|n| !n.is_empty()),
                download_url(&asset["browser_download_url"], "github.com"),
            ) {
                files.push(
                    json!({"name":name,"url":url,"size":asset["size"].as_u64(),"source":false}),
                );
            }
        }
    }
    if let Some(url) = download_url(&value["zipball_url"], "api.github.com") {
        files.push(json!({"name":crate::safe_name(&format!("{}-{tag}.zip",repo.name)),"url":url,"size":null,"source":true}));
    }
    json!({"tag":tag,"name":value["name"].as_str().filter(|s|!s.is_empty()).unwrap_or(tag),"prerelease":value["prerelease"]==true,"published_at":value["published_at"],"files":files})
}
async fn read(client: &Client, mut url: Url) -> ApiResult<Value> {
    url.set_fragment(None);
    let response = client
        .get(url)
        .header("User-Agent", "FlowDisk")
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(|_| bad("无法连接 GitHub，请检查服务器网络或代理设置"))?;
    match response.status().as_u16() {
        200 => {}
        403 | 429 => {
            return Err(bad(
                "GitHub API 限流或拒绝访问，请稍后重试；仍可直接下载公开仓库 ZIP",
            ));
        }
        404 => {
            return Err(bad(
                "仓库或 Release 不存在、尚未发布或不是公开内容；请核对链接",
            ));
        }
        _ => return Err(bad("GitHub 暂时无法读取，请稍后重试")),
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|_| bad("GitHub 响应读取中断"))?
    {
        if body.len() + chunk.len() > 4 * 1024 * 1024 {
            return Err(bad("Release 信息过大，请粘贴具体 Release 地址"));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| bad("GitHub 返回了无效的 Release 信息"))
}
async fn inspect_at(client: &Client, input: Inspect, base: &Url) -> ApiResult<Value> {
    let repo = Repository::parse(&input.url)?;
    if input.page == 0 || input.page > 1000 {
        return Err(bad("Release 页码无效"));
    }
    let mut url = repo.api(
        base,
        &format!(
            "releases{}",
            if repo.release.is_empty() {
                String::new()
            } else {
                format!("/{}", repo.release)
            }
        ),
    );
    let listing = repo.release.is_empty();
    if listing {
        url.query_pairs_mut()
            .append_pair("per_page", "20")
            .append_pair("page", &input.page.to_string());
    }
    let value = read(client, url).await?;
    let values = if listing {
        value
            .as_array()
            .ok_or_else(|| bad("GitHub Release 列表无效"))?
            .clone()
    } else {
        if !value["tag_name"].is_string() {
            return Err(bad("GitHub Release 信息无效"));
        }
        vec![value]
    };
    let has_more = listing && values.len() == 20;
    let releases: Vec<_> = values
        .iter()
        .filter(|r| r["draft"] != true)
        .map(|r| release(r, &repo))
        .collect();
    Ok(
        json!({"repository":format!("{}/{}",repo.owner,repo.name),"releases":releases,"page":input.page,"has_more":has_more}),
    )
}
async fn inspect(State(app): State<App>, Json(input): Json<Inspect>) -> ApiResult<Json<Value>> {
    Ok(Json(
        inspect_at(
            &app.0.client,
            input,
            &Url::parse("https://api.github.com").unwrap(),
        )
        .await?,
    ))
}
pub fn routes() -> Router<App> {
    Router::new().route("/github/inspect", post(inspect))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn github_urls_keep_tags_encoded_and_reject_non_repository_pages() {
        let repo = Repository::parse("https://github.com/owner/repo.git/").unwrap();
        assert_eq!(repo.name, "repo");
        let repo =
            Repository::parse("https://github.com/owner/repo/releases/tag/release%2F1").unwrap();
        assert_eq!(
            repo.api(
                &Url::parse("https://api.github.com").unwrap(),
                &format!("releases/{}", repo.release)
            )
            .as_str(),
            "https://api.github.com/repos/owner/repo/releases/tags/release%2F1"
        );
        for raw in [
            "https://github.com.evil.test/o/r",
            "https://user@github.com/o/r",
            "http://github.com/o/r",
            "https://github.com:444/o/r",
            "https://github.com/o",
            "https://github.com/o/r/issues",
            "https://github.com/o/r/releases/download/v1/file.zip",
        ] {
            assert!(Repository::parse(raw).is_err(), "{raw}");
        }
    }
    #[tokio::test]
    async fn public_releases_preserve_assets_sizes_sources_and_api_errors() {
        use axum::{extract::OriginalUri, routing::get};
        async fn fixture(OriginalUri(uri): OriginalUri) -> (axum::http::StatusCode, Json<Value>) {
            let value = json!({"tag_name":"v1","name":"Release 1","prerelease":true,"draft":false,"published_at":"2026-01-01T00:00:00Z","zipball_url":"https://api.github.com/repos/o/r/zipball/v1","assets":[{"name":"app.zip","size":1234,"state":"uploaded","browser_download_url":"https://github.com/o/r/releases/download/v1/app.zip"},{"name":"bad","state":"uploaded","browser_download_url":"http://127.0.0.1/private"}]});
            if uri.path().ends_with("/latest") || uri.path().ends_with("/tags/v1") {
                return (axum::http::StatusCode::OK, Json(value));
            }
            if uri.query().unwrap_or("").split('&').any(|v| v == "page=2") {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(json!({"message":"rate limit"})),
                );
            }
            assert_eq!(uri.path(), "/repos/o/r/releases");
            assert!(uri.query().unwrap().contains("per_page=20"));
            (axum::http::StatusCode::OK, Json(json!(vec![value; 20])))
        }
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", socket.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(socket, Router::new().route("/{*path}", get(fixture)))
                .await
                .unwrap()
        });
        let client = Client::builder().no_proxy().build().unwrap();
        for suffix in ["", "/releases", "/releases/latest", "/releases/tag/v1"] {
            let result = inspect_at(
                &client,
                Inspect {
                    url: format!("https://github.com/o/r{suffix}"),
                    page: 1,
                },
                &base,
            )
            .await
            .unwrap();
            let files = result["releases"][0]["files"].as_array().unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(files[0]["size"], 1234);
            assert_eq!(files[1]["name"], "r-v1.zip");
            assert!(files[1]["size"].is_null());
            assert_eq!(result["releases"][0]["prerelease"], true);
            assert_eq!(
                result["has_more"],
                suffix.is_empty() || suffix == "/releases"
            );
        }
        let error = inspect_at(
            &client,
            Inspect {
                url: "https://github.com/o/r".into(),
                page: 2,
            },
            &base,
        )
        .await
        .err()
        .unwrap();
        assert!(error.1.contains("限流"));
        server.abort();
    }
}
