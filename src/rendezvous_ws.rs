
use hbb_common::tokio::net::TcpStream;
use hbb_common::{
    allow_err,
    anyhow::anyhow,
    bail,
    config::Config,
    lazy_static, log, ResultType,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_tungstenite::Connector::NativeTls;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const DNS_CACHE_TTL: Duration = Duration::from_secs(300);

lazy_static::lazy_static! {
    static ref DNS_CACHE: Mutex<HashMap<String, (Vec<SocketAddr>, Instant)>> = Default::default();
}

async fn resolve_host_cached(host: &str) -> ResultType<(Vec<SocketAddr>, bool)> {
    use hbb_common::tokio;
    if let Some((addrs, resolved_at)) = DNS_CACHE.lock().unwrap().get(host).cloned() {
        if resolved_at.elapsed() < DNS_CACHE_TTL {
            return Ok((addrs, true));
        }
    }
    let addrs: Vec<_> = tokio::net::lookup_host(host).await?.collect();
    if addrs.is_empty() {
        log::info!("Error: Cannot resolve dns for the host");
        bail!("Cannot resolve dns for the host");
    }
    DNS_CACHE
        .lock()
        .unwrap()
        .insert(host.to_owned(), (addrs.clone(), Instant::now()));
    Ok((addrs, false))
}

async fn connect_first(addrs: &[SocketAddr], attempt: usize) -> Option<TcpStream> {
    use hbb_common::tokio;
    let count = addrs.len();
    for i in 0..count {
        let addr = addrs[(attempt + i) % count];
        match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Some(stream),
            Ok(Err(e)) => log::info!("Signal server {} connect failed: {}", addr, e),
            Err(_) => log::info!("Signal server {} connect timed out", addr),
        }
    }
    None
}

const KEEPALIVE_IDLE_SECS: hbb_common::libc::c_int = 30;
const KEEPALIVE_INTVL_SECS: hbb_common::libc::c_int = 10;
#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos", target_os = "ios"))]
const KEEPALIVE_PROBES: hbb_common::libc::c_int = 3;

fn set_aggressive_keepalive(socket: &TcpStream) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = socket.as_raw_fd();
        let on: hbb_common::libc::c_int = 1;
        unsafe {
            hbb_common::libc::setsockopt(
                fd,
                hbb_common::libc::SOL_SOCKET,
                hbb_common::libc::SO_KEEPALIVE,
                &on as *const _ as *const hbb_common::libc::c_void,
                std::mem::size_of_val(&on) as hbb_common::libc::socklen_t,
            );
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            hbb_common::libc::setsockopt(
                fd,
                hbb_common::libc::IPPROTO_TCP,
                hbb_common::libc::TCP_KEEPIDLE,
                &KEEPALIVE_IDLE_SECS as *const _ as *const hbb_common::libc::c_void,
                std::mem::size_of_val(&KEEPALIVE_IDLE_SECS) as hbb_common::libc::socklen_t,
            );
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            hbb_common::libc::setsockopt(
                fd,
                hbb_common::libc::IPPROTO_TCP,
                hbb_common::libc::TCP_KEEPALIVE,
                &KEEPALIVE_IDLE_SECS as *const _ as *const hbb_common::libc::c_void,
                std::mem::size_of_val(&KEEPALIVE_IDLE_SECS) as hbb_common::libc::socklen_t,
            );
            #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos", target_os = "ios"))]
            {
                hbb_common::libc::setsockopt(
                    fd,
                    hbb_common::libc::IPPROTO_TCP,
                    hbb_common::libc::TCP_KEEPINTVL,
                    &KEEPALIVE_INTVL_SECS as *const _ as *const hbb_common::libc::c_void,
                    std::mem::size_of_val(&KEEPALIVE_INTVL_SECS) as hbb_common::libc::socklen_t,
                );
                hbb_common::libc::setsockopt(
                    fd,
                    hbb_common::libc::IPPROTO_TCP,
                    hbb_common::libc::TCP_KEEPCNT,
                    &KEEPALIVE_PROBES as *const _ as *const hbb_common::libc::c_void,
                    std::mem::size_of_val(&KEEPALIVE_PROBES) as hbb_common::libc::socklen_t,
                );
            }
        }
    }
    #[cfg(windows)]
    {
        use std::mem::size_of;
        use std::os::windows::io::AsRawSocket;
        #[repr(C)]
        struct TcpKeepalive {
            onoff: u32,
            keepalivetime: u32,
            keepaliveinterval: u32,
        }
        const SIO_KEEPALIVE_VALS: u32 = 0x98000004;
        extern "system" {
            fn WSAIoctl(
                s: usize,
                dwIoControlCode: u32,
                lpvInBuffer: *const std::ffi::c_void,
                cbInBuffer: u32,
                lpvOutBuffer: *mut std::ffi::c_void,
                cbOutBuffer: u32,
                lpcbBytesReturned: *mut u32,
                lpOverlapped: *mut std::ffi::c_void,
                lpCompletionRoutine: *mut std::ffi::c_void,
            ) -> i32;
        }
        let vals = TcpKeepalive {
            onoff: 1,
            keepalivetime: (KEEPALIVE_IDLE_SECS as u32) * 1000,
            keepaliveinterval: (KEEPALIVE_INTVL_SECS as u32) * 1000,
        };
        let mut bytes_returned: u32 = 0;
        unsafe {
            WSAIoctl(
                socket.as_raw_socket() as usize,
                SIO_KEEPALIVE_VALS,
                &vals as *const _ as *const std::ffi::c_void,
                size_of::<TcpKeepalive>() as u32,
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }
}

pub(crate) async fn create_websocket_with_peer_id(
    host_list: &str,
    my_peer_id: &str,
    attempt: usize,
) -> ResultType<(
    std::net::IpAddr,
    String,
    WebSocketStream<MaybeTlsStream<TcpStream>>,
)> {
	let hosts = host_list.split(';');
    for host in hosts {
        let ret = create_websocket_(host, Some(my_peer_id.to_owned()), attempt).await;
        allow_err!(&ret);

        if ret.is_ok() {
            return ret;
        }
    }

    bail!("Failed to connect any of the hosts in list");
}

pub(crate) async fn create_websocket_(
    host: &str,
    my_peer_id: Option<String>,
    attempt: usize,
) -> ResultType<(
    std::net::IpAddr,
    String,
    WebSocketStream<MaybeTlsStream<TcpStream>>,
)> {
	let mut split = host.split("://").collect::<Vec<&str>>();
    if split.len() < 1 {
        bail!("Uri must contain both scheme and host");
    } else if split.len() == 1 {
        // Use ws by default
        split.insert(0, "ws");
    }

    let host = split[1];

    // Establish TCP connection - either directly or through proxy
    let socket = if let Some(conf) = Config::get_socks() {
        log::info!("Connecting to signal server via proxy: {}", host);
        hbb_common::proxy::connect_via_proxy(&conf, host, 10_000)
            .await
            .map_err(|e| anyhow!("Proxy connection failed: {}", e))?
    } else {
        log::info!("Resolving Signal server {}", host);
        let (addrs, from_cache) = resolve_host_cached(host).await?;
        let mut connected = connect_first(&addrs, attempt).await;
        if connected.is_none() && from_cache {
            DNS_CACHE.lock().unwrap().remove(host);
            let (addrs, _) = resolve_host_cached(host).await?;
            connected = connect_first(&addrs, attempt).await;
        }
        match connected {
            Some(stream) => stream,
            None => bail!("Failed to connect to any resolved address for {}", host),
        }
    };

    set_aggressive_keepalive(&socket);


	let local_ip = socket.local_addr().unwrap().ip();
    let mut peer_id = Config::get_id();
    if let Some(my_peer_id) = my_peer_id {
        peer_id = my_peer_id
    }
    let scheme = split[0];
	let uri = format!("{}://{}/?user={}", scheme, host, peer_id);
    let allow_invalid_certs = !Config::get_option("custom-rendezvous-server").is_empty();
    let tls_opts = Some(NativeTls(
        native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(allow_invalid_certs)
            .danger_accept_invalid_hostnames(allow_invalid_certs)
			.use_sni(false)
            .build()?,
    ));
	log::info!("Connecting to signal server: {}://{}", scheme, host);
    // Use the established TCP connection (direct or proxied) for the WebSocket handshake
    let (websocket, _) = tokio_tungstenite::client_async_tls_with_config(
        &uri, socket, None, tls_opts,
    ).await?;

    log::info!("Websocket connected succesfully");
    return Ok((local_ip, format!("{}://{}", scheme, host), websocket));
}
