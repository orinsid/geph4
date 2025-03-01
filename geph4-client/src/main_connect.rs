use crate::{cache::ClientCache, kalive::Keepalive, stats::StatCollector, AuthOpt, CommonOpt};
use crate::{china, stats::GLOBAL_LOGGER};
use anyhow::Context;
use async_compat::Compat;
use chrono::prelude::*;
use smol_timeout::TimeoutExt;
use std::{net::Ipv4Addr, net::SocketAddr, net::SocketAddrV4, sync::Arc, time::Duration};
use structopt::StructOpt;

#[derive(Debug, StructOpt, Clone)]
pub struct ConnectOpt {
    #[structopt(flatten)]
    common: CommonOpt,

    #[structopt(flatten)]
    auth: AuthOpt,

    #[structopt(long)]
    /// whether or not to use bridges
    pub use_bridges: bool,

    #[structopt(long, default_value = "127.0.0.1:9910")]
    /// where to listen for HTTP proxy connections
    http_listen: SocketAddr,
    #[structopt(long, default_value = "127.0.0.1:9909")]
    /// where to listen for SOCKS5 connections
    socks5_listen: SocketAddr,
    #[structopt(long, default_value = "127.0.0.1:9809")]
    /// where to listen for REST-based local connections
    stats_listen: SocketAddr,

    #[structopt(long)]
    /// where to listen for proxied DNS requests. Optional.
    dns_listen: Option<SocketAddr>,

    #[structopt(long, default_value = "us-hio-01.exits.geph.io")]
    /// which exit server to connect to. If there isn't an exact match, the exit server with the most similar hostname is picked.
    pub exit_server: String,

    #[structopt(long)]
    /// whether or not to exclude PRC domains
    exclude_prc: bool,

    #[structopt(long)]
    /// whether or not to wait for VPN commands on stdio
    pub stdio_vpn: bool,

    #[structopt(long)]
    /// an endpoint to send test results. If set, will periodically do network testing.
    nettest_server: Option<SocketAddr>,

    #[structopt(long)]
    /// a name for this test instance.
    nettest_name: Option<String>,

    #[structopt(long)]
    /// whether or not to force TCP mode.
    pub use_tcp: bool,
}

pub async fn main_connect(opt: ConnectOpt) -> anyhow::Result<()> {
    log::info!("connect mode started");

    //start socks 2 http
    smolscale::spawn(Compat::new(socks2http::run_tokio(opt.http_listen, {
        let mut addr = opt.socks5_listen;
        addr.set_ip("127.0.0.1".parse().unwrap());
        addr
    })))
    .detach();

    let stat_collector = Arc::new(StatCollector::default());
    // create a db directory if doesn't exist
    let client_cache =
        ClientCache::from_opts(&opt.common, &opt.auth).context("cannot create ClientCache")?;
    // create a kalive
    let keepalive = Keepalive::new(stat_collector.clone(), opt.clone(), Arc::new(client_cache));
    // enter the socks5 loop
    let socks5_listener = smol::net::TcpListener::bind(opt.socks5_listen)
        .await
        .context("cannot bind socks5")?;
    let stat_listener = smol::net::TcpListener::bind(opt.stats_listen)
        .await
        .context("cannot bind stats")?;
    let scollect = stat_collector.clone();
    // scope
    if let Some(dns_listen) = opt.dns_listen {
        log::debug!("starting dns...");
        smolscale::spawn(crate::dns::dns_loop(dns_listen, keepalive.clone())).detach();
    }
    if let Some(nettest_server) = opt.nettest_server {
        log::info!("Network testing enabled at {}!", nettest_server);
        smolscale::spawn(crate::nettest::nettest(
            opt.nettest_name.unwrap(),
            nettest_server,
        ))
        .detach();
    }
    let _stat: smol::Task<anyhow::Result<()>> = {
        let keepalive = keepalive.clone();
        smolscale::spawn(async move {
            loop {
                let (stat_client, _) = stat_listener.accept().await?;
                let scollect = scollect.clone();
                let keepalive = keepalive.clone();
                smolscale::spawn(async move {
                    drop(
                        async_h1::accept(stat_client, |req| {
                            handle_stats(scollect.clone(), &keepalive, req)
                        })
                        .await,
                    );
                })
                .detach();
            }
        })
    };
    let exclude_prc = opt.exclude_prc;

    loop {
        let (s5client, _) = socks5_listener
            .accept()
            .await
            .context("cannot accept socks5")?;
        let keepalive = keepalive.clone();
        let stat_collector = stat_collector.clone();
        smolscale::spawn(async move {
            handle_socks5(stat_collector, s5client, &keepalive, exclude_prc).await
        })
        .detach()
    }
}
use std::io::prelude::*;

/// Handle a request for stats
async fn handle_stats(
    stats: Arc<StatCollector>,
    kalive: &Keepalive,
    _req: http_types::Request,
) -> http_types::Result<http_types::Response> {
    let mut res = http_types::Response::new(http_types::StatusCode::Ok);
    match _req.url().path() {
        "/debugpack" => {
            // create logs and sosistab buffers
            // form a tar
            let tar_buffer = Vec::new();
            let mut tar_build = tar::Builder::new(tar_buffer);
            let mut logs_buffer = Vec::new();
            {
                let noo = GLOBAL_LOGGER.read();
                for line in noo.iter() {
                    writeln!(logs_buffer, "{}", line)?;
                }
            }
            let detail = kalive.get_stats().timeout(Duration::from_secs(1)).await;
            if let Some(detail) = detail {
                let detail = detail?;
                let mut sosistab_buf = Vec::new();
                writeln!(sosistab_buf, "time,last_recv,total_recv,total_loss,ping")?;
                if let Some(first) = detail.first() {
                    let first_time = first.time;
                    for item in detail.iter() {
                        writeln!(
                            sosistab_buf,
                            "{},{},{},{},{}",
                            item.time
                                .duration_since(first_time)
                                .unwrap_or_default()
                                .as_secs_f64(),
                            item.high_recv,
                            item.total_recv,
                            item.total_loss,
                            item.ping.as_secs_f64() * 1000.0,
                        )?;
                    }
                }
                let mut sosis_header = tar::Header::new_gnu();
                sosis_header.set_mode(0o666);
                sosis_header.set_size(sosistab_buf.len() as u64);
                tar_build.append_data(
                    &mut sosis_header,
                    "sosistab-trace.csv",
                    sosistab_buf.as_slice(),
                )?;
            }
            let mut logs_header = tar::Header::new_gnu();
            logs_header.set_mode(0o666);
            logs_header.set_size(logs_buffer.len() as u64);
            tar_build.append_data(&mut logs_header, "logs.txt", logs_buffer.as_slice())?;
            let result = tar_build.into_inner()?;
            res.insert_header("content-type", "application/tar");
            res.insert_header(
                "content-disposition",
                format!(
                    "attachment; filename=\"geph4-debug-{}.tar\"",
                    Local::now().to_rfc3339()
                ),
            );
            res.set_body(result);
            Ok(res)
        }
        "/proxy.pac" => {
            res.set_body("function FindProxyForURL(url, host){return 'PROXY 127.0.0.1:9910';}");
            Ok(res)
        }
        "/kill" => std::process::exit(0),
        _ => {
            let detail = kalive.get_stats().timeout(Duration::from_millis(100)).await;
            if let Some(Ok(details)) = detail {
                if let Some(detail) = details.last() {
                    stats.set_latency(detail.ping.as_secs_f64() * 1000.0);
                    // compute loss
                    let midpoint_stat = details[details.len() / 2];
                    let delta_high = detail
                        .high_recv
                        .saturating_sub(midpoint_stat.high_recv)
                        .max(1) as f64;
                    let delta_total = detail
                        .total_recv
                        .saturating_sub(midpoint_stat.total_recv)
                        .max(1) as f64;
                    // dbg!(delta_total);
                    // dbg!(delta_high);
                    let loss = 1.0 - (delta_total / delta_high).min(1.0).max(0.0);
                    stats.set_loss(loss * 100.0)
                }
            }
            let jstats = serde_json::to_string(&stats)?;
            res.set_body(jstats);
            res.insert_header("Content-Type", "application/json");
            Ok(res)
        }
    }
}

/// Handle a socks5 client from localhost.
async fn handle_socks5(
    stats: Arc<StatCollector>,
    s5client: smol::net::TcpStream,
    keepalive: &Keepalive,
    exclude_prc: bool,
) -> anyhow::Result<()> {
    s5client.set_nodelay(true)?;
    use socksv5::v5::*;
    let _handshake = read_handshake(s5client.clone()).await?;
    write_auth_method(s5client.clone(), SocksV5AuthMethod::Noauth).await?;
    let request = read_request(s5client.clone()).await?;
    let port = request.port;
    let v4addr: Option<Ipv4Addr>;
    let addr: String = match &request.host {
        SocksV5Host::Domain(dom) => {
            v4addr = String::from_utf8_lossy(&dom).parse().ok();
            format!("{}:{}", String::from_utf8_lossy(&dom), request.port)
        }
        SocksV5Host::Ipv4(v4) => SocketAddr::V4(SocketAddrV4::new(
            {
                v4addr = Some(Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3]));
                v4addr.unwrap()
            },
            request.port,
        ))
        .to_string(),
        _ => anyhow::bail!("not supported"),
    };
    write_request_status(
        s5client.clone(),
        SocksV5RequestStatus::Success,
        request.host,
        port,
    )
    .await?;
    let must_direct = exclude_prc
        && (china::is_chinese_host(addr.split(':').next().unwrap())
            || v4addr.map(china::is_chinese_ip).unwrap_or(false));
    if must_direct {
        log::debug!("bypassing {}", addr);
        let conn = smol::net::TcpStream::connect(&addr).await?;
        smol::future::race(
            aioutils::copy_with_stats(conn.clone(), s5client.clone(), |_| ()),
            aioutils::copy_with_stats(s5client.clone(), conn.clone(), |_| ()),
        )
        .await?;
    } else {
        let conn = keepalive.connect(&addr).await?;
        smol::future::race(
            aioutils::copy_with_stats(conn.clone(), s5client.clone(), |n| {
                stats.incr_total_rx(n as u64)
            }),
            aioutils::copy_with_stats(s5client, conn, |n| stats.incr_total_tx(n as u64)),
        )
        .await?;
    }
    Ok(())
}

// /// Smallify the buffers for a TCP connection
// fn debuffer(conn: async_net::TcpStream) -> async_net::TcpStream {
//     let conn: Arc<smol::Async<std::net::TcpStream>> = conn.into();
//     let conn: std::net::TcpStream = conn.get_ref().try_clone().unwrap();
//     let conn: socket2::Socket = conn.into();
//     conn.set_nodelay(true).unwrap();
//     conn.set_recv_buffer_size(163840).unwrap();
//     conn.set_send_buffer_size(163840).unwrap();
//     smol::Async::new(conn.into_tcp_stream()).unwrap().into()
// }
