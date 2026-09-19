pub mod fs;

use anyhow::{Context, Result};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tracing::info;

use crate::nfs::fs::GitNfsFileSystem;

pub struct RunningNfsServer {
    pub port: u16,
    pub ip: String,
}

pub async fn start_nfs_server(
    ip: &str,
    port: u16,
    fs: GitNfsFileSystem,
) -> Result<RunningNfsServer> {
    let bind_addr = format!("{ip}:{port}");
    info!("Binding NFSv3 TCP listener to {bind_addr}...");
    let listener = NFSTcpListener::bind(&bind_addr, fs)
        .await
        .context(format!("Binding NFS TCP listener to {bind_addr}"))?;

    let bound_port = listener.get_listen_port();
    info!("NFSv3 server listening on {ip}:{bound_port}");

    tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            tracing::error!("NFS server listener encountered error: {e}");
        }
    });

    Ok(RunningNfsServer {
        port: bound_port,
        ip: ip.to_string(),
    })
}
