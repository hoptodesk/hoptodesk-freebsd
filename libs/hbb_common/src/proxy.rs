use crate::config::{Config, ProxyType, Socks5Server};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

lazy_static::lazy_static! {
    static ref RESOLVED_PROXY_TYPE: Mutex<Option<ProxyType>> = Mutex::new(None);
}

/// Get the cached resolved proxy type (non-async, safe to call anywhere).
pub fn get_resolved_proxy_type() -> Option<ProxyType> {
    *RESOLVED_PROXY_TYPE.lock().unwrap()
}

/// Clear the cached resolved proxy type (call when proxy config changes).
pub fn clear_resolved_proxy_type() {
    *RESOLVED_PROXY_TYPE.lock().unwrap() = None;
}

/// Establish an HTTP CONNECT tunnel through an HTTP proxy.
/// Returns the TcpStream after successful CONNECT handshake.
pub async fn http_connect(
    proxy_addr: &str,
    target_host: &str,
    username: &str,
    password: &str,
) -> crate::ResultType<TcpStream> {
    let stream = TcpStream::connect(proxy_addr).await?;
    stream.set_nodelay(true)?;

    let connect_req = if username.is_empty() {
        format!(
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            target_host, target_host
        )
    } else {
        let credentials =
            sodiumoxide::base64::encode(format!("{}:{}", username, password), sodiumoxide::base64::Variant::Original);
        format!(
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: Basic {}\r\n\r\n",
            target_host, target_host, credentials
        )
    };

    let (mut reader, mut writer) = stream.into_split();
    writer.write_all(connect_req.as_bytes()).await?;

    // Read response - need at least the status line
    let mut buf = vec![0u8; 4096];
    let n = reader.read(&mut buf).await?;
    if n == 0 {
        anyhow::bail!("HTTP CONNECT: proxy closed connection");
    }
    let response = String::from_utf8_lossy(&buf[..n]);

    // Check for 200 status
    let first_line = response.lines().next().unwrap_or("");
    if !first_line.contains("200") {
        anyhow::bail!("HTTP CONNECT failed: {}", first_line);
    }

    // Reunite the split halves back into a TcpStream
    Ok(reader.reunite(writer)?)
}

/// Detect proxy type by trying SOCKS5 handshake first, then HTTP CONNECT.
pub async fn detect_proxy_type(proxy_addr: &str) -> ProxyType {
    // Try SOCKS5: send version greeting (version=5, 1 method, no-auth=0)
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(2), async {
        if let Ok(mut stream) = TcpStream::connect(proxy_addr).await {
            if stream.write_all(&[0x05, 0x01, 0x00]).await.is_ok() {
                let mut buf = [0u8; 2];
                if stream.read_exact(&mut buf).await.is_ok() && buf[0] == 0x05 {
                    return Some(ProxyType::Socks5);
                }
            }
        }
        None
    })
    .await
    {
        if let Some(t) = result {
            return t;
        }
    }

    // Try HTTP: send a minimal CONNECT and look for HTTP response
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(2), async {
        if let Ok(mut stream) = TcpStream::connect(proxy_addr).await {
            let req = "CONNECT 0.0.0.0:0 HTTP/1.1\r\nHost: 0.0.0.0:0\r\n\r\n";
            if stream.write_all(req.as_bytes()).await.is_ok() {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    if n >= 4 && buf.starts_with(b"HTTP") {
                        return Some(ProxyType::Http);
                    }
                }
            }
        }
        None
    })
    .await
    {
        if let Some(t) = result {
            return t;
        }
    }

    // Default to SOCKS5 for backward compatibility
    log::warn!("Proxy auto-detection failed for {}, defaulting to SOCKS5", proxy_addr);
    ProxyType::Socks5
}

/// Resolve Auto proxy type and cache the result.
pub async fn resolve_proxy_type(conf: &Socks5Server) -> ProxyType {
    match conf.proxy_type {
        ProxyType::Socks5 | ProxyType::Http => conf.proxy_type,
        ProxyType::Auto => {
            if let Some(cached) = get_resolved_proxy_type() {
                return cached;
            }
            let detected = detect_proxy_type(&conf.proxy).await;
            *RESOLVED_PROXY_TYPE.lock().unwrap() = Some(detected);
            log::info!("Auto-detected proxy type: {:?}", detected);
            detected
        }
    }
}

/// Connect to a target through the configured proxy (SOCKS5 or HTTP CONNECT).
/// Returns a raw TcpStream tunneled through the proxy.
pub async fn connect_via_proxy(
    conf: &Socks5Server,
    target: &str,
    timeout_ms: u64,
) -> crate::ResultType<TcpStream> {
    let proxy_type = resolve_proxy_type(conf).await;
    match proxy_type {
        ProxyType::Http => {
            tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                http_connect(&conf.proxy, target, &conf.username, &conf.password),
            )
            .await?
        }
        ProxyType::Socks5 | ProxyType::Auto => {
            use tokio_socks::tcp::Socks5Stream;
            let stream = if conf.username.trim().is_empty() {
                tokio::time::timeout(
                    Duration::from_millis(timeout_ms),
                    Socks5Stream::connect(conf.proxy.as_str(), target),
                )
                .await??
            } else {
                tokio::time::timeout(
                    Duration::from_millis(timeout_ms),
                    Socks5Stream::connect_with_password(
                        conf.proxy.as_str(),
                        target,
                        &conf.username,
                        &conf.password,
                    ),
                )
                .await??
            };
            Ok(stream.into_inner())
        }
    }
}

/// Build the proxy URL string for use with reqwest or curl.
pub fn proxy_url(conf: &Socks5Server) -> String {
    let scheme = Config::proxy_scheme(conf);
    if !conf.username.is_empty() {
        format!("{}://{}:{}@{}", scheme, conf.username, conf.password, conf.proxy)
    } else {
        format!("{}://{}", scheme, conf.proxy)
    }
}
