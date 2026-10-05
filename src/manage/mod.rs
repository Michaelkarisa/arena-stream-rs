use crate::api::CLIENT;
use crate::config::MANAGE_NET_PORT;
use crate::model::{OutputKind, REGISTRY};
use crate::overlay::{LineupState, OverlayState, TeamLineupData};
use crate::pipeline;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::{info, warn};

pub async fn run() -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", MANAGE_NET_PORT)).await?;
    info!(port = MANAGE_NET_PORT, "manage net socket listening");

    loop {
        let (socket, addr) = listener.accept().await?;
        let peer_ip = addr.ip().to_string();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, peer_ip.clone()).await {
                warn!(peer_ip, "manage net connection ended: {e}");
            }
        });
    }
}