//! 远程图片异步加载器。
//!
//! 安全边界（远程 URL 来自邮件正文，按不可信输入处理）：
//! - 仅 `http` / `https`，只允许 80 / 443 / 8080 / 8443 端口；
//! - 拒绝本机名、`.local` / `.lan` / `.internal`，拒绝回环、私网、链路本地、CGNAT 地址；
//! - 发送前解析 DNS 并检查全部地址，防止直接访问内网；
//! - 不发送 Cookie / Referer / Authorization，仅 `User-Agent` 与 `Accept: image/*`；
//! - 单张 ≤ 8 MB，20 秒超时，最多 3 次重定向，重定向目标同样做 URL 检查；
//! - 会话内缓存 64 MB 上限，超出后清理已完成的条目。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const MAX_CONCURRENT: usize = 4;

#[derive(Clone)]
pub enum RemoteImage {
    Loading,
    Ready(Arc<Vec<u8>>),
    Failed,
}

#[derive(Clone)]
pub struct RemoteImageLoader {
    state: Arc<Mutex<HashMap<String, RemoteImage>>>,
    inflight: Arc<Mutex<HashSet<String>>>,
    bytes_used: Arc<AtomicUsize>,
    client: Option<Arc<reqwest::Client>>,
    handle: Handle,
    wake: Arc<dyn Fn() + Send + Sync>,
    permits: Arc<Semaphore>,
}

impl RemoteImageLoader {
    pub fn new(handle: Handle, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("MyEmail/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(8))
            // 手动处理重定向：每一跳都重新做 URL 与 DNS 检查。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()
            .map(Arc::new);

        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            bytes_used: Arc::new(AtomicUsize::new(0)),
            client,
            handle,
            wake,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        }
    }

    pub fn lookup(&self, url: &str) -> Option<RemoteImage> {
        self.state
            .lock()
            .expect("image cache poisoned")
            .get(url)
            .cloned()
    }

    /// 幂等：已请求 / 已完成的 URL 不会重复抓取。
    pub fn request(&self, url: &str) {
        if !is_allowed_url_str(url) {
            self.state
                .lock()
                .expect("image cache poisoned")
                .insert(url.to_string(), RemoteImage::Failed);
            (self.wake)();
            return;
        }
        {
            let mut inflight = self.inflight.lock().expect("image inflight poisoned");
            if !inflight.insert(url.to_string()) {
                return;
            }
        }
        {
            let mut state = self.state.lock().expect("image cache poisoned");
            if state.contains_key(url) {
                self.inflight
                    .lock()
                    .expect("image inflight poisoned")
                    .remove(url);
                return;
            }
            state.insert(url.to_string(), RemoteImage::Loading);
        }

        let Some(client) = self.client.clone() else {
            self.finish(url, RemoteImage::Failed);
            return;
        };
        let url = url.to_string();
        let state = self.state.clone();
        let inflight = self.inflight.clone();
        let bytes_used = self.bytes_used.clone();
        let wake = self.wake.clone();
        let permits = self.permits.clone();

        self.handle.spawn(async move {
            let outcome = {
                let _permit = permits.acquire_owned().await;
                fetch(&client, &url).await
            };
            let value = match outcome {
                Ok(bytes) => {
                    let used = bytes_used.fetch_add(bytes.len(), Ordering::Relaxed) + bytes.len();
                    if used > MAX_CACHE_BYTES {
                        trim_cache(&state, &bytes_used);
                    }
                    RemoteImage::Ready(Arc::new(bytes))
                }
                Err(err) => {
                    tracing::debug!(url = %sanitize_url(&url), %err, "remote image fetch failed");
                    RemoteImage::Failed
                }
            };
            state
                .lock()
                .expect("image cache poisoned")
                .insert(url.clone(), value);
            inflight
                .lock()
                .expect("image inflight poisoned")
                .remove(&url);
            (wake)();
        });
    }

    fn finish(&self, url: &str, value: RemoteImage) {
        self.state
            .lock()
            .expect("image cache poisoned")
            .insert(url.to_string(), value);
        self.inflight
            .lock()
            .expect("image inflight poisoned")
            .remove(url);
        (self.wake)();
    }
}

fn trim_cache(state: &Arc<Mutex<HashMap<String, RemoteImage>>>, bytes_used: &Arc<AtomicUsize>) {
    let mut map = state.lock().expect("image cache poisoned");
    map.retain(|_, value| matches!(value, RemoteImage::Loading));
    bytes_used.store(0, Ordering::Relaxed);
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        check_resolved_host(&current).await?;
        let response = client
            .get(&current)
            .header(reqwest::header::ACCEPT, "image/*")
            .send()
            .await
            .map_err(|err| err.to_string())?;

        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "重定向缺少 Location".to_string())?;
            let base = reqwest::Url::parse(&current).map_err(|err| err.to_string())?;
            let next = base.join(location).map_err(|err| err.to_string())?;
            if !is_allowed_url(&next) {
                return Err("重定向目标被拒绝".into());
            }
            current = next.to_string();
            continue;
        }

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        if let Some(length) = response.content_length()
            && length as usize > MAX_IMAGE_BYTES
        {
            return Err("图片超过大小上限".into());
        }
        if let Some(content_type) = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            && !content_type.to_ascii_lowercase().starts_with("image/")
        {
            return Err(format!("非图片 Content-Type: {content_type}"));
        }

        let mut stream = response.bytes_stream();
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| err.to_string())?;
            if out.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
                return Err("图片超过大小上限".into());
            }
            out.extend_from_slice(&chunk);
        }
        if out.is_empty() {
            return Err("空响应".into());
        }
        return Ok(out);
    }
    Err("重定向次数过多".into())
}

/// DNS 预解析：任一解析结果落在内网就拒绝。实际连接仍用域名（存在理论上的
/// DNS rebinding 窗口，但结合 URL 主机名/端口/IP 字面量检查已覆盖常见 SSRF）。
async fn check_resolved_host(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|err| err.to_string())?;
    let host = parsed.host_str().ok_or_else(|| "缺少主机名".to_string())?;
    let host_for_ip = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_for_ip.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err("拒绝访问内网地址".into());
        }
        return Ok(());
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| format!("DNS 解析失败: {err}"))?;
    let mut resolved = false;
    for addr in addrs {
        resolved = true;
        if is_private_ip(addr.ip()) {
            return Err("拒绝访问内网地址".into());
        }
    }
    if !resolved {
        return Err("DNS 无结果".into());
    }
    Ok(())
}

pub fn is_allowed_url(url: &reqwest::Url) -> bool {
    is_allowed_parts(
        url.scheme(),
        url.host_str(),
        url.port_or_known_default(),
        url.username(),
    )
}

pub fn is_allowed_url_str(url: &str) -> bool {
    reqwest::Url::parse(url)
        .map(|parsed| is_allowed_url(&parsed))
        .unwrap_or(false)
}

fn is_allowed_parts(scheme: &str, host: Option<&str>, port: Option<u16>, username: &str) -> bool {
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    if !username.is_empty() {
        return false;
    }
    if !matches!(port, Some(80) | Some(443) | Some(8080) | Some(8443)) {
        return false;
    }
    let Some(host) = host else { return false };
    let host = host.to_ascii_lowercase();
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".lan")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa")
    {
        return false;
    }
    let host_for_ip = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_for_ip.parse::<IpAddr>() {
        return !is_private_ip(ip);
    }
    true
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_private_v4(ip),
        IpAddr::V6(ip) => is_private_v6(ip),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_private_v4(mapped);
    }
    ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local() || ip.is_unicast_link_local()
}

fn sanitize_url(url: &str) -> String {
    // 只保留 scheme://host，避免把查询串（可能含追踪 token）写进日志。
    reqwest::Url::parse(url)
        .map(|parsed| {
            let host = parsed.host_str().unwrap_or("?");
            format!("{}://{host}", parsed.scheme())
        })
        .unwrap_or_else(|_| "<invalid>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(url: &str) -> bool {
        is_allowed_url_str(url)
    }

    #[test]
    fn rejects_private_and_local_targets() {
        assert!(!allowed("http://127.0.0.1/a.png"));
        assert!(!allowed("http://10.0.0.2/a.png"));
        assert!(!allowed("http://192.168.1.1/a.png"));
        assert!(!allowed("http://172.16.0.1/a.png"));
        assert!(!allowed("http://169.254.169.254/latest/meta-data"));
        assert!(!allowed("http://localhost/a.png"));
        assert!(!allowed("http://printer.local/a.png"));
        assert!(!allowed("http://[::1]/a.png"));
        assert!(!allowed("http://[fd00::1]/a.png"));
    }

    #[test]
    fn rejects_unsafe_schemes_ports_and_credentials() {
        assert!(!allowed("file:///C:/secret.png"));
        assert!(!allowed("ftp://example.com/a.png"));
        assert!(!allowed("http://example.com:22/a.png"));
        assert!(!allowed("http://user:pass@example.com/a.png"));
        assert!(!allowed("javascript:alert(1)"));
    }

    #[test]
    fn allows_public_http_and_https() {
        assert!(allowed("https://example.com/a.png"));
        assert!(allowed("http://cdn.example.com:8080/a.png"));
        assert!(allowed("https://8.8.8.8/logo.png"));
    }

    #[tokio::test]
    #[ignore = "requires network access"]
    async fn remote_fetch_smoke() {
        let loader = RemoteImageLoader::new(tokio::runtime::Handle::current(), Arc::new(|| {}));
        let url = "https://httpbin.org/image/png";
        loader.request(url);
        for _ in 0..100 {
            match loader.lookup(url) {
                Some(RemoteImage::Ready(bytes)) => {
                    assert!(bytes.len() > 100);
                    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
                    return;
                }
                Some(RemoteImage::Failed) => panic!("fetch failed"),
                _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
        panic!("timed out waiting for remote image");
    }

    #[test]
    fn cache_trims_completed_entries() {
        let state = Arc::new(Mutex::new(HashMap::new()));
        state
            .lock()
            .unwrap()
            .insert("a".into(), RemoteImage::Ready(Arc::new(vec![1])));
        state
            .lock()
            .unwrap()
            .insert("b".into(), RemoteImage::Loading);
        let used = Arc::new(AtomicUsize::new(10));
        trim_cache(&state, &used);
        let map = state.lock().unwrap();
        assert!(map.get("a").is_none());
        assert!(matches!(map.get("b"), Some(RemoteImage::Loading)));
        assert_eq!(used.load(Ordering::Relaxed), 0);
    }
}
