use std::path::PathBuf;

use iroh::Endpoint;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::protocol::{ChunkData, FileOffer, Message, CHUNK_SIZE, ZAP_ALPN};
use crate::ticket::Ticket;
use crate::{Error, Result};

/// Progress updates for sending
#[derive(Debug, Clone)]
pub enum SendProgress {
    /// Waiting for receiver to connect
    Waiting,

    /// Receiver connected
    Connected,

    /// Sending file data
    Sending { bytes_sent: u64, total_bytes: u64 },

    /// Transfer complete
    Complete,

    /// Error occurred
    Error(String),
}

/// Progress updates for receiving
#[derive(Debug, Clone)]
pub enum ReceiveProgress {
    /// Connecting to sender
    Connecting,

    /// Connected to sender
    Connected,

    /// Received file offer
    Offer { name: String, size: u64 },

    /// Receiving file data
    Receiving {
        bytes_received: u64,
        total_bytes: u64,
    },

    /// Transfer complete
    Complete { path: PathBuf },

    /// Error occurred
    Error(String),
}

/// Handle to control an ongoing transfer
pub struct TransferHandle {
    cancel_tx: mpsc::Sender<()>,
}

impl TransferHandle {
    /// Cancel the transfer
    pub async fn cancel(&self) {
        let _ = self.cancel_tx.send(()).await;
    }
}

/// Run the sender side of a transfer
pub async fn run_sender(
    endpoint: Endpoint,
    path: PathBuf,
    progress: mpsc::Sender<SendProgress>,
) -> Result<()> {
    let _ = progress.send(SendProgress::Waiting).await;

    // Accept incoming connection
    let conn = loop {
        let Some(incoming) = endpoint.accept().await else {
            return Err(Error::ConnectionFailed("endpoint closed".into()));
        };

        let conn = incoming.accept()?.await?;

        // Check ALPN
        if conn.alpn() == ZAP_ALPN {
            break conn;
        }

        debug!("ignoring connection with wrong ALPN");
    };

    let _ = progress.send(SendProgress::Connected).await;
    info!("receiver connected");

    // Accept bidirectional stream from the receiver
    // The receiver sends Ready first to trigger stream creation (QUIC streams are lazy)
    let (mut send_stream, mut recv_stream) = conn.accept_bi().await?;
    debug!("accepted bidirectional stream");

    // Wait for Ready message from receiver
    let ready_msg = recv_message(&mut recv_stream).await?;
    if !matches!(ready_msg, Message::Ready) {
        return Err(Error::Protocol("expected Ready message".into()));
    }
    debug!("received Ready from receiver");

    // Read file metadata
    let file = File::open(&path).await?;
    let metadata = file.metadata().await?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let file_size = metadata.len();

    // Send offer
    let offer = Message::Offer(FileOffer {
        name: file_name.clone(),
        size: file_size,
        checksum: None, // TODO: compute checksum
    });
    send_message(&mut send_stream, &offer).await?;
    debug!("sent offer");

    // Wait for accept/reject
    let response = recv_message(&mut recv_stream).await?;
    match response {
        Message::Accept => {
            info!("receiver accepted transfer");
        }
        Message::Reject { reason } => {
            return Err(Error::TransferFailed(format!(
                "receiver rejected: {}",
                reason
            )));
        }
        _ => {
            return Err(Error::Protocol("unexpected message".into()));
        }
    }

    // Send file chunks
    let mut reader = BufReader::new(file);
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut offset = 0u64;

    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        let chunk = Message::Chunk(ChunkData {
            offset,
            data: buffer[..bytes_read].to_vec(),
        });
        send_message(&mut send_stream, &chunk).await?;

        offset += bytes_read as u64;
        let _ = progress
            .send(SendProgress::Sending {
                bytes_sent: offset,
                total_bytes: file_size,
            })
            .await;
    }

    // Send done
    let done = Message::Done {
        checksum: [0u8; 32], // TODO: actual checksum
    };
    send_message(&mut send_stream, &done).await?;
    debug!("sent done message");

    // Finish the stream and wait for it to be fully sent
    send_stream.finish()?;

    // Wait for the stream to be fully acknowledged
    // This ensures the receiver has time to read the Done message
    match send_stream.stopped().await {
        Ok(_) => debug!("stream finished cleanly"),
        Err(e) => debug!("stream stopped: {:?}", e),
    }

    let _ = progress.send(SendProgress::Complete).await;
    info!("transfer complete");

    Ok(())
}

/// Run the receiver side of a transfer
pub async fn run_receiver(
    endpoint: Endpoint,
    ticket: Ticket,
    output_dir: Option<PathBuf>,
    progress: mpsc::Sender<ReceiveProgress>,
) -> Result<()> {
    let _ = progress.send(ReceiveProgress::Connecting).await;

    debug!(addr = ?ticket.addr, "connecting to sender");

    // Connect to sender
    let conn = endpoint.connect(ticket.addr.clone(), ZAP_ALPN).await?;

    let _ = progress.send(ReceiveProgress::Connected).await;
    info!("connected to sender");

    // Open bidirectional stream
    let (mut send_stream, mut recv_stream) = conn.open_bi().await?;
    debug!("opened bidirectional stream");

    // Send Ready message to trigger stream creation on sender side
    // (QUIC streams are lazy - only created when data is sent)
    send_message(&mut send_stream, &Message::Ready).await?;
    debug!("sent Ready message");

    // Receive offer
    let offer = match recv_message(&mut recv_stream).await? {
        Message::Offer(offer) => offer,
        _ => return Err(Error::Protocol("expected offer".into())),
    };

    let _ = progress
        .send(ReceiveProgress::Offer {
            name: offer.name.clone(),
            size: offer.size,
        })
        .await;

    info!(name = %offer.name, size = offer.size, "received offer");

    // Send accept
    send_message(&mut send_stream, &Message::Accept).await?;

    // Prepare output file
    let output_path = output_dir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        .join(&offer.name);

    let file = File::create(&output_path).await?;
    let mut writer = BufWriter::new(file);
    let mut bytes_received = 0u64;

    // Receive chunks
    loop {
        let msg = recv_message(&mut recv_stream).await?;
        match msg {
            Message::Chunk(chunk) => {
                writer.write_all(&chunk.data).await?;
                bytes_received += chunk.data.len() as u64;

                let _ = progress
                    .send(ReceiveProgress::Receiving {
                        bytes_received,
                        total_bytes: offer.size,
                    })
                    .await;
            }
            Message::Done { checksum: _ } => {
                break;
            }
            Message::Error { message } => {
                return Err(Error::TransferFailed(message));
            }
            _ => {
                return Err(Error::Protocol("unexpected message".into()));
            }
        }
    }

    writer.flush().await?;
    drop(writer);

    let _ = progress
        .send(ReceiveProgress::Complete {
            path: output_path.clone(),
        })
        .await;
    info!(path = %output_path.display(), "transfer complete");

    Ok(())
}

/// Send a length-prefixed message
async fn send_message(stream: &mut iroh::endpoint::SendStream, msg: &Message) -> Result<()> {
    let bytes = msg
        .to_bytes()
        .map_err(|e| Error::Protocol(format!("serialization error: {}", e)))?;

    let len = (bytes.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&bytes).await?;

    Ok(())
}

/// Receive a length-prefixed message
async fn recv_message(stream: &mut iroh::endpoint::RecvStream) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 10 * 1024 * 1024 {
        return Err(Error::Protocol("message too large".into()));
    }

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    Message::from_bytes(&buf).map_err(|e| Error::Protocol(format!("deserialization error: {}", e)))
}
