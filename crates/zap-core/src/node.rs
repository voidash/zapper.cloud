use std::path::Path;

use iroh::{Endpoint, EndpointAddr, SecretKey};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::protocol::ZAP_ALPN;
use crate::ticket::Ticket;
use crate::transfer::{self, ReceiveProgress, SendProgress};
use crate::{Error, Result};

/// A zap node that can send and receive files
pub struct ZapNode {
    endpoint: Endpoint,
}

impl ZapNode {
    /// Create a new zap node
    pub async fn new() -> Result<Self> {
        Self::with_secret_key(SecretKey::generate(&mut rand::rng())).await
    }

    /// Create a new zap node with a specific secret key
    pub async fn with_secret_key(secret_key: SecretKey) -> Result<Self> {
        let endpoint = Endpoint::builder()
            .secret_key(secret_key)
            .alpns(vec![ZAP_ALPN.to_vec()])
            .bind()
            .await?;

        // Wait for the endpoint to be online (connected to relay)
        endpoint.online().await;

        info!(node_id = %endpoint.id(), "zap node started");

        Ok(Self { endpoint })
    }

    /// Get this node's endpoint address for sharing
    ///
    /// This includes both relay URLs and direct socket addresses when available.
    pub fn addr(&self) -> EndpointAddr {
        let mut addr = self.endpoint.addr();

        // Add bound socket addresses for direct connections
        for socket_addr in self.endpoint.bound_sockets() {
            addr = addr.with_ip_addr(socket_addr);
        }

        addr
    }

    /// Get the endpoint's ID
    pub fn id(&self) -> iroh::PublicKey {
        self.endpoint.id()
    }

    /// Generate a ticket for others to connect to this node
    pub fn ticket(&self) -> Ticket {
        Ticket::new(self.addr())
    }

    /// Send a file to a receiver
    ///
    /// Returns a channel that will receive progress updates
    pub async fn send<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<(Ticket, mpsc::Receiver<SendProgress>)> {
        let path = path.as_ref().to_path_buf();

        // Validate file exists
        if !path.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("file not found: {}", path.display()),
            )));
        }

        if !path.is_file() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path must be a file",
            )));
        }

        let (progress_tx, progress_rx) = mpsc::channel(32);
        let endpoint = self.endpoint.clone();
        let ticket = self.ticket();

        // Spawn the sender task
        tokio::spawn(async move {
            if let Err(e) = transfer::run_sender(endpoint, path, progress_tx.clone()).await {
                let _ = progress_tx.send(SendProgress::Error(e.to_string())).await;
            }
        });

        Ok((ticket, progress_rx))
    }

    /// Receive a file from a sender
    ///
    /// Returns a channel that will receive progress updates
    pub async fn receive(
        &self,
        ticket: Ticket,
        output_dir: Option<&Path>,
    ) -> Result<mpsc::Receiver<ReceiveProgress>> {
        let (progress_tx, progress_rx) = mpsc::channel(32);
        let endpoint = self.endpoint.clone();
        let output_dir = output_dir.map(|p| p.to_path_buf());

        // Connect to the sender
        debug!(node_id = %ticket.addr.id, "connecting to sender");

        tokio::spawn(async move {
            if let Err(e) =
                transfer::run_receiver(endpoint, ticket, output_dir, progress_tx.clone()).await
            {
                let _ = progress_tx
                    .send(ReceiveProgress::Error(e.to_string()))
                    .await;
            }
        });

        Ok(progress_rx)
    }

    /// Shutdown the node gracefully
    pub async fn shutdown(self) -> Result<()> {
        self.endpoint.close().await;
        Ok(())
    }
}
