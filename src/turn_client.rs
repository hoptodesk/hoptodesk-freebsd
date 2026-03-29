use futures::SinkExt;
use hbb_common::{
    bail, config::Config, lazy_static, log, socket_client,
    tcp::FramedStream,
    tokio::{self, net::TcpStream, sync::mpsc, time::timeout},
    ResultType,
};
use std::sync::Mutex;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use hbb_common::proxy;
use tokio_rustls::rustls::{self, ClientConfig as TlsClientConfig, OwnedTrustAnchor};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use turn::client::{tcp::TcpTurn, ClientConfig, TlsConfig};
use webrtc_util::conn::Conn;

use crate::rendezvous_messages::{self, ToJson};

lazy_static::lazy_static! {
    static ref PUBLIC_IP: Arc<Mutex<Option<(IpAddr, SocketAddr, Instant)>>> = Default::default();
    static ref TURN_SERVERS_CACHE: Arc<Mutex<Option<(Vec<TurnConfig>, Instant)>>> = Default::default();
}

#[derive(Debug, Clone)]
pub struct TurnConfig {
    addr: String,
    username: String,
    password: String,
    tls_config: Option<TlsConfig>,
}

pub struct ServerConfigs {
    pub turn_servers: Vec<TurnConfig>,
}

async fn get_server_configs() -> Option<ServerConfigs> {
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject.as_ref().to_vec(),
            ta.subject_public_key_info.as_ref().to_vec(),
            ta.name_constraints
                .as_ref()
                .map(|nc| nc.as_ref().to_vec()),
        )
    }));
    let tls_config = Arc::new(
        TlsClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth(),
    );
    let map = hbb_common::api::call_api().await.ok()?;
    let mut turn_servers = Vec::new();
    let mut seen_addrs = std::collections::HashSet::new();

    // Check if turnservers exists, if not add a dummy server
    if let Some(turnservers) = map["turnservers"].as_array() {
        for server in turnservers {
            let host = server["host"].as_str()?;
            let port = server["port"].as_str()?;
            let addr = if host.contains(':') {
                // IPv6 address: wrap in brackets for valid socket address
                format!("[{}]:{}", host, port)
            } else {
                format!("{}:{}", host, port)
            };

			if seen_addrs.contains(&addr) {
				log::warn!("[turn] Skipping duplicate server");
				continue;
			}

            seen_addrs.insert(addr.clone());

            if server["protocol"].as_str()? == "turn" {
                turn_servers.push(TurnConfig {
                    addr,
                    username: server["username"].as_str()?.to_string(),
                    password: server["password"].as_str()?.to_string(),
                    tls_config: None,
                });
            } else if server["protocol"].as_str()? == "turn-tls" {
                turn_servers.push(TurnConfig {
                    addr: addr.clone(),
                    username: server["username"].as_str()?.to_string(),
                    password: server["password"].as_str()?.to_string(),
                    tls_config: Some(TlsConfig {
                        client_config: tls_config.clone(),
                        domain: server["host"].as_str()?.try_into().unwrap(),
                    }),
                });
            }
        }
    } else {
        turn_servers.push(TurnConfig {
            addr: "127.0.0.1:3478".to_string(),
            username: "".to_string(),
            password: "".to_string(),
            tls_config: None,
        });
    }

    if !turn_servers.is_empty() {
        log::info!("[turn] Loaded {} TURN servers", turn_servers.len());
    }
    Some(ServerConfigs { turn_servers })
}

async fn get_turn_servers() -> Option<Vec<TurnConfig>> {
    {
        let cached = TURN_SERVERS_CACHE.lock().unwrap();
        if let Some((servers, cached_at)) = cached.as_ref() {
            if cached_at.elapsed() < Duration::from_secs(60) {
                return Some(servers.clone());
            }
        }
    }

    for attempt in 0..3u64 {
        if attempt > 0 {
            log::info!("[turn] Retry attempt {} to fetch TURN servers", attempt);
            tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
        }
        if let Some(configs) = get_server_configs().await {
            if !configs.turn_servers.is_empty() {
                let mut cached = TURN_SERVERS_CACHE.lock().unwrap();
                *cached = Some((configs.turn_servers.clone(), Instant::now()));
                return Some(configs.turn_servers);
            }
        }
    }
    log::warn!("[turn] Failed to fetch TURN servers after 3 attempts");
    None
}

pub async fn connect_over_turn_servers(
    peer_id: &str,
    peer_addr: SocketAddr,
    sender: Arc<tokio::sync::Mutex<crate::client::WsSender>>,
) -> ResultType<(Arc<impl Conn>, FramedStream)> {
    let turn_servers = match get_turn_servers().await {
        Some(servers) => servers,
        None => bail!("empty turn servers!"),
    };
    let srv_len = turn_servers.len();
    let (tx, mut rx) = mpsc::channel(srv_len);
    let mut handles = Vec::new();
	
    for config in turn_servers {
        let sender = sender.clone();
        let peer_id = peer_id.to_owned();
        let tx = tx.clone();
        let handle = tokio::spawn(async move {
            let turn_server = config.addr.clone();
            let truncated_ip = turn_server.split('.').take(3).collect::<Vec<&str>>().join(".");
            log::info!("[turn] start establishing over TURN server: {}", truncated_ip);
            
            let conn = match timeout(
                tokio::time::Duration::from_secs(7),
                create_relay_connection(config, &peer_id, peer_addr, sender.clone()),
            )
            .await
            {
                Ok(Some(conn)) => {
                    log::info!("[turn] established over TURN server: {}", truncated_ip);
                    Some(conn)
                }
                Ok(None) => {
                    log::warn!("[turn] didn't establish over TURN server: {}", truncated_ip);
                    None
                }
                Err(_) => {
                    log::warn!("[turn] timeout establishing over TURN server: {}", truncated_ip);
                    None
                }
            };
            
            if tx.send(conn).await.is_err() {
                log::warn!("failed to send result to channel: {}", truncated_ip);
            }
        });
        handles.push(handle);
    }
    
    drop(tx); // drop tx to end the channel
    
    // Wait for ANY successful connection, not the first completion
    let mut completed_tasks = 0;
    let mut all_results = Vec::new();
    
    while let Some(ret) = rx.recv().await {
        completed_tasks += 1;
        
        // If we got a successful connection, use it immediately
        if let Some(success) = ret {
            // Cancel all remaining tasks
            for handle in handles {
                handle.abort();
            }
            log::info!("[turn] successfully established connection over a TURN server");
            return Ok(success);
        }
        
        all_results.push(ret);
        
        // Only give up after ALL tasks have completed AND all failed
        if completed_tasks >= srv_len {
            break;
        }
    }
    
    // Wait for any remaining tasks to complete (cleanup)
    for handle in handles {
        let _ = handle.await;
    }
    
    bail!("Failed to connect via relay server: all {} candidates failed!", srv_len)
}


async fn create_relay_connection(
    config: TurnConfig,
    peer_id: &str,
    peer_addr: SocketAddr,
    sender: Arc<tokio::sync::Mutex<crate::client::WsSender>>,
) -> Option<(Arc<impl Conn>, FramedStream)> {
    if let Ok(turn_client) = TurnClient::new(config).await {

		match turn_client.create_relay_connection(peer_addr).await {
            Ok(relay) => {
                let conn = relay.0;
                let relay_addr = relay.1;
				if let Ok(stream) =
                    establish_over_relay(&peer_id, turn_client, relay_addr, sender).await
                {
                    return Some((conn, stream));
                }
            }
            Err(err) => log::warn!("create relay conn failed: {}", err),
        }
    }
    return None;
}

async fn establish_over_relay(
    peer_id: &str,
    turn_client: TurnClient,
    relay_addr: SocketAddr,
    sender: Arc<tokio::sync::Mutex<crate::client::WsSender>>,
) -> ResultType<FramedStream> {
    let mut sender = sender.lock().await;
    sender
        .send(WsMessage::Text(
            rendezvous_messages::RelayConnection::new(peer_id, relay_addr).to_json(),
        ))
        .await?;
    match turn_client.wait_new_connection().await {
        Ok(stream) => {
            sender
                .send(WsMessage::Text(
                    rendezvous_messages::RelayReady::new(peer_id).to_json(),
                ))
                .await?;
            let _ = sender.close().await; // close after established
            return Ok(stream);
        }
        Err(e) => bail!("Failed to connect via relay server: {}", e),
    }
}

pub async fn get_public_ip() -> Option<SocketAddr> {
    {
        let mut cached = PUBLIC_IP.lock().unwrap();
        if let Some((cached_local_ip, public_ip, cached_at)) = *cached {
            //  Time since cached is in 10 minutes.
            if cached_at.elapsed() < Duration::from_secs(600) {
                let local_ip = socket_client::get_lan_ipv4().ok()?;
                if cached_local_ip == local_ip {
                    log::info!("Got public ip from cache");
                    return Some(public_ip);
                }
            }
        }
        *cached = None;
    }

    if let Some(turn_servers) = get_turn_servers().await {
        let len = turn_servers.len();
        let (tx, mut rx) = tokio::sync::mpsc::channel(len);
        for config in turn_servers {
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Ok(turn_client) = TurnClient::new(config).await {
                    if let Ok(addr) = turn_client.get_public_ip().await {
                        let _ = tx.send(Some(addr)).await;
                        return;
                    }
                }
                let _ = tx.send(None).await;
            });
        }
        for _ in 0..len {
            if let Some(Some(addr)) = rx.recv().await {
                if let Ok(local_ip) = socket_client::get_lan_ipv4() {
                    let mut cached = PUBLIC_IP.lock().unwrap();
                    *cached = Some((local_ip, addr, Instant::now()));
                }
                return Some(addr);
            }
        }
        log::warn!("[turn] All TURN servers failed for public IP discovery");
    }

    None
}

pub struct TurnClient {
    client: turn::client::Client,
    local_addr: SocketAddr,
}

impl TurnClient {
    pub async fn new(config: TurnConfig) -> ResultType<Self> {
        // Connect to TURN server - either directly or through proxy
        let stream = if let Some(conf) = Config::get_socks() {
            log::info!("[turn] Connecting to TURN server via proxy: {}", config.addr);
            proxy::connect_via_proxy(&conf, &config.addr, 10_000).await?
        } else {
            TcpStream::connect(&config.addr).await?
        };
        let local_addr = stream.local_addr()?;
        // Resolve the TURN server address to fix peer_addr when proxied
        // (into_inner() makes peer_addr point to the SOCKS proxy, not the target)
        let turn_server_addr: SocketAddr = config.addr.parse().or_else(|_| {
            use std::net::ToSocketAddrs;
            config.addr.to_socket_addrs()
                .map(|mut addrs| addrs.next().unwrap())
        })?;
        let mut tcp_turn = if let Some(tls) = config.tls_config.as_ref() {
            TcpTurn::new_tls(tls.client_config.clone(), stream, tls.domain.clone()).await?
        } else {
            TcpTurn::from(stream)
        };
        if Config::get_socks().is_some() {
            tcp_turn.set_peer_addr(turn_server_addr);
        }
        // Pass proxy config to TURN client for sub-connections (CONNECTION_ATTEMPT)
        let socks5_proxy = Config::get_socks().map(|conf| {
            let resolved_type = match conf.proxy_type {
                hbb_common::config::ProxyType::Http => turn::client::ProxyType::Http,
                hbb_common::config::ProxyType::Socks5 => turn::client::ProxyType::Socks5,
                hbb_common::config::ProxyType::Auto => {
                    match proxy::get_resolved_proxy_type() {
                        Some(hbb_common::config::ProxyType::Http) => turn::client::ProxyType::Http,
                        _ => turn::client::ProxyType::Socks5,
                    }
                }
            };
            turn::client::ProxyConfig {
                proxy: conf.proxy,
                username: conf.username,
                password: conf.password,
                proxy_type: resolved_type,
            }
        });
        let mut client = turn::client::Client::new(ClientConfig {
            stun_serv_addr: config.addr.clone(),
            turn_serv_addr: config.addr,
            username: config.username,
            password: config.password,
            realm: String::new(),
            tls_config: config.tls_config,
            software: String::new(),
            rto_in_ms: 0,
            conn: Arc::new(tcp_turn),
            vnet: None,
            socks5_proxy,
        })
        .await?;
        client.listen().await?;
        Ok(Self { client, local_addr })
    }

	pub async fn get_public_ip(&self) -> ResultType<SocketAddr> {
        Ok(self.client.send_binding_request().await?)
    }

    pub async fn create_relay_connection(
        &self,
        peer_addr: SocketAddr,
    ) -> ResultType<(Arc<impl Conn>, SocketAddr)> {
        let relay_connection = self.client.allocate().await?;
		relay_connection.send_to(b"init", peer_addr).await?;
        let local_addr = relay_connection.local_addr()?;
		
        Ok((
            Arc::new(relay_connection),
            local_addr,
        ))
    }

    pub async fn wait_new_connection(&self) -> ResultType<FramedStream> {
        let tcp_stream = self.client.wait_new_connection().await.unwrap();
        Ok(FramedStream::from(tcp_stream, self.local_addr))
    }
}