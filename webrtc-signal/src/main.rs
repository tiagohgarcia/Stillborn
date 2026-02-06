use axum::{ // axum is a webframework
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    routing::get,
    Router, // define HTTP routes
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

type Tx = mpsc::UnboundedSender<Message>;
type Rooms = Arc<Mutex<HashMap<String, Vec<Tx>>>>;

#[derive(Clone)]
struct AppState {
    rooms: Rooms,
}

#[derive(Debug, Serialize, Deserialize)]
struct SignalEnvelope {
    // "offer" | "answer" | "candidate"
    #[serde(rename = "type")]
    msg_type: String,
    payload: serde_json::Value,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    /*
        create an empty room map
        wrap in Mutex, wrap in Arc
        store in 'state'
    */
    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
    };

    /*
        create a Router
        define a route /ws/:room
        attatch the shared 'state'
    */
    let app = Router::new()
        .route("/ws/:room", get(ws_handler))
        .with_state(state);

    // .parse() turns the string into a SocketAddr
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    info!("Signaling server on ws://{}/ws/<room>", addr);

    // bind + serve
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(room): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, room, socket))
}

async fn handle_socket(state: AppState, room: String, socket: WebSocket) {
    info!("client joined room={}", room);

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<Message>();

    // Register client sender into room
    {
        let mut rooms = state.rooms.lock().await;
        rooms.entry(room.clone()).or_default().push(client_tx);
    }

    // Task: forward messages from server channel -> websocket
    let mut ws_tx_task = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Receive from websocket and broadcast to other clients in the room
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(txt) => {
                // Validate it’s JSON shaped like our envelope (not required but helps debugging)
                if serde_json::from_str::<SignalEnvelope>(&txt).is_err() {
                    warn!("invalid json message: {}", txt);
                    continue;
                }

                let mut rooms = state.rooms.lock().await;
                if let Some(clients) = rooms.get_mut(&room) {
                    // broadcast to everyone (including sender) is OK, but we’ll avoid echo by sending
                    // to all and letting the browser ignore self if needed.
                    for tx in clients.iter() {
                        let _ = tx.send(Message::Text(txt.clone()));
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    ws_tx_task.abort();
    info!("client left room={}", room);

    // Cleanup: remove dead senders (best-effort)
    let mut rooms = state.rooms.lock().await;
    if let Some(clients) = rooms.get_mut(&room) {
        clients.retain(|tx| !tx.is_closed());
        if clients.is_empty() {
            rooms.remove(&room);
        }
    }
}