//! A dedicated connector exists only while browser OAuth sessions are active.
use anyhow::{Context, Result, bail};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct OAuthTunnel {
    config: Option<PathBuf>,
    binary: PathBuf,
    state: Arc<Mutex<State>>,
}
#[derive(Default)]
struct State {
    child: Option<Child>,
    sessions: BTreeMap<Uuid, u64>,
}
impl OAuthTunnel {
    pub fn new() -> Self {
        Self {
            config: std::env::var_os("OAUTH_TUNNEL_CONFIG")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            binary: PathBuf::from("cloudflared"),
            state: Arc::new(Mutex::new(State::default())),
        }
    }
    pub fn enabled(&self) -> bool {
        self.config.is_some()
    }
    pub async fn acquire(&self, id: Uuid, expires: u64) -> Result<()> {
        let Some(config) = &self.config else {
            return Ok(());
        };
        let mut state = self.state.lock().await;
        state.sessions.retain(|_, expiry| *expiry > crate::now());
        if let Some(child) = state.child.as_mut()
            && child.try_wait()?.is_some()
        {
            state.child = None;
        }
        if state.child.is_none() {
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let metrics = socket.local_addr()?;
            drop(socket);
            let mut child = Command::new(&self.binary)
                .arg("tunnel")
                .arg("--no-autoupdate")
                .arg("--config")
                .arg(config)
                .arg("--metrics")
                .arg(metrics.to_string())
                .arg("run")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("无法启动临时授权隧道，请检查 cloudflared 和隧道配置")?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(500))
                .build()?;
            let ready = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if child.try_wait()?.is_some() {
                        bail!("授权隧道启动失败，请检查隧道凭据和网络");
                    }
                    if client
                        .get(format!("http://{metrics}/ready"))
                        .send()
                        .await
                        .is_ok_and(|r| r.status().is_success())
                    {
                        break Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
            .await;
            match ready {
                Ok(Ok(())) => {}
                failure => {
                    let _ = child.kill().await;
                    match failure {
                        Ok(Err(error)) => return Err(error),
                        _ => bail!("授权隧道连接超时，请检查服务器到 Cloudflare 的网络"),
                    }
                }
            }
            state.child = Some(child);
            eprintln!("临时授权隧道已连接");
        }
        state.sessions.insert(id, expires);
        let tunnel = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(expires.saturating_sub(crate::now()))).await;
            tunnel.cancel(id).await;
        });
        Ok(())
    }
    pub async fn cancel(&self, id: Uuid) {
        let mut state = self.state.lock().await;
        state.sessions.remove(&id);
        if state.sessions.is_empty()
            && let Some(mut child) = state.child.take()
        {
            let _ = child.kill().await;
            eprintln!("临时授权隧道已关闭");
        }
    }
    pub fn finish(&self, id: Uuid) {
        if !self.enabled() {
            return;
        }
        // Let the completed callback load its tiny self-closing script first.
        let tunnel = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            tunnel.cancel(id).await;
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn connector_leases_close_on_finish_cancel_and_timeout() {
        let root = std::env::temp_dir().join(format!("flowdisk-oauth-tunnel-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let binary = root.join("cloudflared");
        std::fs::write(
            &binary,
            r#"#!/usr/bin/env python3
import sys
from http.server import BaseHTTPRequestHandler,HTTPServer
host,port=sys.argv[sys.argv.index('--metrics')+1].split(':')
class Ready(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):self.send_response(200);self.end_headers()
HTTPServer((host,int(port)),Ready).serve_forever()
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tunnel = OAuthTunnel {
            config: Some(root.join("config.yml")),
            binary,
            state: Arc::new(Mutex::new(State::default())),
        };
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert!(tunnel.state.lock().await.child.is_none());
        tunnel.acquire(first, crate::now() + 30).await.unwrap();
        let pid = tunnel.state.lock().await.child.as_ref().unwrap().id();
        tunnel.acquire(second, crate::now() + 30).await.unwrap();
        assert_eq!(pid, tunnel.state.lock().await.child.as_ref().unwrap().id());
        tunnel.cancel(first).await;
        assert!(tunnel.state.lock().await.child.is_some());
        tunnel.finish(second);
        tokio::time::sleep(Duration::from_millis(2250)).await;
        assert!(tunnel.state.lock().await.child.is_none());
        let expires = Uuid::new_v4();
        tunnel.acquire(expires, crate::now() + 1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(tunnel.state.lock().await.child.is_none());
        let cancel = Uuid::new_v4();
        tunnel.acquire(cancel, crate::now() + 30).await.unwrap();
        tunnel.cancel(cancel).await;
        assert!(tunnel.state.lock().await.child.is_none());
        let broken = OAuthTunnel {
            config: Some(root.join("config.yml")),
            binary: root.join("missing"),
            state: Arc::new(Mutex::new(State::default())),
        };
        assert!(
            broken
                .acquire(Uuid::new_v4(), crate::now() + 30)
                .await
                .is_err()
        );
        assert!(broken.state.lock().await.child.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
