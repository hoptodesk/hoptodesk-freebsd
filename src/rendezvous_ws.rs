use std::net::ToSocketAddrs;

use hbb_common::tokio::net::TcpStream;
use hbb_common::{
    allow_err,
    anyhow::anyhow,
    bail,
    config::Config,
    log, ResultType,
};
use tokio_tungstenite::Connector::NativeTls;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub(crate) async fn create_websocket_with_peer_id(
    host_list: &str,
    my_peer_id: &str,
) -> ResultType<(
    std::net::IpAddr,
    String,
    WebSocketStream<MaybeTlsStream<TcpStream>>,
)> {
	let hosts = host_list.split(';');
    for host in hosts {
        let ret = create_websocket_(host, Some(my_peer_id.to_owned())).await;
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

    use hbb_common::tokio;
    use std::time::Duration;

    // Establish TCP connection - either directly or through proxy
    let socket = if let Some(conf) = Config::get_socks() {
        log::info!("Connecting to signal server via proxy: {}", host);
        hbb_common::proxy::connect_via_proxy(&conf, host, 10_000)
            .await
            .map_err(|e| anyhow!("Proxy connection failed: {}", e))?
    } else {
        log::info!("Resolving Signal server {}", host);
        let addr = host
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| {
                let error_msg = anyhow!("Cannot resolve dns for the host");
                log::info!("Error: {}", error_msg);
                error_msg
            })?;

        tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await?
            .map_err(|_| anyhow!("TCP connection timed out"))?
    };

    {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            use std::os::unix::io::{FromRawFd, IntoRawFd};
            let raw_fd = socket.as_raw_fd();
            let sock2 = unsafe { hbb_common::socket2::Socket::from_raw_fd(raw_fd) };
            let _ = sock2.set_keepalive(Some(Duration::from_secs(30)));
            let _ = sock2.into_raw_fd(); // release ownership back to TcpStream
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            use std::os::windows::io::{FromRawSocket, IntoRawSocket};
            let raw_socket = socket.as_raw_socket();
            let sock2 = unsafe { hbb_common::socket2::Socket::from_raw_socket(raw_socket) };
            let _ = sock2.set_keepalive(Some(Duration::from_secs(30)));
            let _ = sock2.into_raw_socket(); // release ownership back to TcpStream
        }
    }


	let local_ip = socket.local_addr().unwrap().ip();
    let mut peer_id = Config::get_id();
    if let Some(my_peer_id) = my_peer_id {
        peer_id = my_peer_id
    }
    let scheme = split[0];
	let uri = format!("{}://{}/?user={}", scheme, host, peer_id);
    //Ignore invalid certificate
    let tls_opts = Some(NativeTls(
        native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
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
