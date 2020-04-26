use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_tungstenite::tokio::connect_async;
use async_tungstenite::tungstenite::Message;
use futures::channel::{mpsc, oneshot};
use futures::future;
use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use tokio::task;

use crate::errors::Result;
use crate::transports::{BatchTransport, PubsubTransport, Transport};
use crate::types::{
    Call, MethodCall, Notification, Params, Request, RequestId, Response, SubscriptionId, Value,
    Version,
};

type Pending = oneshot::Sender<Result<Response>>;
type Pendings = Arc<Mutex<BTreeMap<RequestId, Pending>>>;
type Subscription = mpsc::UnboundedSender<Value>;
type Subscriptions = Arc<Mutex<BTreeMap<SubscriptionId, Subscription>>>;

type WebSocketSender = mpsc::UnboundedSender<Message>;
type WebSocketReceiver = mpsc::UnboundedReceiver<Message>;

pub struct WebSocketTransport {
    id: Arc<AtomicUsize>,
    _url: String,
    pendings: Pendings,
    subscriptions: Subscriptions,
    sender: WebSocketSender,
    _handle: task::JoinHandle<()>,
}

impl WebSocketTransport {
    pub fn new<U: Into<String>>(url: U) -> Self {
        let url = url.into();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let subscriptions = Arc::new(Mutex::new(BTreeMap::new()));
        let (writer_tx, writer_rx) = mpsc::unbounded();

        let handle = task::spawn(ws_task(
            url.clone(),
            pending.clone(),
            subscriptions.clone(),
            writer_tx.clone(),
            writer_rx,
        ));

        Self {
            id: Arc::new(AtomicUsize::new(1)),
            _url: url,
            pendings: pending,
            subscriptions,
            sender: writer_tx,
            _handle: handle,
        }
    }

    async fn send_request(&self, id: RequestId, request: &Request) -> Result<Response> {
        let request = serde_json::to_string(request)?;
        debug!("Calling: {}", request);

        let (tx, rx) = oneshot::channel();
        self.pendings.lock().insert(id, tx);
        self.sender
            .unbounded_send(Message::Text(request))
            .expect("");

        rx.await.unwrap()
    }
}

async fn ws_task(
    url: String,
    pendings: Pendings,
    sub: Subscriptions,
    tx: WebSocketSender,
    rx: WebSocketReceiver,
) {
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    info!("{}: handshake has been successfully completed", url);
    let (sink, stream) = ws_stream.split();

    // receive request from WebSocketSender,
    // and forward the request to sink that will send message to websocket stream.
    let write_to_ws = rx.map(Ok).forward(sink);
    // read websocket message from websocket stream, and handle the incoming message.
    let read_from_ws = stream.for_each(|msg| async {
        match msg {
            Ok(msg) => handle_incoming_msg(msg, pendings.clone(), sub.clone(), tx.clone()),
            Err(err) => error!("WebSocket stream read error: {}", err),
        }
    });

    futures::pin_mut!(write_to_ws, read_from_ws);
    future::select(write_to_ws, read_from_ws).await;
}

fn handle_incoming_msg(
    msg: Message,
    pendings: Pendings,
    subscriptions: Subscriptions,
    tx: WebSocketSender,
) {
    match msg {
        Message::Text(msg) => {
            handle_subscription(subscriptions, &msg);
            handle_pending_response(pendings, &msg);
        }
        Message::Binary(msg) => warn!("Receive `Binary` Message: {:?}", msg),
        Message::Close(msg) => {
            warn!("Receive `Close` Message: {:?}", msg);
            tx.unbounded_send(Message::Close(msg)).expect("")
        }
        Message::Ping(msg) => {
            warn!("Receive `Ping` Message: {:?}", msg);
            tx.unbounded_send(Message::Pong(msg)).expect("")
        }
        Message::Pong(msg) => warn!("Receive `Pong` Message: {:?}", msg),
    }
}

fn handle_subscription(subscriptions: Subscriptions, msg: &str) {
    if let Ok(notification) = serde_json::from_str::<Notification>(msg) {
        if let Params::Array(params) = notification.params {
            let id = params.get(0);
            let result = params.get(1);
            if let (Some(Value::Number(id)), Some(result)) = (id, result) {
                let id = id.as_u64().unwrap() as usize;
                if let Some(stream) = subscriptions.lock().get(&id) {
                    stream.unbounded_send(result.clone()).expect("");
                } else {
                    warn!("Got notification for unknown subscription (id: {})", id);
                }
            } else {
                error!("Got unsupported notification (id: {:?})", id);
            }
        } else {
            error!(
                "The Notification Params is not JSON array type: {}",
                serde_json::to_string(&notification.params)
                    .expect("Serialize `Params` never fails")
            );
        }
    }
}

fn handle_pending_response(pendings: Pendings, msg: &str) {
    let response = serde_json::from_str::<Response>(msg).map_err(Into::into);
    let id = match &response {
        Ok(Response::Single(output)) => output.id(),
        Ok(Response::Batch(outputs)) => outputs.get(0).map_or(0, |output| output.id()),
        Err(_) => 0,
    };
    if let Some(request) = pendings.lock().remove(&id) {
        if let Err(err) = request.send(response) {
            error!("Sending a response to deallocated channel: {:?}", err);
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Transport for WebSocketTransport {
    fn prepare<M: Into<String>>(&self, method: M, params: Params) -> (RequestId, Call) {
        let id = self.id.fetch_add(1, Ordering::AcqRel);
        let call = Call::MethodCall(MethodCall {
            jsonrpc: Some(Version::V2),
            id,
            method: method.into(),
            params,
        });
        (id, call)
    }

    async fn execute(&self, id: RequestId, request: &Request) -> Result<Response> {
        self.send_request(id, request).await
    }
}

#[async_trait::async_trait(?Send)]
impl BatchTransport for WebSocketTransport {}

#[async_trait::async_trait(?Send)]
impl PubsubTransport for WebSocketTransport {
    type NotificationStream = BoxStream<'static, Value>;

    async fn subscribe(&self, id: SubscriptionId) -> Self::NotificationStream {
        let (tx, rx) = mpsc::unbounded();
        if self.subscriptions.lock().insert(id, tx).is_some() {
            warn!("Replacing already-registered subscription with id {:?}", id);
        }
        Box::pin(rx)
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        self.subscriptions.lock().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_request() {
        env_logger::init();
        let ws = WebSocketTransport::new("ws://127.0.0.1:1234/rpc/v0");
        let version: Value = ws
            .send("Filecoin.Version", Params::Array(vec![]))
            .await
            .unwrap();
        println!("Version: {:?}", version);
    }

    #[tokio::test]
    async fn test_subscription() {
        env_logger::init();
        let ws = WebSocketTransport::new("ws://127.0.0.1:1234/rpc/v0");
        let id: usize = ws
            .send("Filecoin.SyncIncomingBlocks", Params::Array(vec![]))
            .await
            .unwrap();
        println!("Subscription Id: {}", id);

        let mut stream = ws.subscribe(id).await;
        while let Some(value) = stream.next().await {
            println!("Block: {:?}", value);
        }
    }
}
