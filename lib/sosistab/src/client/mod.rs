use tcp::TcpClientBackhaul;

use crate::*;

use std::{net::SocketAddr, sync::Arc};

mod inner;

/// Connects to a remote server over UDP.
pub async fn connect_udp(
    server_addr: SocketAddr,
    pubkey: x25519_dalek::PublicKey,
) -> std::io::Result<Session> {
    inner::connect_custom(inner::ClientConfig {
        server_addr,
        server_pubkey: pubkey,
        backhaul_gen: Arc::new(|| {
            Arc::new(smol::future::block_on(runtime::new_udp_socket_bind("0.0.0.0:0")).unwrap())
        }),
        num_shards: 8,
        reset_interval: Some(Duration::from_secs(20)),
    })
    .await
}

/// Connects to a remote server over UDP.
pub async fn connect_tcp(
    server_addr: SocketAddr,
    pubkey: x25519_dalek::PublicKey,
) -> std::io::Result<Session> {
    inner::connect_custom(inner::ClientConfig {
        server_addr,
        server_pubkey: pubkey,
        backhaul_gen: Arc::new(move || {
            Arc::new(TcpClientBackhaul::new().add_remote_key(server_addr, pubkey))
        }),
        num_shards: 16,
        reset_interval: None,
    })
    .await
}
