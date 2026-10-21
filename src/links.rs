use crate::{ApiResult, App, bad};
use axum::{Json, Router, extract::State, routing::post};
use futures_util::TryStreamExt;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Inspect {
    url: String,
}
fn valid(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}
fn extract(raw: &str, url: &Url) -> Vec<Value> {
    let html = scraper::Html::parse_document(raw);
    let base_selector = scraper::Selector::parse("base[href]").unwrap();
    let base = html
        .select(&base_selector)
        .next()
        .and_then(|e| e.value().attr("href"))
        .and_then(|href| url.join(href).ok())
        .filter(valid)
        .unwrap_or_else(|| url.clone());
    let selector = scraper::Selector::parse("a[href],video[src],audio[src],source[src]").unwrap();
    let mut seen = BTreeSet::new();
    let mut links = Vec::new();
    for element in html.select(&selector) {
        let Some(href) = element
            .value()
            .attr("href")
            .or_else(|| element.value().attr("src"))
        else {
            continue;
        };
        let Ok(mut target) = base.join(href) else {
            continue;
        };
        target.set_fragment(None);
        if !(valid(&target)
            || target.scheme() == "magnet"
            || matches!(target.scheme(), "ftp" | "sftp"))
            || !target.username().is_empty()
            || target.password().is_some()
        {
            continue;
        }
        if !seen.insert(target.to_string()) {
            continue;
        }
        let title = element
            .value()
            .attr("download")
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                element
                    .text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        links.push(json!({"url":target.to_string(),"name":title.chars().take(200).collect::<String>(),"download_hint":element.value().attr("download").is_some(),"kind":target.scheme()}));
        if links.len() >= 500 {
            break;
        }
    }
    links
}
async fn inspect(State(app): State<App>, Json(input): Json<Inspect>) -> ApiResult<Json<Value>> {
    let url = Url::parse(input.url.trim()).map_err(|_| bad("请输入普通 HTTP/HTTPS 网页地址"))?;
    if !valid(&url) || input.url.len() > 16384 {
        return Err(bad("网页地址无效"));
    }
    let response = app
        .0
        .client
        .get(url.clone())
        .send()
        .await
        .map_err(|_| bad("网页读取失败，请检查链接和服务器网络"))?;
    if !response.status().is_success() {
        return Err(bad("网页拒绝访问，请检查链接权限"));
    }
    let final_url = response.url().clone();
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let size = response.content_length();
    if !mime.contains("text/html") && !mime.contains("application/xhtml+xml") {
        return Ok(Json(
            json!({"direct":true,"size":size,"content_type":mime,"links":[{"url":final_url.to_string(),"name":final_url.path_segments().and_then(|mut p|p.next_back()).unwrap_or("download"),"download_hint":true,"kind":final_url.scheme()}]}),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|_| bad("网页内容读取中断"))?
    {
        if body.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(bad("网页超过 2 MiB，请直接填写其中的下载链接"));
        }
        body.extend_from_slice(&chunk);
    }
    let raw = String::from_utf8_lossy(&body);
    let links = extract(&raw, &final_url);
    Ok(Json(
        json!({"direct":false,"links":links,"message":"仅提取静态页面链接，不执行脚本；请勾选真正的文件链接。"}),
    ))
}
pub fn routes() -> Router<App> {
    Router::new().route("/links/inspect", post(inspect))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_links_respect_base_decode_entities_and_skip_scripts() {
        let u = Url::parse("https://example.test/page/").unwrap();
        let v = extract(
            r#"<base href="/files/"><a href="one.zip?a=1&amp;b=2" download="one.zip">file</a><a href="javascript:alert(1)">bad</a><a href="one.zip?a=1&amp;b=2">duplicate</a><video src="movie.mp4"></video>"#,
            &u,
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["url"], "https://example.test/files/one.zip?a=1&b=2");
        assert_eq!(v[0]["download_hint"], true);
    }
}
