use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A ticket contains everything needed to connect to a sender
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    /// The sender's endpoint address
    pub addr: EndpointAddr,
}

impl Ticket {
    /// Create a new ticket from an endpoint address
    pub fn new(addr: EndpointAddr) -> Self {
        Self { addr }
    }

    /// Serialize to a human-friendly string
    pub fn serialize(&self) -> String {
        let bytes = postcard::to_allocvec(self).expect("ticket serialization cannot fail");
        data_encoding::BASE32_NOPAD.encode(&bytes).to_lowercase()
    }

    /// Parse from a human-friendly string
    pub fn deserialize(s: &str) -> Result<Self> {
        let s = s.trim().to_uppercase();
        let bytes = data_encoding::BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|e| Error::InvalidTicket(format!("invalid base32: {}", e)))?;

        postcard::from_bytes(&bytes)
            .map_err(|e| Error::InvalidTicket(format!("invalid ticket data: {}", e)))
    }
}

impl std::fmt::Display for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.serialize())
    }
}

impl std::str::FromStr for Ticket {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::deserialize(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn test_ticket_roundtrip() {
        let secret = SecretKey::generate(&mut rand::rng());
        let public = secret.public();
        let addr = EndpointAddr::new(public);

        let ticket = Ticket::new(addr.clone());
        let encoded = ticket.serialize();
        let decoded = Ticket::deserialize(&encoded).unwrap();

        assert_eq!(ticket.addr.id, decoded.addr.id);
    }
}
