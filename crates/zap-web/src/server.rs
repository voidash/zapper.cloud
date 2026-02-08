use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use uuid::Uuid;
use zap_core::{ReceiveProgress, SendProgress, Ticket, ZapNode};

/// Maximum file size (1 GB)
const MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;

/// How long to keep completed transfers before cleanup (1 hour)
const TRANSFER_TTL: Duration = Duration::from_secs(60 * 60);

/// Cleanup interval (5 minutes)
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Generate a short, easy-to-share code (6 characters, alphanumeric)
fn generate_short_code() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789"; // No confusing chars (0,1,i,l,o)
    let mut rng = rand::rng();
    (0..6)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    transfers: Arc<RwLock<HashMap<String, TransferState>>>,
    /// Maps short codes to full tickets for easy sharing
    ticket_codes: Arc<RwLock<HashMap<String, String>>>,
    temp_dir: PathBuf,
}

struct TransferState {
    status: TransferStatus,
    ticket: Option<String>,
    short_code: Option<String>,
    file_name: Option<String>,
    file_path: Option<PathBuf>,
    progress_tx: mpsc::Sender<ProgressUpdate>,
    created_at: Instant,
    completed_at: Option<Instant>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
enum TransferStatus {
    Pending,
    Waiting,
    Connected,
    Transferring { bytes: u64, total: u64 },
    Complete { path: Option<String> },
    Error { message: String },
}

#[derive(Clone, Debug, Serialize)]
struct ProgressUpdate {
    status: TransferStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
}

pub async fn run(addr: SocketAddr) -> Result<()> {
    let temp_dir = std::env::var("ZAP_TEMP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("zap-uploads"));

    fs::create_dir_all(&temp_dir).await?;
    info!("using temp directory: {}", temp_dir.display());

    let state = AppState {
        transfers: Arc::new(RwLock::new(HashMap::new())),
        ticket_codes: Arc::new(RwLock::new(HashMap::new())),
        temp_dir,
    };

    // Start background cleanup task
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        cleanup_loop(cleanup_state).await;
    });

    // Configure CORS for production
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);


    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/install", get(install_page))
        .route("/install.sh", get(install_script))
        .route("/send", post(handle_send))
        .route("/receive", post(handle_receive))
        .route("/ws/{id}", get(handle_websocket))
        .route("/download/{id}", get(handle_download))
        // API routes for CLI support
        .route("/api/register", post(api_register_ticket))
        .route("/api/lookup/{code}", get(api_lookup_ticket))
        .with_state(state)
        .layer(DefaultBodyLimit::max(MAX_FILE_SIZE))
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    info!("zap web server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown on SIGTERM
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("server shut down gracefully");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received Ctrl+C, shutting down"),
        _ = terminate => info!("received SIGTERM, shutting down"),
    }
}

async fn cleanup_loop(state: AppState) {
    let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
    loop {
        interval.tick().await;
        cleanup_old_transfers(&state).await;
    }
}

async fn cleanup_old_transfers(state: &AppState) {
    let now = Instant::now();
    let mut to_remove = Vec::new();

    {
        let transfers = state.transfers.read().await;
        for (id, transfer) in transfers.iter() {
            let should_remove = match transfer.completed_at {
                Some(completed) => now.duration_since(completed) > TRANSFER_TTL,
                None => now.duration_since(transfer.created_at) > TRANSFER_TTL * 2,
            };

            if should_remove {
                to_remove.push((id.clone(), transfer.file_path.clone()));
            }
        }
    }

    if !to_remove.is_empty() {
        info!("cleaning up {} old transfers", to_remove.len());

        let mut transfers = state.transfers.write().await;
        for (id, file_path) in to_remove {
            transfers.remove(&id);

            // Clean up files
            if let Some(path) = file_path {
                if let Some(parent) = path.parent() {
                    if parent.starts_with(&state.temp_dir) {
                        if let Err(e) = fs::remove_dir_all(parent).await {
                            warn!("failed to remove temp dir {:?}: {}", parent, e);
                        }
                    }
                }
            }
        }
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> &'static str {
    "ok"
}


async fn install_page() -> Html<&'static str> {
    Html(INSTALL_HTML)
}

async fn install_script() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/x-shellscript")],
        INSTALL_SCRIPT,
    )
        .into_response()
}

/// Returns update script if available (empty string means no update)
/// Check UPDATE_SCRIPT env var or /var/lib/zap/update.sh file

async fn ready(State(state): State<AppState>) -> Response {
    // Check if we can access the temp directory
    let test_file = state.temp_dir.join(".ready-check");
    match fs::write(&test_file, b"ok").await {
        Ok(_) => {
            let _ = fs::remove_file(&test_file).await;
            "ready".into_response()
        }
        Err(e) => {
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, format!("not ready: {}", e))
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct ReceiveForm {
    ticket: String,
}

async fn handle_send(State(state): State<AppState>, mut multipart: Multipart) -> Response {
    let transfer_id = Uuid::new_v4().to_string();
    let transfer_dir = state.temp_dir.join(&transfer_id);

    if let Err(e) = fs::create_dir_all(&transfer_dir).await {
        return Html(format!(
            r##"<div class="text-red-400">Error creating directory: {}</div>"##,
            e
        ))
        .into_response();
    }

    // Stream file to disk instead of loading into memory
    let mut file_name = None;
    let mut file_path = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            let name = field.file_name().unwrap_or("file").to_string();
            let path = transfer_dir.join(&name);

            // Stream to file
            match stream_to_file(field, &path).await {
                Ok(_) => {
                    file_name = Some(name);
                    file_path = Some(path);
                }
                Err(e) => {
                    let _ = fs::remove_dir_all(&transfer_dir).await;
                    return Html(format!(
                        r##"<div class="text-red-400">Error saving file: {}</div>"##,
                        e
                    ))
                    .into_response();
                }
            }
            break;
        }
    }

    let (file_name, file_path) = match (file_name, file_path) {
        (Some(n), Some(p)) => (n, p),
        _ => {
            let _ = fs::remove_dir_all(&transfer_dir).await;
            return Html(r##"<div class="text-red-400">No file provided</div>"##.to_string())
                .into_response();
        }
    };

    // Create progress channel
    let (progress_tx, _) = mpsc::channel(32);

    // Store transfer state
    {
        let mut transfers = state.transfers.write().await;
        transfers.insert(
            transfer_id.clone(),
            TransferState {
                status: TransferStatus::Pending,
                ticket: None,
                short_code: None,
                file_name: Some(file_name.clone()),
                file_path: Some(file_path),
                progress_tx,
                created_at: Instant::now(),
                completed_at: None,
            },
        );
    }

    Html(format!(
        r##"
        <div id="transfer-status" class="text-center">
            <div id="status-text" class="animate-pulse text-gray-400 mb-4">Starting transfer...</div>
            <div class="text-sm text-gray-500 mb-4">File: {file_name}</div>
            <div id="code-display" class="hidden">
                <p class="text-sm text-gray-400 mb-3">Share this code with the receiver:</p>
                <div class="flex items-center justify-center gap-3">
                    <code id="short-code" class="text-3xl font-bold tracking-widest text-cyan-400 bg-gray-800 px-6 py-3 rounded-lg"></code>
                    <button onclick="navigator.clipboard.writeText(document.getElementById('short-code').textContent); this.textContent='Copied!'; setTimeout(() => this.textContent='Copy', 1500)"
                            class="px-4 py-2 bg-cyan-600 hover:bg-cyan-500 rounded-lg text-sm font-medium transition">Copy</button>
                </div>
            </div>
            <div id="progress-bar" class="hidden mt-4 w-full bg-gray-700 rounded-full h-2">
                <div id="progress-fill" class="bg-cyan-500 h-2 rounded-full transition-all" style="width: 0%"></div>
            </div>
        </div>
        <script>
            (function() {{
                let completed = false;
                const wsUrl = (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws/{transfer_id}';
                const ws = new WebSocket(wsUrl);
                ws.onmessage = function(event) {{
                    const data = JSON.parse(event.data);
                    const statusText = document.getElementById('status-text');
                    const codeDisplay = document.getElementById('code-display');
                    const shortCode = document.getElementById('short-code');
                    const progressBar = document.getElementById('progress-bar');
                    const progressFill = document.getElementById('progress-fill');

                    if (data.short_code) {{
                        shortCode.textContent = data.short_code;
                        codeDisplay.classList.remove('hidden');
                    }}

                    switch(data.status.type) {{
                        case 'Waiting':
                            statusText.textContent = 'Waiting for receiver...';
                            statusText.className = 'animate-pulse text-yellow-400 mb-4';
                            break;
                        case 'Connected':
                            statusText.textContent = 'Receiver connected! Transferring...';
                            statusText.className = 'text-cyan-400 mb-4';
                            codeDisplay.classList.add('hidden');
                            progressBar.classList.remove('hidden');
                            break;
                        case 'Transferring':
                            const pct = Math.round((data.status.bytes / data.status.total) * 100);
                            statusText.textContent = 'Transferring... ' + pct + '%';
                            progressFill.style.width = pct + '%';
                            break;
                        case 'Complete':
                            completed = true;
                            statusText.textContent = 'Transfer complete!';
                            statusText.className = 'text-green-400 mb-4';
                            progressFill.style.width = '100%';
                            break;
                        case 'Error':
                            statusText.textContent = 'Error: ' + data.status.message;
                            statusText.className = 'text-red-400 mb-4';
                            break;
                    }}
                }};
                ws.onerror = function() {{
                    if (!completed) {{
                        document.getElementById('status-text').textContent = 'Connection error';
                        document.getElementById('status-text').className = 'text-red-400 mb-4';
                    }}
                }};
                ws.onclose = function() {{
                    if (!completed) {{
                        document.getElementById('status-text').textContent = 'Connection closed';
                        document.getElementById('status-text').className = 'text-red-400 mb-4';
                    }}
                }};
            }})();
        </script>
        "##
    ))
    .into_response()
}

async fn stream_to_file(
    mut field: axum::extract::multipart::Field<'_>,
    path: &std::path::Path,
) -> Result<u64, std::io::Error> {
    let mut file = File::create(path).await?;
    let mut total = 0u64;

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
    {
        file.write_all(&chunk).await?;
        total += chunk.len() as u64;
    }

    file.flush().await?;
    Ok(total)
}

async fn handle_receive(
    State(state): State<AppState>,
    axum::Form(form): axum::Form<ReceiveForm>,
) -> Response {
    let transfer_id = Uuid::new_v4().to_string();
    let input = form.ticket.trim().to_lowercase();

    // Check if input is a short code (6 alphanumeric chars) or full ticket
    let ticket_str = if input.len() <= 8 && input.chars().all(|c| c.is_alphanumeric()) {
        // Look up short code (case-insensitive)
        let codes = state.ticket_codes.read().await;
        match codes.get(&input) {
            Some(full_ticket) => full_ticket.clone(),
            None => {
                return Html(r##"<div class="text-red-400">Invalid code. Please check and try again.</div>"##.to_string())
                    .into_response();
            }
        }
    } else {
        input.to_string()
    };

    // Validate ticket
    // Validate ticket format (actual transfer will be started when WebSocket connects)
    let _ticket = match Ticket::deserialize(&ticket_str) {
        Ok(t) => t,
        Err(e) => {
            return Html(format!(
                r##"<div class="text-red-400">Invalid ticket: {}</div>"##,
                e
            ))
            .into_response();
        }
    };

    // Create progress channel
    let (progress_tx, _) = mpsc::channel(32);

    // Store transfer state (use ticket_str which is the full ticket after short code lookup)
    {
        let mut transfers = state.transfers.write().await;
        transfers.insert(
            transfer_id.clone(),
            TransferState {
                status: TransferStatus::Pending,
                ticket: Some(ticket_str),
                short_code: None,
                file_name: None,
                file_path: None,
                progress_tx,
                created_at: Instant::now(),
                completed_at: None,
            },
        );
    }

    // Note: receive task will be started when WebSocket connects (in handle_socket)
    // This ensures progress updates are sent to the correct channel

    Html(format!(
        r##"
        <div id="recv-transfer-status" class="text-center">
            <div id="recv-status-text" class="animate-pulse text-gray-400 mb-4">Connecting to sender...</div>
            <div id="recv-progress-bar" class="hidden mt-4 w-full bg-gray-700 rounded-full h-2">
                <div id="recv-progress-fill" class="bg-purple-500 h-2 rounded-full transition-all" style="width: 0%"></div>
            </div>
            <div id="recv-download-link" class="hidden mt-4"></div>
        </div>
        <script>
            (function() {{
                let completed = false;
                const wsUrl = (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws/{transfer_id}';
                const ws = new WebSocket(wsUrl);
                ws.onmessage = function(event) {{
                    const data = JSON.parse(event.data);
                    const statusText = document.getElementById('recv-status-text');
                    const progressBar = document.getElementById('recv-progress-bar');
                    const progressFill = document.getElementById('recv-progress-fill');
                    const downloadLink = document.getElementById('recv-download-link');

                    switch(data.status.type) {{
                        case 'Connected':
                            statusText.textContent = 'Connected! Receiving file...';
                            statusText.className = 'text-purple-400 mb-4';
                            progressBar.classList.remove('hidden');
                            break;
                        case 'Transferring':
                            const pct = Math.round((data.status.bytes / data.status.total) * 100);
                            statusText.textContent = 'Receiving... ' + pct + '%';
                            progressFill.style.width = pct + '%';
                            break;
                        case 'Complete':
                            completed = true;
                            statusText.textContent = 'Transfer complete!';
                            statusText.className = 'text-green-400 mb-4';
                            progressFill.style.width = '100%';
                            if (data.status.path) {{
                                downloadLink.innerHTML = '<a href="/download/{transfer_id}" class="inline-block mt-2 px-6 py-2 bg-purple-600 hover:bg-purple-500 rounded-lg font-medium">Download ' + (data.file_name || 'File') + '</a>';
                                downloadLink.classList.remove('hidden');
                            }}
                            break;
                        case 'Error':
                            statusText.textContent = 'Error: ' + data.status.message;
                            statusText.className = 'text-red-400 mb-4';
                            break;
                    }}
                }};
                ws.onerror = function() {{
                    if (!completed) {{
                        document.getElementById('recv-status-text').textContent = 'Connection error';
                        document.getElementById('recv-status-text').className = 'text-red-400 mb-4';
                    }}
                }};
                ws.onclose = function() {{
                    if (!completed) {{
                        document.getElementById('recv-status-text').textContent = 'Connection closed';
                        document.getElementById('recv-status-text').className = 'text-red-400 mb-4';
                    }}
                }};
            }})();
        </script>
        "##
    ))
    .into_response()
}

async fn handle_websocket(
    State(state): State<AppState>,
    Path(transfer_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, transfer_id))
}

async fn handle_socket(mut socket: WebSocket, state: AppState, transfer_id: String) {
    debug!("WebSocket connected for transfer {}", transfer_id);

    // Create a new channel for this WebSocket connection FIRST
    let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(32);

    // Check what kind of transfer this is and update channel
    let (should_start_send, should_start_receive, ticket_str) = {
        let mut transfers = state.transfers.write().await;
        if let Some(transfer) = transfers.get_mut(&transfer_id) {
            // Update channel before starting any transfer
            transfer.progress_tx = tx;

            let is_send = matches!(transfer.status, TransferStatus::Pending) && transfer.file_path.is_some();
            let is_receive = matches!(transfer.status, TransferStatus::Pending) && transfer.ticket.is_some() && transfer.file_path.is_none();
            let ticket = transfer.ticket.clone();
            (is_send, is_receive, ticket)
        } else {
            (false, false, None)
        }
    };

    if should_start_send {
        // Generate secret key before spawning (ThreadRng is !Send)
        let secret_key = SecretKey::generate(&mut rand::rng());

        let state_clone = state.clone();
        let transfer_id_clone = transfer_id.clone();
        tokio::spawn(async move {
            run_send_transfer(state_clone, transfer_id_clone, secret_key).await;
        });
    }

    if should_start_receive {
        if let Some(ticket_string) = ticket_str {
            if let Ok(ticket) = Ticket::deserialize(&ticket_string) {
                let secret_key = SecretKey::generate(&mut rand::rng());
                let state_clone = state.clone();
                let transfer_id_clone = transfer_id.clone();
                tokio::spawn(async move {
                    run_receive_transfer(state_clone, transfer_id_clone, ticket, secret_key).await;
                });
            }
        }
    }

    // Listen for progress updates and send to WebSocket
    loop {
        tokio::select! {
            Some(update) = rx.recv() => {
                let html = render_progress(&update);
                if socket.send(Message::Text(html.into())).await.is_err() {
                    break;
                }

                // Check if transfer is complete
                if matches!(update.status, TransferStatus::Complete { .. } | TransferStatus::Error { .. }) {
                    break;
                }
            }
            else => break,
        }
    }
}

async fn handle_download(
    State(state): State<AppState>,
    Path(transfer_id): Path<String>,
) -> Response {
    let transfers = state.transfers.read().await;
    if let Some(transfer) = transfers.get(&transfer_id) {
        if let Some(ref path) = transfer.file_path {
            if path.exists() {
                let file_name = transfer
                    .file_name
                    .clone()
                    .unwrap_or_else(|| "file".to_string());

                // Use tokio_util for streaming instead of loading into memory
                match File::open(path).await {
                    Ok(file) => {
                        let stream = tokio_util::io::ReaderStream::new(file);
                        let body = axum::body::Body::from_stream(stream);

                        return (
                            [
                                (
                                    axum::http::header::CONTENT_TYPE,
                                    "application/octet-stream",
                                ),
                                (
                                    axum::http::header::CONTENT_DISPOSITION,
                                    &format!("attachment; filename=\"{}\"", file_name),
                                ),
                            ],
                            body,
                        )
                            .into_response();
                    }
                    Err(e) => {
                        return Html(format!("Error reading file: {}", e)).into_response();
                    }
                }
            }
        }
    }
    (axum::http::StatusCode::NOT_FOUND, "File not found").into_response()
}

// ============ API Handlers for CLI Support ============

#[derive(Deserialize)]
struct RegisterTicketRequest {
    ticket: String,
    #[serde(default)]
    #[allow(dead_code)]
    file_name: Option<String>,
}

#[derive(Serialize)]
struct RegisterTicketResponse {
    code: String,
    words: String,
}

#[derive(Serialize)]
struct LookupTicketResponse {
    ticket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
}

/// API endpoint for CLI to register a ticket and get a short code
async fn api_register_ticket(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<RegisterTicketRequest>,
) -> Response {
    // Validate the ticket is parseable
    if Ticket::deserialize(&req.ticket).is_err() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "Invalid ticket format"})),
        )
            .into_response();
    }

    // Generate short code
    let short_code = generate_short_code();
    let words = code_to_words(&short_code);

    // Store the mapping
    {
        let mut codes = state.ticket_codes.write().await;
        codes.insert(short_code.clone(), req.ticket.clone());
    }

    axum::Json(RegisterTicketResponse {
        code: short_code,
        words,
    })
    .into_response()
}

/// API endpoint for CLI to look up a ticket by short code or words
async fn api_lookup_ticket(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Response {
    // Normalize: could be a short code or word-based code
    let lookup_code = if code.contains('-') {
        // Word-based code like "apple-banana-cherry"
        words_to_code(&code)
    } else {
        code.to_lowercase()
    };

    let codes = state.ticket_codes.read().await;
    match codes.get(&lookup_code) {
        Some(ticket) => axum::Json(LookupTicketResponse {
            ticket: ticket.clone(),
            file_name: None,
        })
        .into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "Code not found or expired"})),
        )
            .into_response(),
    }
}

/// Convert a short code to human-readable words
fn code_to_words(code: &str) -> String {
    // Simple word list - easy to spell, no ambiguity
    const WORDS: &[&str] = &[
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
        "quebec", "romeo", "sierra", "tango", "uniform", "victor", "whiskey",
        "xray", "yankee", "zulu", "zero", "one", "two", "three", "four", "five",
    ];

    code.chars()
        .filter_map(|c| {
            let idx = match c {
                'a'..='z' => (c as usize) - ('a' as usize),
                '2'..='9' => 26 + (c as usize) - ('2' as usize),
                _ => return None,
            };
            WORDS.get(idx).copied()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Convert word-based code back to short code
fn words_to_code(words: &str) -> String {
    const WORDS: &[&str] = &[
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
        "quebec", "romeo", "sierra", "tango", "uniform", "victor", "whiskey",
        "xray", "yankee", "zulu", "zero", "one", "two", "three", "four", "five",
    ];

    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz23456789";

    words
        .split('-')
        .filter_map(|word| {
            let word_lower = word.to_lowercase();
            WORDS.iter().position(|&w| w == word_lower).map(|idx| {
                if idx < CHARSET.len() {
                    CHARSET[idx] as char
                } else {
                    '?'
                }
            })
        })
        .collect()
}

async fn run_send_transfer(state: AppState, transfer_id: String, secret_key: SecretKey) {
    let file_path = {
        let transfers = state.transfers.read().await;
        transfers
            .get(&transfer_id)
            .and_then(|t| t.file_path.clone())
    };

    let file_path = match file_path {
        Some(p) => p,
        None => return,
    };

    let node = match ZapNode::with_secret_key(secret_key).await {
        Ok(n) => n,
        Err(e) => {
            update_transfer_status(
                &state,
                &transfer_id,
                TransferStatus::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    let (ticket, mut progress_rx) = match node.send(&file_path).await {
        Ok(r) => r,
        Err(e) => {
            update_transfer_status(
                &state,
                &transfer_id,
                TransferStatus::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    // Generate short code and store ticket mapping
    let short_code = generate_short_code();
    let ticket_str = ticket.to_string();

    {
        let mut codes = state.ticket_codes.write().await;
        codes.insert(short_code.clone(), ticket_str.clone());
    }

    {
        let mut transfers = state.transfers.write().await;
        if let Some(transfer) = transfers.get_mut(&transfer_id) {
            transfer.ticket = Some(ticket_str);
            transfer.short_code = Some(short_code);
        }
    }

    // Send waiting status with short code
    update_transfer_status(&state, &transfer_id, TransferStatus::Waiting).await;

    // Process progress updates
    while let Some(progress) = progress_rx.recv().await {
        let status = match progress {
            SendProgress::Waiting => TransferStatus::Waiting,
            SendProgress::Connected => TransferStatus::Connected,
            SendProgress::Sending {
                bytes_sent,
                total_bytes,
            } => TransferStatus::Transferring {
                bytes: bytes_sent,
                total: total_bytes,
            },
            SendProgress::Complete => TransferStatus::Complete { path: None },
            SendProgress::Error(msg) => TransferStatus::Error { message: msg },
        };

        let is_terminal = matches!(
            status,
            TransferStatus::Complete { .. } | TransferStatus::Error { .. }
        );

        update_transfer_status(&state, &transfer_id, status).await;

        if is_terminal {
            // Mark as completed for cleanup
            let mut transfers = state.transfers.write().await;
            if let Some(transfer) = transfers.get_mut(&transfer_id) {
                transfer.completed_at = Some(Instant::now());
            }
            break;
        }
    }

    let _ = node.shutdown().await;
}

async fn run_receive_transfer(
    state: AppState,
    transfer_id: String,
    ticket: Ticket,
    secret_key: SecretKey,
) {
    let output_dir = state.temp_dir.join(&transfer_id);
    if let Err(e) = fs::create_dir_all(&output_dir).await {
        update_transfer_status(
            &state,
            &transfer_id,
            TransferStatus::Error {
                message: e.to_string(),
            },
        )
        .await;
        return;
    }

    let node = match ZapNode::with_secret_key(secret_key).await {
        Ok(n) => n,
        Err(e) => {
            update_transfer_status(
                &state,
                &transfer_id,
                TransferStatus::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    let mut progress_rx = match node.receive(ticket, Some(&output_dir)).await {
        Ok(r) => r,
        Err(e) => {
            update_transfer_status(
                &state,
                &transfer_id,
                TransferStatus::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    while let Some(progress) = progress_rx.recv().await {
        let status = match &progress {
            ReceiveProgress::Connecting => TransferStatus::Pending,
            ReceiveProgress::Connected => TransferStatus::Connected,
            ReceiveProgress::Offer { name, size: _ } => {
                // Update file name
                {
                    let mut transfers = state.transfers.write().await;
                    if let Some(transfer) = transfers.get_mut(&transfer_id) {
                        transfer.file_name = Some(name.clone());
                    }
                }
                TransferStatus::Connected
            }
            ReceiveProgress::Receiving {
                bytes_received,
                total_bytes,
            } => TransferStatus::Transferring {
                bytes: *bytes_received,
                total: *total_bytes,
            },
            ReceiveProgress::Complete { path } => {
                // Update file path
                {
                    let mut transfers = state.transfers.write().await;
                    if let Some(transfer) = transfers.get_mut(&transfer_id) {
                        transfer.file_path = Some(path.clone());
                        transfer.completed_at = Some(Instant::now());
                    }
                }
                TransferStatus::Complete {
                    path: Some(format!("/download/{}", transfer_id)),
                }
            }
            ReceiveProgress::Error(msg) => TransferStatus::Error {
                message: msg.clone(),
            },
        };

        let is_terminal = matches!(
            status,
            TransferStatus::Complete { .. } | TransferStatus::Error { .. }
        );

        update_transfer_status(&state, &transfer_id, status).await;

        if is_terminal {
            break;
        }
    }

    let _ = node.shutdown().await;
}

async fn update_transfer_status(state: &AppState, transfer_id: &str, status: TransferStatus) {
    let mut transfers = state.transfers.write().await;
    if let Some(transfer) = transfers.get_mut(transfer_id) {
        transfer.status = status.clone();

        let update = ProgressUpdate {
            status,
            short_code: transfer.short_code.clone(),
            file_name: transfer.file_name.clone(),
        };

        // Try to send, ignore if channel is closed
        let _ = transfer.progress_tx.try_send(update);
    }
}

fn render_progress(update: &ProgressUpdate) -> String {
    // Return JSON for plain JavaScript WebSocket handler
    serde_json::to_string(update).unwrap_or_else(|_| r#"{"status":{"type":"Error","message":"Serialization failed"}}"#.to_string())
}

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
    <title>zap ⚡ send files instantly</title>
    <link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>⚡</text></svg>">
    <link href="https://fonts.googleapis.com/css2?family=Caveat:wght@400;500;600;700&family=Patrick+Hand&display=swap" rel="stylesheet">
    <script src="https://cdn.tailwindcss.com"></script>
    <script src="https://unpkg.com/htmx.org@2.0.4"></script>
    <script src="https://unpkg.com/roughjs@4.6.6/bundled/rough.js"></script>
    <style>
        :root {
            --paper: #faf8f5;
            --paper-lines: #e8e4dd;
            --ink: #2d3748;
            --ink-light: #718096;
            --accent-blue: #4299e1;
            --accent-yellow: #f6e05e;
            --accent-green: #68d391;
            --accent-purple: #b794f4;
            --accent-red: #fc8181;
        }
        * { -webkit-tap-highlight-color: transparent; }
        body { 
            font-family: 'Patrick Hand', cursive;
            background: var(--paper);
            background-image: 
                linear-gradient(var(--paper-lines) 1px, transparent 1px);
            background-size: 100% 28px;
            min-height: 100dvh;
            color: var(--ink);
        }
        .font-title { font-family: 'Caveat', cursive; }
        
        /* Sketchy card styles */
        .sketch-card {
            background: rgba(255,255,255,0.7);
            border: 3px solid var(--ink);
            border-radius: 3px;
            position: relative;
            transform: rotate(-0.5deg);
            box-shadow: 4px 4px 0 rgba(0,0,0,0.1);
        }
        .sketch-card:nth-child(2) { transform: rotate(0.5deg); }
        .sketch-card::before {
            content: '';
            position: absolute;
            top: -2px; left: -2px; right: -2px; bottom: -2px;
            border: 2px solid var(--ink);
            border-radius: 5px;
            opacity: 0.3;
            transform: translate(2px, 2px);
        }
        
        /* Wobbly animations */
        @keyframes wobble {
            0%, 100% { transform: rotate(-0.5deg); }
            50% { transform: rotate(0.5deg); }
        }
        @keyframes draw-in {
            from { stroke-dashoffset: 1000; opacity: 0; }
            to { stroke-dashoffset: 0; opacity: 1; }
        }
        .wobble { animation: wobble 3s ease-in-out infinite; }
        .draw-in { 
            stroke-dasharray: 1000;
            animation: draw-in 1s ease-out forwards;
        }
        
        /* Highlighter hover effect */
        .highlight-hover {
            position: relative;
            transition: all 0.2s;
        }
        .highlight-hover::after {
            content: '';
            position: absolute;
            bottom: 0; left: -4px; right: -4px;
            height: 40%;
            background: var(--accent-yellow);
            opacity: 0;
            z-index: -1;
            transform: skew(-5deg) rotate(-1deg);
            transition: opacity 0.2s;
        }
        .highlight-hover:hover::after { opacity: 0.6; }
        
        /* Sketch button */
        .sketch-btn {
            background: var(--accent-blue);
            color: white;
            border: 3px solid var(--ink);
            border-radius: 4px;
            font-family: 'Caveat', cursive;
            font-size: 1.4rem;
            font-weight: 600;
            padding: 12px 24px;
            transform: rotate(-1deg);
            transition: all 0.15s;
            box-shadow: 3px 3px 0 var(--ink);
        }
        .sketch-btn:hover {
            transform: rotate(0deg) translateY(-2px);
            box-shadow: 5px 5px 0 var(--ink);
        }
        .sketch-btn:active {
            transform: rotate(0deg) translateY(2px);
            box-shadow: 1px 1px 0 var(--ink);
        }
        .sketch-btn.purple { background: var(--accent-purple); }
        
        /* Sketchy input */
        .sketch-input {
            background: white;
            border: 2px solid var(--ink);
            border-radius: 3px;
            font-family: 'Patrick Hand', cursive;
            font-size: 1.5rem;
            padding: 16px;
            transform: rotate(0.3deg);
        }
        .sketch-input:focus {
            outline: none;
            box-shadow: 0 0 0 3px var(--accent-yellow);
        }
        
        /* Drop zone */
        .sketch-drop {
            border: 3px dashed var(--ink-light);
            border-radius: 4px;
            background: repeating-linear-gradient(
                -45deg,
                transparent,
                transparent 10px,
                rgba(0,0,0,0.02) 10px,
                rgba(0,0,0,0.02) 20px
            );
            transition: all 0.2s;
        }
        .sketch-drop:hover, .sketch-drop.dragover {
            border-color: var(--accent-blue);
            background: rgba(66, 153, 225, 0.1);
        }
        
        /* Tab bookmark style */
        .tab-bookmark {
            background: var(--paper);
            border: 2px solid var(--ink);
            border-bottom: none;
            border-radius: 8px 8px 0 0;
            padding: 8px 20px;
            margin-bottom: -2px;
            position: relative;
            font-family: 'Caveat', cursive;
            font-size: 1.2rem;
            color: var(--ink-light);
            transition: all 0.2s;
        }
        .tab-bookmark.active {
            background: white;
            color: var(--ink);
            z-index: 10;
        }
        .tab-bookmark:hover:not(.active) {
            background: #fff9e6;
        }
        
        /* Code block */
        .code-sketch {
            background: #2d3748;
            color: #68d391;
            border: 3px solid var(--ink);
            border-radius: 4px;
            font-family: monospace;
            padding: 16px;
            transform: rotate(-0.3deg);
        }
        
        /* Doodle decorations */
        .doodle-arrow {
            position: absolute;
            width: 60px;
            height: 40px;
        }
        
        /* Feature icons hand-drawn style */
        .feature-icon {
            width: 64px;
            height: 64px;
            border: 3px solid var(--ink);
            border-radius: 50%;
            display: flex;
            align-items: center;
            justify-content: center;
            font-size: 2rem;
            background: white;
            transform: rotate(-3deg);
        }
        
        /* Scribble underline */
        .scribble-underline {
            position: relative;
        }
        .scribble-underline::after {
            content: '';
            position: absolute;
            bottom: -4px;
            left: 0;
            width: 100%;
            height: 8px;
            background: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 10'%3E%3Cpath d='M0,5 Q25,0 50,5 T100,5' stroke='%234299e1' stroke-width='3' fill='none'/%3E%3C/svg%3E") repeat-x;
            background-size: 100px 10px;
        }
    </style>
</head>
<body class="min-h-screen">
    <div class="max-w-4xl mx-auto px-4 py-8 md:py-12">
        <!-- Header -->
        <header class="text-center mb-12">
            <h1 class="font-title text-6xl md:text-8xl text-ink mb-2 wobble">
                ⚡ zap
            </h1>
            <p class="text-xl md:text-2xl text-ink-light">
                <span class="scribble-underline">send files</span> to anyone, instantly!
            </p>
        </header>

        <!-- Main transfer cards -->
        <div class="grid md:grid-cols-2 gap-6 md:gap-8 mb-16">
            <!-- Send Card -->
            <div class="sketch-card p-6 md:p-8">
                <div class="flex items-center gap-3 mb-6">
                    <div class="feature-icon" style="transform: rotate(3deg);">📤</div>
                    <h2 class="font-title text-3xl">Send a file</h2>
                </div>
                <form hx-post="/send" hx-target="#send-result" hx-swap="innerHTML" hx-encoding="multipart/form-data">
                    <div id="drop-zone" class="sketch-drop rounded-lg p-8 text-center cursor-pointer mb-4" onclick="document.getElementById('file-input').click()">
                        <input type="file" name="file" id="file-input" required class="hidden" onchange="updateFileName(this)">
                        <div class="text-5xl mb-3">📁</div>
                        <p id="file-name" class="text-lg text-ink-light">click or drop a file here!</p>
                    </div>
                    <button type="submit" class="sketch-btn w-full">
                        Send it! →
                    </button>
                </form>
                <div id="send-result" class="mt-4"></div>
            </div>

            <!-- Receive Card -->
            <div class="sketch-card p-6 md:p-8">
                <div class="flex items-center gap-3 mb-6">
                    <div class="feature-icon" style="transform: rotate(-5deg);">📥</div>
                    <h2 class="font-title text-3xl">Get a file</h2>
                </div>
                <form hx-post="/receive" hx-target="#receive-result" hx-swap="innerHTML">
                    <div class="mb-4">
                        <label class="block text-lg mb-2 text-ink-light">got a code? paste it here:</label>
                        <input name="ticket" required placeholder="abc123" 
                            class="sketch-input w-full text-center tracking-widest"
                            maxlength="10" autocomplete="off" autocorrect="off" autocapitalize="off" spellcheck="false">
                    </div>
                    <button type="submit" class="sketch-btn purple w-full">
                        Get it! ←
                    </button>
                </form>
                <div id="receive-result" class="mt-4"></div>
            </div>
        </div>

        <!-- Features (hand-drawn style) -->
        <div class="sketch-card p-6 mb-12" style="transform: rotate(0.3deg);">
            <h3 class="font-title text-2xl text-center mb-6">why zap? ✨</h3>
            <div class="grid grid-cols-3 gap-4 text-center">
                <div>
                    <div class="text-3xl mb-2">🔐</div>
                    <div class="font-title text-xl">encrypted</div>
                    <div class="text-sm text-ink-light">end-to-end</div>
                </div>
                <div>
                    <div class="text-3xl mb-2">🚀</div>
                    <div class="font-title text-xl">fast</div>
                    <div class="text-sm text-ink-light">peer-to-peer</div>
                </div>
                <div>
                    <div class="text-3xl mb-2">🌍</div>
                    <div class="font-title text-xl">anywhere</div>
                    <div class="text-sm text-ink-light">works everywhere</div>
                </div>
            </div>
        </div>

        <!-- CLI Section -->
        <div class="sketch-card p-6 md:p-8 mb-8" style="transform: rotate(-0.2deg);">
            <h3 class="font-title text-2xl mb-2">
                psst... 🤫 there's a CLI too!
            </h3>
            <p class="text-ink-light mb-6">even faster from your terminal</p>
            
            <!-- Bookmark tabs -->
            <div class="flex gap-1 mb-0">
                <button onclick="showTab('mac')" id="tab-mac" class="tab-bookmark active">🍎 macOS</button>
                <button onclick="showTab('linux')" id="tab-linux" class="tab-bookmark">🐧 Linux</button>
            </div>
            
            <div class="bg-white border-2 border-ink rounded-lg rounded-tl-none p-4">
                <div id="content-mac" class="tab-content">
                    <div class="code-sketch flex items-center justify-between">
                        <code>brew install voidash/tap/zap</code>
                        <button onclick="copyText('brew install voidash/tap/zap', this)" class="text-accent-yellow hover:text-white ml-4">
                            📋
                        </button>
                    </div>
                </div>
                <div id="content-linux" class="tab-content hidden">
                    <div class="code-sketch flex items-center justify-between">
                        <code class="text-sm">curl -fsSL https://zapper.cloud/install.sh | sh</code>
                        <button onclick="copyText('curl -fsSL https://zapper.cloud/install.sh | sh', this)" class="text-accent-yellow hover:text-white ml-4">
                            📋
                        </button>
                    </div>
                </div>
                
                <!-- Usage examples -->
                <div class="mt-6 pt-6 border-t-2 border-dashed border-ink-light">
                    <h4 class="font-title text-xl mb-4">how to use:</h4>
                    <div class="grid md:grid-cols-2 gap-4">
                        <div>
                            <div class="text-sm text-ink-light mb-1">→ send a file:</div>
                            <div class="code-sketch text-sm">
                                <div><span class="text-accent-yellow">$</span> zap send photo.jpg</div>
                                <div class="text-ink-light mt-1">Code: <span class="text-accent-green">abc123</span></div>
                            </div>
                        </div>
                        <div>
                            <div class="text-sm text-ink-light mb-1">→ receive a file:</div>
                            <div class="code-sketch text-sm">
                                <div><span class="text-accent-yellow">$</span> zap receive abc123</div>
                                <div class="text-ink-light mt-1">Saved: <span class="text-accent-green">photo.jpg</span></div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </div>

        <!-- Footer -->
        <footer class="text-center text-ink-light py-8">
            <p>
                made with ♥ • powered by 
                <a href="https://iroh.computer" class="highlight-hover text-ink">iroh</a> • 
                <a href="https://github.com/voidash/zapper.cloud" class="highlight-hover text-ink">github</a>
            </p>
        </footer>
    </div>

    <script>
        // File selection
        function updateFileName(input) {
            const name = input.files[0]?.name;
            document.getElementById('file-name').textContent = name ? '📄 ' + name : 'click or drop a file here!';
        }

        // Drag and drop
        const dropZone = document.getElementById('drop-zone');
        ['dragenter', 'dragover'].forEach(e => {
            dropZone.addEventListener(e, (ev) => { ev.preventDefault(); dropZone.classList.add('dragover'); });
        });
        ['dragleave', 'drop'].forEach(e => {
            dropZone.addEventListener(e, (ev) => { ev.preventDefault(); dropZone.classList.remove('dragover'); });
        });
        dropZone.addEventListener('drop', (e) => {
            const file = e.dataTransfer.files[0];
            if (file) {
                document.getElementById('file-input').files = e.dataTransfer.files;
                updateFileName(document.getElementById('file-input'));
            }
        });

        // Tabs
        function showTab(tab) {
            document.querySelectorAll('.tab-bookmark').forEach(b => b.classList.remove('active'));
            document.querySelectorAll('.tab-content').forEach(c => c.classList.add('hidden'));
            document.getElementById('tab-' + tab).classList.add('active');
            document.getElementById('content-' + tab).classList.remove('hidden');
        }

        // Copy with feedback
        function copyText(text, btn) {
            navigator.clipboard.writeText(text);
            const original = btn.textContent;
            btn.textContent = '✓';
            setTimeout(() => btn.textContent = original, 1500);
        }
    </script>
</body>
</html>"##;

const INSTALL_HTML: &str = r##"<!DOCTYPE html>
<html lang="en" class="dark">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Install zap - Fast File Transfer</title>
    <script src="https://cdn.tailwindcss.com"></script>
    <style>
        body { background: #0f0f0f; }
        pre { background: #1a1a1a; }
    </style>
</head>
<body class="min-h-screen text-gray-100">
    <div class="container mx-auto px-4 py-16 max-w-3xl">
        <header class="text-center mb-12">
            <h1 class="text-5xl font-bold mb-2">
                <span class="text-cyan-400">zap</span>
            </h1>
            <p class="text-gray-400">Fast, secure file transfers</p>
        </header>

        <div class="space-y-8">
            <!-- Quick Install -->
            <div class="bg-gray-900 rounded-lg p-6 border border-gray-800">
                <h2 class="text-2xl font-semibold mb-4 text-cyan-400">Quick Install</h2>
                <p class="text-gray-400 mb-4">Run this command in your terminal:</p>
                <pre class="p-4 rounded-lg overflow-x-auto text-sm"><code class="text-green-400">curl -fsSL https://zapper.cloud/install.sh | sh</code></pre>
            </div>

            <!-- macOS with Homebrew -->
            <div class="bg-gray-900 rounded-lg p-6 border border-gray-800">
                <h2 class="text-2xl font-semibold mb-4 text-cyan-400">macOS (Homebrew)</h2>
                <p class="text-gray-400 mb-4">First, install Homebrew if you don't have it:</p>
                <pre class="p-4 rounded-lg overflow-x-auto text-sm mb-4"><code class="text-green-400">/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"</code></pre>
                <p class="text-gray-400 mb-4">Then install zap:</p>
                <pre class="p-4 rounded-lg overflow-x-auto text-sm"><code class="text-green-400">brew install zap</code></pre>
                <p class="text-gray-500 text-sm mt-2">(Coming soon to Homebrew)</p>
            </div>

            <!-- Manual Install -->
            <div class="bg-gray-900 rounded-lg p-6 border border-gray-800">
                <h2 class="text-2xl font-semibold mb-4 text-cyan-400">Manual Install</h2>
                <p class="text-gray-400 mb-4">Download the binary for your platform:</p>
                <div class="space-y-2">
                    <a href="https://github.com/voidash/zap/releases" class="block bg-gray-800 hover:bg-gray-700 p-3 rounded transition">
                        <span class="text-cyan-400">Linux (x86_64)</span>
                        <span class="text-gray-500 text-sm ml-2">zap-linux-x86_64</span>
                    </a>
                    <a href="https://github.com/voidash/zap/releases" class="block bg-gray-800 hover:bg-gray-700 p-3 rounded transition">
                        <span class="text-cyan-400">macOS (Apple Silicon)</span>
                        <span class="text-gray-500 text-sm ml-2">zap-darwin-arm64</span>
                    </a>
                    <a href="https://github.com/voidash/zap/releases" class="block bg-gray-800 hover:bg-gray-700 p-3 rounded transition">
                        <span class="text-cyan-400">macOS (Intel)</span>
                        <span class="text-gray-500 text-sm ml-2">zap-darwin-x86_64</span>
                    </a>
                </div>
            </div>

            <!-- Usage -->
            <div class="bg-gray-900 rounded-lg p-6 border border-gray-800">
                <h2 class="text-2xl font-semibold mb-4 text-cyan-400">Usage</h2>

                <h3 class="text-lg font-medium mb-2 text-gray-300">Send a file:</h3>
                <pre class="p-4 rounded-lg overflow-x-auto text-sm mb-4"><code class="text-green-400">zap send myfile.zip</code></pre>
                <p class="text-gray-500 text-sm mb-6">This will print a ticket to share with the receiver.</p>

                <h3 class="text-lg font-medium mb-2 text-gray-300">Receive a file:</h3>
                <pre class="p-4 rounded-lg overflow-x-auto text-sm"><code class="text-green-400">zap receive &lt;ticket&gt;</code></pre>
            </div>

            <!-- Build from Source -->
            <div class="bg-gray-900 rounded-lg p-6 border border-gray-800">
                <h2 class="text-2xl font-semibold mb-4 text-cyan-400">Build from Source</h2>
                <p class="text-gray-400 mb-4">Requires Rust 1.75+:</p>
                <pre class="p-4 rounded-lg overflow-x-auto text-sm"><code class="text-green-400">git clone https://github.com/voidash/zap.git
cd zap
cargo build --release
./target/release/zap --help</code></pre>
            </div>
        </div>

        <footer class="text-center text-gray-600 text-sm mt-12">
            <p><a href="/" class="text-cyan-400 hover:underline">Back to Web UI</a></p>
            <p class="mt-2">Powered by <a href="https://iroh.computer" class="text-cyan-400 hover:underline">iroh</a></p>
        </footer>
    </div>
</body>
</html>"##;

const INSTALL_SCRIPT: &str = r##"#!/bin/sh
set -e

# zap installer script
# Usage: curl -fsSL https://zapper.cloud/install.sh | sh

REPO="voidash/zapper.cloud"
INSTALL_DIR="/usr/local/bin"
ZAP_BIN=""

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

case "$OS" in
    linux) PLATFORM="linux-$ARCH" ;;
    darwin) PLATFORM="darwin-$ARCH" ;;
    *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

echo "Detected platform: $PLATFORM"

# Get latest release URL
LATEST_URL="https://api.github.com/repos/$REPO/releases/latest"
DOWNLOAD_URL=$(curl -fsSL "$LATEST_URL" | grep "browser_download_url.*$PLATFORM" | cut -d '"' -f 4)

if [ -z "$DOWNLOAD_URL" ]; then
    echo "Could not find release for $PLATFORM"
    echo ""
    echo "Build from source instead:"
    echo "  git clone https://github.com/$REPO.git"
    echo "  cd zap && cargo build --release"
    exit 1
fi

echo "Downloading from: $DOWNLOAD_URL"

# Download and install
TMP_FILE=$(mktemp)
curl -fsSL "$DOWNLOAD_URL" -o "$TMP_FILE"
chmod +x "$TMP_FILE"

# Try to install to /usr/local/bin, fall back to ~/.local/bin
if [ -w "$INSTALL_DIR" ]; then
    mv "$TMP_FILE" "$INSTALL_DIR/zap"
    ZAP_BIN="$INSTALL_DIR/zap"
    echo "Installed to $INSTALL_DIR/zap"
else
    mkdir -p "$HOME/.local/bin"
    mv "$TMP_FILE" "$HOME/.local/bin/zap"
    ZAP_BIN="$HOME/.local/bin/zap"
    echo "Installed to $HOME/.local/bin/zap"
    echo ""
    echo "Add to your PATH:"
    echo '  export PATH="$HOME/.local/bin:$PATH"'
fi

echo ""
echo "Run 'zap --help' to get started!"

"##;

