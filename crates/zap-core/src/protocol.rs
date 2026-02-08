use serde::{Deserialize, Serialize};

/// ALPN protocol identifier for zap
pub const ZAP_ALPN: &[u8] = b"zap/1";

/// Chunk size for file transfers (256 KB)
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Messages sent over the wire
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Receiver signals ready to receive
    /// This is sent first to establish the stream (QUIC streams are lazy)
    Ready,

    /// Sender announces file metadata
    Offer(FileOffer),

    /// Receiver accepts the transfer
    Accept,

    /// Receiver rejects the transfer
    Reject { reason: String },

    /// File data chunk
    Chunk(ChunkData),

    /// Transfer complete
    Done { checksum: [u8; 32] },

    /// Error occurred
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOffer {
    /// Original filename
    pub name: String,

    /// Total size in bytes
    pub size: u64,

    /// BLAKE3 hash of the file (computed incrementally)
    pub checksum: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkData {
    /// Offset in the file
    pub offset: u64,

    /// The actual data
    pub data: Vec<u8>,
}

impl Message {
    /// Serialize message to bytes using postcard
    pub fn to_bytes(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize message from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}
