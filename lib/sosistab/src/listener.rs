use crate::*;
use crate::{
    recfilter::RECENT_FILTER,
    session::{Session, SessionConfig},
};
use bytes::Bytes;

use governor::{Quota, RateLimiter};
use parking_lot::RwLock;
use protocol::HandshakeFrame::*;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use smol::net::AsyncToSocketAddrs;
use smol::{
    channel::{Receiver, Sender},
    net::TcpListener,
};
use std::net::SocketAddr;
use std::{num::NonZeroU32, sync::Arc};
use table::ShardedAddrs;
use tcp::TcpServerBackhaul;

use self::table::SessionTable;

mod table;

pub struct Listener {
    accepted: Receiver<Session>,
    local_addr: SocketAddr,
    _task: smol::Task<Option<()>>,
}

impl Listener {
    /// Accepts a session. This function must be repeatedly called for the entire Listener to make any progress.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn accept_session(&self) -> Option<Session> {
        self.accepted.recv().await.ok()
    }
    /// Creates a new listener given the parameters.
    pub async fn listen_udp(
        addr: impl AsyncToSocketAddrs,
        long_sk: x25519_dalek::StaticSecret,
        on_recv: impl Fn(usize, SocketAddr) + 'static + Send + Sync,
        on_send: impl Fn(usize, SocketAddr) + 'static + Send + Sync,
    ) -> Self {
        // let addr = async_net::resolve(addr).await;
        let socket = runtime::new_udp_socket_bind(addr).await.unwrap();
        let local_addr = socket.get_ref().local_addr().unwrap();
        let cookie = crypt::Cookie::new((&long_sk).into());
        let (send, recv) = smol::channel::unbounded();
        let task = runtime::spawn_local(
            ListenerActor {
                socket: Arc::new(StatsBackhaul::new(socket, on_recv, on_send)),
                cookie,
                long_sk,
            }
            .run(send),
        );
        Listener {
            accepted: recv,
            local_addr,
            _task: task,
        }
    }

    /// Creates a new listener given the parameters.
    pub async fn listen_tcp(
        addr: impl AsyncToSocketAddrs,
        long_sk: x25519_dalek::StaticSecret,
        on_recv: impl Fn(usize, SocketAddr) + 'static + Send + Sync,
        on_send: impl Fn(usize, SocketAddr) + 'static + Send + Sync,
    ) -> Self {
        // let addr = async_net::resolve(addr).await;
        let listener = TcpListener::bind(addr).await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let cookie = crypt::Cookie::new((&long_sk).into());
        let socket = TcpServerBackhaul::new(listener, long_sk.clone());
        let (send, recv) = smol::channel::unbounded();
        let task = runtime::spawn_local(
            ListenerActor {
                socket: Arc::new(StatsBackhaul::new(socket, on_recv, on_send)),
                cookie,
                long_sk,
            }
            .run(send),
        );
        Listener {
            accepted: recv,
            local_addr,
            _task: task,
        }
    }

    /// Gets the local address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

struct ListenerActor {
    socket: Arc<dyn Backhaul>,
    cookie: crypt::Cookie,
    long_sk: x25519_dalek::StaticSecret,
}
impl ListenerActor {
    #[allow(clippy::mutable_key_type)]
    #[tracing::instrument(skip(self), level = "trace")]
    async fn run(self, accepted: Sender<Session>) -> Option<()> {
        // session table
        let mut session_table = SessionTable::default();
        // channel for dropping sessions
        let (send_dead, recv_dead) = smol::channel::unbounded();

        let token_key = {
            let mut buf = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut buf);
            buf
        };

        let read_socket = self.socket.clone();
        let write_socket = self.socket.clone();

        // "fallthrough" rate limit. the idea is that new sessions on the same addr are infrequent, so we don't need to check constantly.
        let fallthrough_limiter = RateLimiter::dashmap_with_clock(
            Quota::per_minute(NonZeroU32::new(5u32).unwrap()),
            &governor::clock::MonotonicClock,
        );

        // two possible events
        enum Evt {
            NewRecv(Vec<(Bytes, SocketAddr)>),
            DeadSess(Bytes),
        }

        for trace_id in 0u64.. {
            let event = smol::future::race(
                async { Some(Evt::NewRecv(read_socket.recv_from_many().await.unwrap())) },
                async { Some(Evt::DeadSess(recv_dead.recv().await.ok()?)) },
            );
            if rand::random::<f32>() < 0.001 {
                fallthrough_limiter.retain_recent();
            }
            smol::future::yield_now().await;
            match event.await? {
                Evt::DeadSess(resume_token) => {
                    tracing::trace!("removing existing session!");
                    session_table.delete(resume_token);
                }
                Evt::NewRecv(items) => {
                    let items: Vec<(Bytes, SocketAddr)> = items;
                    for (buffer, addr) in items {
                        // first we attempt to map this to an existing session
                        if let Some(handle) = session_table.lookup(addr) {
                            let _ = handle.try_send(buffer.clone());
                            if fallthrough_limiter.check_key(&addr).is_err() {
                                continue;
                            }
                            // TODO figure out a way to decide whether to continue
                        }
                        // we know it's not part of an existing session then. we decrypt it under the current key
                        let s2c_key = self.cookie.generate_s2c().next().unwrap();
                        for possible_key in self.cookie.generate_c2s() {
                            smol::future::yield_now().await;
                            let crypter = crypt::LegacyAEAD::new(&possible_key);
                            if let Some(handshake) =
                                crypter.pad_decrypt_v1::<protocol::HandshakeFrame>(&buffer)
                            {
                                if !RECENT_FILTER.lock().check(&buffer) {
                                    tracing::debug!(
                                        "discarding replay attempt with len {}",
                                        buffer.len()
                                    );
                                    continue;
                                }
                                tracing::debug!(
                                    "[{}] decoded some sort of handshake: {:?}",
                                    trace_id,
                                    handshake
                                );
                                match handshake[0].clone() {
                                    ClientHello {
                                        long_pk,
                                        eph_pk,
                                        version,
                                    } => {
                                        if version != 1 && version != 2 && version != 3 {
                                            tracing::warn!(
                                                "got packet with incorrect version {}",
                                                version
                                            );
                                            break;
                                        }
                                        // generate session key
                                        let my_eph_sk = x25519_dalek::StaticSecret::new(
                                            &mut rand::thread_rng(),
                                        );
                                        let token = TokenInfo {
                                            sess_key: crypt::triple_ecdh(
                                                &self.long_sk,
                                                &my_eph_sk,
                                                &long_pk,
                                                &eph_pk,
                                            )
                                            .as_bytes()
                                            .to_vec()
                                            .into(),
                                            init_time_ms: std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap()
                                                .as_millis()
                                                as u64,
                                            version,
                                        }
                                        .encrypt(&token_key);
                                        let reply = protocol::HandshakeFrame::ServerHello {
                                            long_pk: (&self.long_sk).into(),
                                            eph_pk: (&my_eph_sk).into(),
                                            resume_token: token,
                                        };
                                        let reply = crypt::LegacyAEAD::new(&s2c_key)
                                            .pad_encrypt_v1(&[reply], 1000);
                                        tracing::debug!(
                                            "[{}] GONNA reply to ClientHello from {}",
                                            trace_id,
                                            addr
                                        );
                                        let _ = write_socket.send_to(reply, addr).await;
                                        tracing::debug!(
                                            "[{}] replied to ClientHello from {}",
                                            trace_id,
                                            addr
                                        );
                                    }
                                    ClientResume {
                                        resume_token,
                                        shard_id,
                                    } => {
                                        tracing::trace!(
                                            "Got ClientResume-{} from {}!",
                                            shard_id,
                                            addr
                                        );
                                        let tokinfo = TokenInfo::decrypt(&token_key, &resume_token);
                                        if let Some(tokinfo) = tokinfo {
                                            // first check whether we know about the resume token
                                            if !session_table.rebind(
                                                addr,
                                                shard_id,
                                                resume_token.clone(),
                                            ) {
                                                tracing::debug!(
                                                    "[{}] ClientResume from {} is new!",
                                                    trace_id,
                                                    addr
                                                );

                                                let up_key = blake3::keyed_hash(
                                                    crypt::UP_KEY,
                                                    &tokinfo.sess_key,
                                                );
                                                let dn_key = blake3::keyed_hash(
                                                    crypt::DN_KEY,
                                                    &tokinfo.sess_key,
                                                );
                                                let write_socket = write_socket.clone();
                                                let (session_input, session_input_recv) =
                                                    smol::channel::bounded(1000);
                                                // create session
                                                let (session_output_send, session_output_recv) =
                                                    smol::channel::bounded(1000);
                                                let locked_addrs =
                                                    ShardedAddrs::new(shard_id, addr);
                                                let locked_addrs =
                                                    Arc::new(RwLock::new(locked_addrs));
                                                let output_poller = {
                                                    let locked_addrs = locked_addrs.clone();
                                                    runtime::spawn(async move {
                                                        loop {
                                                            match session_output_recv.recv().await {
                                                                Ok(data) => {
                                                                    // let start = Instant::now();
                                                                    let remote_addr = locked_addrs
                                                                        .write()
                                                                        .get_addr();
                                                                    drop(
                                                                        write_socket
                                                                            .send_to(
                                                                                data,
                                                                                remote_addr,
                                                                            )
                                                                            .await,
                                                                    );
                                                                }
                                                                Err(_) => {
                                                                    smol::future::pending::<()>()
                                                                        .await
                                                                }
                                                            }
                                                        }
                                                    })
                                                };
                                                let mut session = Session::new(SessionConfig {
                                                    send_packet: session_output_send,
                                                    recv_packet: session_input_recv,
                                                    recv_timeout: Duration::from_secs(3600),
                                                    statistics: 128,

                                                    send_crypt_legacy: crypt::LegacyAEAD::new(
                                                        dn_key.as_bytes(),
                                                    ),
                                                    recv_crypt_legacy: crypt::LegacyAEAD::new(
                                                        up_key.as_bytes(),
                                                    ),

                                                    send_crypt_ng: crypt::NgAEAD::new(
                                                        dn_key.as_bytes(),
                                                    ),
                                                    recv_crypt_ng: crypt::NgAEAD::new(
                                                        up_key.as_bytes(),
                                                    ),
                                                    version: tokinfo.version,
                                                });
                                                let send_dead_clo = send_dead.clone();
                                                let resume_token_clo = resume_token.clone();
                                                session.on_drop(move || {
                                                    drop(output_poller);
                                                    drop(send_dead_clo.try_send(resume_token_clo))
                                                });
                                                // spawn a task that writes to the socket.
                                                session_table.new_sess(
                                                    resume_token.clone(),
                                                    session_input,
                                                    locked_addrs,
                                                );
                                                session_table.rebind(addr, shard_id, resume_token);
                                                tracing::debug!("[{}] accept {}", trace_id, addr);
                                                accepted.try_send(session).ok()?;
                                            } else {
                                                tracing::debug!(
                                                    "[{}] ClientResume from {} rebound",
                                                    trace_id,
                                                    addr
                                                );
                                            }
                                        }
                                    }
                                    _ => continue,
                                }
                            }
                        }
                    }
                }
            }
        }
        unreachable!()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenInfo {
    sess_key: Bytes,
    init_time_ms: u64,
    version: u64,
}

impl TokenInfo {
    fn decrypt(key: &[u8], encrypted: &[u8]) -> Option<Self> {
        // first we decrypt
        let crypter = crypt::LegacyAEAD::new(key);
        let plain = crypter.decrypt(encrypted)?;
        bincode::deserialize(&plain).ok()
    }

    fn encrypt(&self, key: &[u8]) -> Bytes {
        let crypter = crypt::LegacyAEAD::new(key);
        let mut rng = rand::thread_rng();
        crypter.encrypt(
            &bincode::serialize(self).expect("must serialize"),
            rng.gen(),
        )
    }
}
