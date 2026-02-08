#[cfg(test)]
mod unit_tests {
    use crate::protocol::{ChunkData, FileOffer, Message, CHUNK_SIZE};
    use crate::ticket::Ticket;
    use iroh::{EndpointAddr, SecretKey};

    #[test]
    fn test_message_serialization_offer() {
        let offer = Message::Offer(FileOffer {
            name: "test.txt".to_string(),
            size: 1024,
            checksum: None,
        });

        let bytes = offer.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Offer(o) => {
                assert_eq!(o.name, "test.txt");
                assert_eq!(o.size, 1024);
                assert!(o.checksum.is_none());
            }
            _ => panic!("expected Offer message"),
        }
    }

    #[test]
    fn test_message_serialization_chunk() {
        let data = vec![1, 2, 3, 4, 5];
        let chunk = Message::Chunk(ChunkData {
            offset: 100,
            data: data.clone(),
        });

        let bytes = chunk.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Chunk(c) => {
                assert_eq!(c.offset, 100);
                assert_eq!(c.data, data);
            }
            _ => panic!("expected Chunk message"),
        }
    }

    #[test]
    fn test_message_serialization_accept() {
        let msg = Message::Accept;
        let bytes = msg.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();
        assert!(matches!(decoded, Message::Accept));
    }

    #[test]
    fn test_message_serialization_reject() {
        let msg = Message::Reject {
            reason: "file too large".to_string(),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Reject { reason } => {
                assert_eq!(reason, "file too large");
            }
            _ => panic!("expected Reject message"),
        }
    }

    #[test]
    fn test_message_serialization_done() {
        let checksum = [42u8; 32];
        let msg = Message::Done { checksum };
        let bytes = msg.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Done { checksum: c } => {
                assert_eq!(c, checksum);
            }
            _ => panic!("expected Done message"),
        }
    }

    #[test]
    fn test_message_serialization_error() {
        let msg = Message::Error {
            message: "something went wrong".to_string(),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Error { message } => {
                assert_eq!(message, "something went wrong");
            }
            _ => panic!("expected Error message"),
        }
    }

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

    #[test]
    fn test_ticket_case_insensitive() {
        let secret = SecretKey::generate(&mut rand::rng());
        let public = secret.public();
        let addr = EndpointAddr::new(public);

        let ticket = Ticket::new(addr);
        let encoded = ticket.serialize();

        // Should work with uppercase
        let upper = encoded.to_uppercase();
        let decoded = Ticket::deserialize(&upper).unwrap();
        assert_eq!(ticket.addr.id, decoded.addr.id);

        // Should work with mixed case
        let mixed: String = encoded
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        let decoded = Ticket::deserialize(&mixed).unwrap();
        assert_eq!(ticket.addr.id, decoded.addr.id);
    }

    #[test]
    fn test_ticket_whitespace_handling() {
        let secret = SecretKey::generate(&mut rand::rng());
        let public = secret.public();
        let addr = EndpointAddr::new(public);

        let ticket = Ticket::new(addr);
        let encoded = ticket.serialize();

        // Should work with leading/trailing whitespace
        let with_whitespace = format!("  {}  \n", encoded);
        let decoded = Ticket::deserialize(&with_whitespace).unwrap();
        assert_eq!(ticket.addr.id, decoded.addr.id);
    }

    #[test]
    fn test_ticket_invalid_base32() {
        let result = Ticket::deserialize("not-valid-base32!");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid base32"));
    }

    #[test]
    fn test_ticket_invalid_data() {
        // Valid base32 but not a valid ticket
        let result = Ticket::deserialize("MFRGGZDFMY");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid ticket data"));
    }

    #[test]
    fn test_chunk_size_reasonable() {
        // Chunk size should be reasonable for network transfer
        assert!(CHUNK_SIZE >= 64 * 1024); // At least 64KB
        assert!(CHUNK_SIZE <= 1024 * 1024); // At most 1MB
    }

    #[test]
    fn test_large_chunk_serialization() {
        let data = vec![42u8; CHUNK_SIZE];
        let chunk = Message::Chunk(ChunkData { offset: 0, data });

        let bytes = chunk.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Chunk(c) => {
                assert_eq!(c.data.len(), CHUNK_SIZE);
                assert!(c.data.iter().all(|&b| b == 42));
            }
            _ => panic!("expected Chunk message"),
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::ZapNode;

    #[tokio::test]
    async fn test_node_creation() {
        let node = ZapNode::new().await;
        assert!(node.is_ok(), "should create node successfully");

        let node = node.unwrap();
        let addr = node.addr();
        assert!(!addr.id.to_string().is_empty(), "should have valid node id");

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_ticket_generation() {
        let node = ZapNode::new().await.unwrap();
        let ticket = node.ticket();

        assert_eq!(ticket.addr.id, node.addr().id);

        let serialized = ticket.serialize();
        assert!(!serialized.is_empty());

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_send_nonexistent_file() {
        let node = ZapNode::new().await.unwrap();
        let result = node.send("/nonexistent/file/path.txt").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_send_directory_fails() {
        let node = ZapNode::new().await.unwrap();

        // Create temp directory
        let temp_dir = tempfile::tempdir().unwrap();
        let result = node.send(temp_dir.path()).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("must be a file"));

        node.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod e2e_tests {
    use crate::{ReceiveProgress, SendProgress, ZapNode};
    use std::time::Duration;
    use tokio::fs;
    use tokio::time::timeout;

    /// Test basic connection between two nodes
    #[tokio::test]
    async fn test_basic_connection() {
        // Create temp file for sender
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, b"Hello").await.unwrap();

        // Create two nodes
        let sender_node = ZapNode::new().await.unwrap();
        let receiver_node = ZapNode::new().await.unwrap();

        println!("Sender address: {:?}", sender_node.addr());

        // Start the sender (this will listen for connections)
        let (ticket, mut sender_progress) = sender_node.send(&test_file).await.unwrap();
        println!("Ticket: {}", ticket);

        // Start the receiver
        let output_dir = temp_dir.path().join("output");
        fs::create_dir(&output_dir).await.unwrap();

        let mut receiver_progress = receiver_node
            .receive(ticket, Some(output_dir.as_path()))
            .await
            .unwrap();

        // Wait for connection on both sides
        let result = timeout(Duration::from_secs(30), async {
            let mut sender_connected = false;
            let mut receiver_connected = false;

            loop {
                tokio::select! {
                    Some(p) = sender_progress.recv() => {
                        println!("Sender progress: {:?}", p);
                        match p {
                            SendProgress::Connected => {
                                sender_connected = true;
                            }
                            SendProgress::Error(e) => {
                                println!("Sender error: {}", e);
                                return false;
                            }
                            _ => {}
                        }
                    }
                    Some(p) = receiver_progress.recv() => {
                        println!("Receiver progress: {:?}", p);
                        match p {
                            ReceiveProgress::Connected => {
                                receiver_connected = true;
                            }
                            ReceiveProgress::Error(e) => {
                                println!("Receiver error: {}", e);
                                return false;
                            }
                            _ => {}
                        }
                    }
                }

                if sender_connected && receiver_connected {
                    println!("Both sides connected!");
                    return true;
                }
            }
        })
        .await;

        sender_node.shutdown().await.unwrap();
        receiver_node.shutdown().await.unwrap();

        assert!(result.is_ok(), "should complete within timeout");
        assert!(result.unwrap(), "connection should succeed");
    }

    /// Test a complete file transfer between two nodes
    #[tokio::test]
    async fn test_file_transfer_small() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        let test_content = b"Hello, Zap!";
        fs::write(&test_file, test_content).await.unwrap();

        // Create sender node and start sending
        let sender_node = ZapNode::new().await.unwrap();
        let (ticket, mut sender_progress) = sender_node.send(&test_file).await.unwrap();
        println!("Sender started with ticket: {}", ticket);

        // Create receiver node and start receiving
        let receiver_node = ZapNode::new().await.unwrap();
        let output_dir = temp_dir.path().join("output");
        fs::create_dir(&output_dir).await.unwrap();

        let mut receiver_progress = receiver_node
            .receive(ticket, Some(output_dir.as_path()))
            .await
            .unwrap();
        println!("Receiver started");

        // Wait for transfer to complete with timeout
        let result = timeout(Duration::from_secs(30), async {
            let mut sender_done = false;
            let mut receiver_done = false;
            let mut received_path = None;

            loop {
                tokio::select! {
                    Some(progress) = sender_progress.recv() => {
                        println!("Sender: {:?}", progress);
                        match progress {
                            SendProgress::Complete => {
                                sender_done = true;
                            }
                            SendProgress::Error(e) => {
                                panic!("sender error: {}", e);
                            }
                            _ => {}
                        }
                    }
                    Some(progress) = receiver_progress.recv() => {
                        println!("Receiver: {:?}", progress);
                        match progress {
                            ReceiveProgress::Complete { path } => {
                                receiver_done = true;
                                received_path = Some(path);
                            }
                            ReceiveProgress::Error(e) => {
                                panic!("receiver error: {}", e);
                            }
                            _ => {}
                        }
                    }
                }

                if sender_done && receiver_done {
                    println!("Both done!");
                    break;
                }
            }

            received_path
        })
        .await;

        assert!(result.is_ok(), "transfer should complete within timeout");
        let received_path = result.unwrap().unwrap();

        // Verify content
        let received_content = fs::read(&received_path).await.unwrap();
        assert_eq!(received_content, test_content);

        sender_node.shutdown().await.unwrap();
        receiver_node.shutdown().await.unwrap();
    }

    /// Test transfer of a larger file
    #[tokio::test]
    async fn test_file_transfer_large() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("large.bin");

        // Create a 1MB file
        let size = 1024 * 1024;
        let test_content: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        fs::write(&test_file, &test_content).await.unwrap();

        // Create sender node and start sending
        let sender_node = ZapNode::new().await.unwrap();
        let (ticket, mut sender_progress) = sender_node.send(&test_file).await.unwrap();

        // Create receiver node and start receiving
        let receiver_node = ZapNode::new().await.unwrap();
        let output_dir = temp_dir.path().join("output");
        fs::create_dir(&output_dir).await.unwrap();

        let mut receiver_progress = receiver_node
            .receive(ticket, Some(output_dir.as_path()))
            .await
            .unwrap();

        // Track progress
        let mut last_bytes_sent = 0u64;
        let mut last_bytes_received = 0u64;

        let result = timeout(Duration::from_secs(60), async {
            let mut sender_done = false;
            let mut receiver_done = false;
            let mut received_path = None;

            loop {
                tokio::select! {
                    Some(progress) = sender_progress.recv() => {
                        match progress {
                            SendProgress::Sending { bytes_sent, total_bytes } => {
                                assert!(bytes_sent >= last_bytes_sent, "progress should not go backwards");
                                assert_eq!(total_bytes, size as u64);
                                last_bytes_sent = bytes_sent;
                            }
                            SendProgress::Complete => {
                                sender_done = true;
                            }
                            SendProgress::Error(e) => {
                                panic!("sender error: {}", e);
                            }
                            _ => {}
                        }
                    }
                    Some(progress) = receiver_progress.recv() => {
                        match progress {
                            ReceiveProgress::Receiving { bytes_received, total_bytes } => {
                                assert!(bytes_received >= last_bytes_received, "progress should not go backwards");
                                assert_eq!(total_bytes, size as u64);
                                last_bytes_received = bytes_received;
                            }
                            ReceiveProgress::Complete { path } => {
                                receiver_done = true;
                                received_path = Some(path);
                            }
                            ReceiveProgress::Error(e) => {
                                panic!("receiver error: {}", e);
                            }
                            _ => {}
                        }
                    }
                }

                if sender_done && receiver_done {
                    break;
                }
            }

            received_path
        })
        .await;

        assert!(result.is_ok(), "transfer should complete within timeout");
        let received_path = result.unwrap().unwrap();

        // Verify content
        let received_content = fs::read(&received_path).await.unwrap();
        assert_eq!(received_content.len(), test_content.len());
        assert_eq!(received_content, test_content);

        sender_node.shutdown().await.unwrap();
        receiver_node.shutdown().await.unwrap();
    }

    /// Test that receiver gets correct file metadata
    #[tokio::test]
    async fn test_file_metadata_transfer() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("metadata_test.txt");
        let test_content = b"Testing metadata";
        fs::write(&test_file, test_content).await.unwrap();

        let sender_node = ZapNode::new().await.unwrap();
        let (ticket, _sender_progress) = sender_node.send(&test_file).await.unwrap();

        let receiver_node = ZapNode::new().await.unwrap();
        let output_dir = temp_dir.path().join("output");
        fs::create_dir(&output_dir).await.unwrap();

        let mut receiver_progress = receiver_node
            .receive(ticket, Some(output_dir.as_path()))
            .await
            .unwrap();

        let result = timeout(Duration::from_secs(30), async {
            let mut got_offer = false;
            let mut offer_name = String::new();
            let mut offer_size = 0u64;

            while let Some(progress) = receiver_progress.recv().await {
                match progress {
                    ReceiveProgress::Offer { name, size } => {
                        got_offer = true;
                        offer_name = name;
                        offer_size = size;
                    }
                    ReceiveProgress::Complete { .. } => {
                        break;
                    }
                    ReceiveProgress::Error(e) => {
                        panic!("receiver error: {}", e);
                    }
                    _ => {}
                }
            }

            (got_offer, offer_name, offer_size)
        })
        .await;

        assert!(result.is_ok());
        let (got_offer, name, size) = result.unwrap();
        assert!(got_offer, "should receive offer");
        assert_eq!(name, "metadata_test.txt");
        assert_eq!(size, test_content.len() as u64);

        sender_node.shutdown().await.unwrap();
        receiver_node.shutdown().await.unwrap();
    }

    /// Test that multiple sequential transfers work
    #[tokio::test]
    async fn test_multiple_transfers() {
        let temp_dir = tempfile::tempdir().unwrap();

        for i in 0..3 {
            let test_file = temp_dir.path().join(format!("test_{}.txt", i));
            let test_content = format!("Content for file {}", i);
            fs::write(&test_file, test_content.as_bytes()).await.unwrap();

            let sender_node = ZapNode::new().await.unwrap();
            let (ticket, mut sender_progress) = sender_node.send(&test_file).await.unwrap();

            let receiver_node = ZapNode::new().await.unwrap();
            let output_dir = temp_dir.path().join(format!("output_{}", i));
            fs::create_dir(&output_dir).await.unwrap();

            let mut receiver_progress = receiver_node
                .receive(ticket, Some(output_dir.as_path()))
                .await
                .unwrap();

            let result = timeout(Duration::from_secs(30), async {
                let mut sender_done = false;
                let mut receiver_done = false;
                let mut received_path = None;

                loop {
                    tokio::select! {
                        Some(progress) = sender_progress.recv() => {
                            match progress {
                                SendProgress::Complete => sender_done = true,
                                SendProgress::Error(e) => panic!("sender error: {}", e),
                                _ => {}
                            }
                        }
                        Some(progress) = receiver_progress.recv() => {
                            match progress {
                                ReceiveProgress::Complete { path } => {
                                    receiver_done = true;
                                    received_path = Some(path);
                                }
                                ReceiveProgress::Error(e) => panic!("receiver error: {}", e),
                                _ => {}
                            }
                        }
                    }

                    if sender_done && receiver_done {
                        break;
                    }
                }

                received_path
            })
            .await;

            assert!(result.is_ok(), "transfer {} should complete", i);
            let received_path = result.unwrap().unwrap();
            let received_content = fs::read_to_string(&received_path).await.unwrap();
            assert_eq!(received_content, test_content);

            sender_node.shutdown().await.unwrap();
            receiver_node.shutdown().await.unwrap();
        }
    }
}
