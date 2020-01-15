#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(unused_imports)]
//! Multiplex transport.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io;
use std::mem::drop;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock, Weak};

use async_std::task;

use futures::channel::mpsc;
use futures::channel::mpsc::{channel, Receiver, SendError, Sender};
use futures::channel::oneshot;
use futures::executor::block_on;
use futures::lock::Mutex as AsyncMutex;
use futures::prelude::*;
use futures::sink::SinkExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::task::Context;
use futures::task::Poll;
use futures::{pin_mut, select};
use pin_project::{pin_project, pinned_drop};

use rand::prelude::*;
use tokio_serde::{Deserializer, Serializer};

//use crate::Transport;

pub enum MultiplexMsg<SerializedContent> {
    /// Open connection to service on specified client port.
    Open { service: u32, client_port: u32 },
    /// Connection to service refused.
    Refused { client_port: u32 },
    /// Connection opened.
    Opened { client_port: u32, server_port: u32 },
    /// Pause sending data on specified port.
    Pause { port: u32 },
    /// Resume sending data on specified port.
    Resume { port: u32 },
    /// No more data will be sent on specifed port.
    Finish { port: u32 },
    /// Not interested on receiving any more data on specified port.
    Hangup { port: u32, graceful: bool },
    /// Data for specified port.
    Data {
        port: u32,
        content: SerializedContent,
    },
}

pub struct Multiplexer<ContentSerializer, ContentDeserializer> {
    content_serializer: ContentSerializer,
    content_deserializer: ContentDeserializer,
}

#[derive(Debug, Clone)]
pub enum MultiplexRunError<TransportSinkError, TransportStreamError> {
    SendError(Arc<TransportSinkError>),
    ReceiveError(Arc<TransportStreamError>),
    ReceiverClosed,
    ProtocolError(String),
}

/// Sets the send lock state of a channel.
struct ChannelSendLockAuthority {
    state: Arc<AsyncMutex<ChannelSendLockState>>,
}

/// Gets the send lock state of a channel.
struct ChannelSendLockRequester {
    state: Arc<AsyncMutex<ChannelSendLockState>>,
}

/// Internal state of a channel send lock.
struct ChannelSendLockState {
    send_allowed: bool,
    send_allowed_notify_tx: Option<oneshot::Sender<()>>,
}

impl ChannelSendLockAuthority {
    /// Pause sending on that channel.
    async fn pause(&mut self) {
        let mut state = self.state.lock().await;
        state.send_allowed = false;
    }

    /// Resume sending on the channel.
    async fn resume(&mut self) {
        let mut state = self.state.lock().await;
        state.send_allowed = true;

        if let Some(tx) = state.send_allowed_notify_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl ChannelSendLockRequester {
    /// Blocks until sending on the channel is allowed.
    async fn request(&self) {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let mut state = self.state.lock().await;
            if state.send_allowed {
                return;
            }

            let (tx, rx) = oneshot::channel();
            state.send_allowed_notify_tx = Some(tx);
            rx_opt = Some(rx);
        }
    }
}

/// Creates a channel send lock.
fn channel_send_lock() -> (ChannelSendLockAuthority, ChannelSendLockRequester) {
    let state = Arc::new(Mutex::new(ChannelSendLockState {
        send_allowed: true,
        send_allowed_notify_tx: None,
    }));
    let authority = ChannelSendLockAuthority {
        state: state.clone(),
    };
    let requester = ChannelSendLockRequester {
        state: state.clone(),
    };
    (authority, requester)
}

/// Channel receiver buffer item enqueuer.
struct ChannelReceiverBufferEnqueuer<SerializedContent> {
    state: Arc<AsyncMutex<ChannelReceiverBufferState<SerializedContent>>>,
    cfg: ChannelReceiverBufferCfg,
}

/// Channel receiver buffer item dequeuer.
struct ChannelReceiverBufferDequeuer<SerializedContent> {
    state: Arc<AsyncMutex<ChannelReceiverBufferState<SerializedContent>>>,
    cfg: ChannelReceiverBufferCfg,
}

/// Channel receive buffer configuration.
#[derive(Clone)]
struct ChannelReceiverBufferCfg {
    /// Buffer length that will trigger sending resume notification.
    resume_length: usize,
    /// Buffer length that will trigger sending pause notification.
    pause_length: usize,
    /// When buffer length reaches this value, enqueue function will block.
    block_length: usize,
    /// Resume notification sender.
    resume_notify_tx: Sender<u32>,
    /// Resume notification send value.
    resume_notify_port: u32,
}

/// Internal state of channel receiver buffer.
struct ChannelReceiverBufferState<SerializedContent> {
    buffer: VecDeque<SerializedContent>,
    item_enqueued: Option<oneshot::Sender<()>>,
    item_dequeued: Option<oneshot::Sender<()>>,
    finished: bool,
}

/// Creates a new channel receiver buffer.
fn channel_receiver_buffer<SerializedContent>(
    cfg: ChannelReceiverBufferCfg,
) -> (
    ChannelReceiverBufferEnqueuer<SerializedContent>,
    ChannelReceiverBufferDequeuer<SerializedContent>,
) {
    assert!(cfg.resume_length > 0);
    assert!(cfg.pause_length > cfg.resume_length);
    assert!(cfg.block_length > cfg.pause_length);

    let state = AsyncMutex::new(ChannelReceiverBufferState {
        buffer: VecDeque::new(),
        item_enqueued: None,
        item_dequeued: None,
        finished: false,
    });

    let enqueuer = ChannelReceiverBufferEnqueuer {
        state: state.clone(),
        cfg: cfg.clone(),
    };
    let dequeuer = ChannelReceiverBufferDequeuer {
        state: state.clone(),
        cfg: cfg.clone(),
    };
    (enqueuer, dequeuer)
}

impl<SerializedContent> ChannelReceiverBufferEnqueuer<SerializedContent> {
    /// Enqueues an item into the receive queue.
    /// Blocks when the block queue length has been reached.
    /// Returns true when the pause queue length has been reached from below.
    async fn enqueue(&self, item: SerializedContent) -> bool {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let state = self.state.lock().await;
            if state.buffer.len() >= self.cfg.block_length {
                let (tx, rx) = oneshot::channel();
                state.item_dequeued = Some(tx);
                rx_opt = Some(rx);
                continue;
            }

            state.buffer.push_back(item);
            if let Some(tx) = state.item_enqueued.take() {
                let _ = tx.send(());
            }

            return state.buffer.len() == self.cfg.pause_length;
        }
    }

    /// Returns true, if buffer length is at or below resume length.
    async fn resumeable(&self) -> bool {
        let state = self.state.lock().await;
        state.buffer.len() <= self.cfg.resume_length
    }
}

impl<SerializedContent> Drop for ChannelReceiverBufferEnqueuer<SerializedContent> {
    fn drop(&mut self) {
        block_on(async move {
            let state = self.state.lock().await;
            state.finished = true;
            if let Some(tx) = state.item_enqueued.take() {
                let _ = tx.send(());
            }            
        });
    }
}

impl<SerializedContent> ChannelReceiverBufferDequeuer<SerializedContent> {
    /// Dequeues an item from the receive queue.
    /// Blocks until an item becomes available.
    /// Notifies the resume notify channel when the resume queue length has been reached from above.
    async fn dequeue(&self) -> Option<SerializedContent> {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let state = self.state.lock().await;
            if state.buffer.is_empty() {
                if state.finished {
                    return None;
                } else {                
                    let (tx, rx) = oneshot::channel();
                    state.item_enqueued = Some(tx);
                    rx_opt = Some(rx);
                    continue;
                }
            }

            let item = state.buffer.pop_front().unwrap();
            if let Some(tx) = state.item_dequeued.take() {
                let _ = tx.send(());
            }

            if state.buffer.len() == self.cfg.resume_length {
                self.cfg
                    .resume_notify_tx
                    .send(self.cfg.resume_notify_port)
                    .await;
            }

            return Some(item);
        }
    }
}

#[derive(Clone)]
struct ServiceConnectData {
    service: u32,
    client_port: u32,
    server_port: u32,
}

#[derive(Debug, Clone)]
pub enum ChannelSendError<TransportSinkError, TransportStreamError, SerializationError> {
    /// Other side closed receive end of channel.
    Closed {
        graceful: bool,
    },
    /// Serialization of item to send failed.
    SerializationError(SerializationError),
    TransportSendError(Arc<TransportSinkError>),
    TransportReceiveError(Arc<TransportStreamError>),
    TransportClosed,
    MultiplexProtocolError(String),
}

impl<TransportSinkError, TransportStreamError, SerializationError>
    From<MultiplexRunError<TransportSinkError, TransportStreamError>>
    for ChannelSendError<TransportSinkError, TransportStreamError, SerializationError>
{
    fn from(err: MultiplexRunError<TransportSinkError, TransportStreamError>) -> Self {
        match err {
            MultiplexRunError::SendError(data) => Self::TransportSendError(data),
            MultiplexRunError::ReceiveError(data) => Self::TransportReceiveError(data),
            MultiplexRunError::ReceiverClosed => Self::TransportClosed,
            MultiplexRunError::ProtocolError(data) => Self::ProtocolError(data),
        }
    }
}

impl<TransportSinkError, TransportStreamError, SerializationError> From<mpsc::SendError>
    for ChannelSendError<TransportSinkError, TransportStreamError, SerializationError>
{
    fn from(err: mpsc::SendError) -> Self {
        if err.is_disconnected() {
            return Self::Closed { graceful: false };
        }
        panic!("Unknown channel close reason.");
    }
}

pub trait InnerSerializer<T, I> {
    type Error;
    fn serialize(&mut self, item: &T) -> Result<I, Self::Error>;
}

struct ChannelSenderData<
    Item,
    ItemSerializer,
    SerializedContent,
    TransportSinkError,
    TransportStreamError,
> {
    data: ServiceConnectData,
    serializer: ItemSerializer,
    tx_lock: ChannelSendLockRequester,
    our_port: u32,
    other_port: u32,
    tx: Sender<ChannelMsg<SerializedContent>>,
    global_error: Arc<RwLock<Option<MultiplexRunError<TransportSinkError, TransportStreamError>>>>,
}

#[pin_project]
pub struct ChannelSender<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError> {
    data: ServiceConnectData,
    our_port: u32,
    other_port: u32,
    #[pin]
    sink: Box<
        dyn Sink<
            Item,
            Error = ChannelSendError<TransportSinkError, TransportStreamError, SerializationError>,
        >,
    >,
    #[pin]
    drop_tx: Sender<ChannelMsg<SerializedContent>>,
}

impl<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError>
    ChannelSender<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError>
{
    fn new<ItemSerializer>(
        data: ChannelSenderData<
            Item,
            ItemSerializer,
            SerializedContent,
            TransportSinkError,
            TransportStreamError,
        >,
    ) -> ChannelSender<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError>
    where
        ItemSerializer:
            InnerSerializer<Item, SerializedContent, Error = SerializationError> + 'static,
        TransportStreamError: 'static,
        TransportSinkError: 'static,
        SerializedContent: 'static,
        Item: 'static,
    {
        let ChannelSenderData {
            data,
            serializer,
            tx_lock,
            our_port, other_port,
            tx,
            global_error,
        } = data;

        let drop_tx = tx.clone();
        let tx_adapted = tx.with(|item: Item| {
            async move {
                if let Some(err) = global_error.read().unwrap().as_ref() {
                    return Err(ChannelSendError::from(*err.clone()));
                }
                tx_lock.request().await;
                let content = serializer
                    .serialize(&item)
                    .map_err(ChannelSendError::SerializationError)?;
                let msg = ChannelMsg::SendMsg {
                    port: other_port,
                    content,
                };
                Ok::<
                    ChannelMsg<SerializedContent>,
                    ChannelSendError<TransportSinkError, TransportStreamError, SerializationError>,
                >(msg)
            }
        });

        ChannelSender {
            data: data,
            our_port,
            other_port,
            sink: Box::new(tx_adapted),
            drop_tx
        }
    }
}

impl<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError> Sink<Item>
    for ChannelSender<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError>
{
    type Error = ChannelSendError<TransportSinkError, TransportStreamError, SerializationError>;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.sink.poll_ready(cx)
    }
    fn start_send(self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        let this = self.project();
        this.sink.start_send(item)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.sink.poll_flush(cx)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.sink.poll_close(cx)
    }
}

#[pinned_drop]
impl<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError> PinnedDrop
for ChannelSender<Item, SerializedContent, TransportSinkError, TransportStreamError, SerializationError> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        block_on(async move {
            this.drop_tx.send(ChannelMsg::SenderDropped {port: *this.other_port}).await;
        });
    }
}

#[derive(Debug, Clone)]
pub enum ChannelReceiveError<TransportSinkError, TransportStreamError, DeserializationError> {
    /// Deserialization of received item failed.
    DeserializationError(DeserializationError),
    TransportSendError(Arc<TransportSinkError>),
    TransportReceiveError(Arc<TransportStreamError>),
    TransportClosed,
    MultiplexProtocolError(String),
}


struct ChannelReceiverData<Item, SerializedContent> {
    data: ServiceConnectData,
    tx: Sender<ChannelMsg<SerializedContent>>,
    rx_buffer: ChannelReceiverBufferDequeuer<SerializedContent>,
}

pub struct ChannelReceiver<Item, SerializedContent, TransportSinkError, TransportStreamError, DeserializationError> {
    stream: Box<dyn Stream<Item=Result<Item, ChannelReceiveError<TransportSinkError, TransportStreamError, DeserializationError>>>>
}

impl <Item, SerializedContent, TransportSinkError, TransportStreamError, DeserializationError> ChannelReceiver<Item, SerializedContent, TransportSinkError, TransportStreamError, DeserializationError> {
    fn new(data: ChannelReceiverData<Item, SerializedContent>) -> ChannelReceiver<Item, SerializedContent, TransportSinkError, TransportStreamError, DeserializationError> {
        // iter, then map, then take_while, then map (to remove option)

    }
}

impl<Item, TransportSinkError, TransportStreamError, DeserializationError> Stream for 
    ChannelReceiver<Item, SerializedContent>
{
    type Item = Result<Item, ChannelReceiveError<TransportSinkError, TransportStreamError, DeserializationError>>;

}

struct ServiceConnectRequest<SerializedContent> {
    receiver_buffer: Option<ChannelReceiverBufferDequeuer<SerializedContent>>,
    data: ServiceConnectData,
    response_tx: Sender<ServiceConnectResponse<SerializedContent>>,
    accepted: bool,
}

struct ServiceConnectResponse<SerializedContent> {
    data: ServiceConnectData,
    decision: ServiceConnectDecision<SerializedContent>,
}

enum ServiceConnectDecision<SerializedContent> {
    Accepted {
        enqueuer: ChannelReceiverBufferEnqueuer<SerializedContent>,
    },
    Rejected,
}

impl<SerializedContent> ServiceConnectRequest<SerializedContent> {
    pub async fn accept(self) {
        let (buf_enqueuer, buf_dequeuer) =
            channel_receiver_buffer(self.receiver_buffer_cfg.take().unwrap());
        self.response_tx
            .send(ServiceConnectResponse {
                data: self.data.clone(),
                decision: ServiceConnectDecision::Accepted {
                    enqueuer: buf_enqueuer,
                },
            })
            .await;
        self.accepted = true;
    }

    pub async fn reject(self) {
        // Will be rejected by dropping.
        drop(self);
    }
}

impl<SerializedContent> Drop for ServiceConnectRequest<SerializedContent> {
    fn drop(&mut self) {
        if !self.accepted {
            block_on(async {
                let _ = self
                    .response_tx
                    .send(ServiceConnectResponse {
                        data: self.data.clone(),
                        decision: ServiceConnectDecision::Rejected,
                    })
                    .await;
            });
        }
    }
}

#[derive(Clone, Debug)]
pub struct Cfg {
    pub channel_rx_queue_length: usize,
}

/// Allocates numbers randomly and uniquely.
struct NumberAllocator {
    used: HashSet<u32>,
    rng: ThreadRng,
}

impl NumberAllocator {
    /// Creates a new number allocator.
    fn new() -> NumberAllocator {
        NumberAllocator {
            used: HashSet::new(),
            rng: thread_rng(),
        }
    }

    /// Allocates a random, unique number.
    /// Panics when more than 2,147,483,647 numbers are allocated.
    fn allocate(&mut self) -> u32 {
        if self.used.len() >= std::u32::MAX / 2 {
            panic!("NumberAllocator is out of numbers to allocate.");
        }
        loop {
            let cand = self.rng.gen();
            if !self.used.contains(&cand) {
                self.used.insert(cand);
                return cand;
            }
        }
    }

    /// Releases a previously allocated number.
    /// Panics when the number is currently not allocated.
    fn release(&mut self, number: u32) {
        if !self.used.remove(&number) {
            panic!("NumberAllocator cannot release a number that is currently not allocated.");
        }
    }
}

enum PortState<SerializedContent> {
    ClientConnecting {
        recv_buffer: ChannelReceiverBufferEnqueuer<SerializedContent>,
        response_tx: oneshot::Sender<ServiceConnectData>,
        service: u32,
    },
    Connected {
        send_lock: ChannelSendLockAuthority,
        recv_buffer: ChannelReceiverBufferEnqueuer<SerializedContent>,
    },
}

enum ChannelMsg<SerializedContent> {
    /// Send message with content.
    SendMsg {
        port: u32,
        content: SerializedContent,
    },
    /// Sender has been dropped.
    SenderDropped { port: u32 },
    /// Receiver has been dropped.
    ReceiverDropped { port: u32 },
}

enum LoopEvent<SerializedContent, TransportStreamError> {
    /// Send message over transport.
    SendMsg(MultiplexMsg<SerializedContent>),
    /// Received message from transport.
    ReceiveMsg(Result<MultiplexMsg<SerializedContent>, TransportStreamError>),
    /// Connect to service with specified id on other side.
    ServiceConnectRequest {
        service: u32,
        recv_buffer: ChannelReceiverBufferEnqueuer<SerializedContent>,
        response_tx: oneshot::Sender<ServiceConnectData>,
    },
    /// Answer to connect request from other side.
    ServiceConnectResponse(ServiceConnectResponse<SerializedContent>),
    /// A channel receive buffer has reached the resume length from above.
    BufferReachedResumeLength(u32),
}

async fn run<
    SerializedContent,
    TransportSink,
    TransportStream,
    TransportSinkError,
    TransportStreamError,
>(
    cfg: Cfg,
    transport_tx: TransportSink,
    transport_rx: TransportStream,
    req_tx: Sender<LoopEvent<SerializedContent, TransportStreamError>>,
    req_rx: Receiver<LoopEvent<SerializedContent, TransportStreamError>>,
    accept_tx: Arc<Mutex<HashMap<u32, Sender<ServiceConnectRequest<SerializedContent>>>>>,
) -> Result<(), MultiplexRunError<TransportSinkError, TransportStreamError>>
where
    TransportSink: Sink<MultiplexMsg<SerializedContent>, Error = TransportSinkError>,
    TransportStream: Stream<Item = Result<MultiplexMsg<SerializedContent>, TransportStreamError>>,
{
    let mut port_pool = NumberAllocator::new();

    let (service_connect_response_tx, service_connect_response_rx) = channel(1);
    let (port_receive_resume_tx, port_receive_resume_rx) = channel(1);

    assert!(
        cfg.channel_rx_queue_length >= 2,
        "Channel receive queue length must be at least 2"
    );
    let channel_receiver_buffer_cfg = ChannelReceiverBufferCfg {
        resume_length: cfg.channel_rx_queue_length / 2,
        pause_length: cfg.channel_rx_queue_length,
        block_length: cfg.channel_rx_queue_length * 2,
        resume_notify_tx: port_receive_resume_tx,
        resume_notify_port: 0,
    };

    pin_mut!(transport_tx);

    // Merge received transport messages into event stream.
    let service_connect_response_rx =
        service_connect_response_rx.map(LoopEvent::ServiceConnectResponse);
    let port_receive_resume_rx = port_receive_resume_rx.map(LoopEvent::BufferReachedResumeLength);
    let transport_rx = transport_rx.map(LoopEvent::ReceiveMsg);
    let event_rx = stream::select(
        req_rx,
        stream::select(
            transport_rx,
            stream::select(port_receive_resume_rx, service_connect_response_rx),
        ),
    );
    pin_mut!(event_rx);

    let mut client_ports = HashMap::new();
    let mut server_ports = HashMap::new();
    //let mut ports =

    let transport_send = |msg| {
        async move {
            transport_tx
                .send(msg)
                .await
                .map_err(MultiplexRunError::SendError)
        }
    };

    loop {
        match event_rx.next().await {
            // Process send queue.
            Some(LoopEvent::SendMsg(msg)) => {
                transport_send(msg).await?;
            }

            // Process received message.
            Some(LoopEvent::ReceiveMsg(Ok(msg))) => {
                match msg {
                    // Process service open request message.
                    MultiplexMsg::Open {
                        service,
                        client_port,
                    } => {
                        // Allocate port and build service connect request.
                        let server_port = port_pool.allocate();
                        let scr = ServiceConnectRequest {
                            receiver_buffer_cfg: Some(ChannelReceiverBufferCfg {
                                resume_notify_port: server_port,
                                ..channel_receiver_buffer_cfg.clone()
                            }),
                            data: ServiceConnectData {
                                service,
                                client_port,
                                server_port,
                            },
                            response_tx: service_connect_response_tx.clone(),
                            accepted: false,
                        };

                        // Submit service connect request to handler.
                        // If sending fails, the drop function will submit a reject response.
                        let mut accept_tx = accept_tx.lock().unwrap();
                        match accept_tx.get(&service) {
                            Some(service_accept_tx) => {
                                if let Err(_) = service_accept_tx.send(scr).await {
                                    accept_tx.remove(&service);
                                }
                            }
                        }
                    }

                    // Process service opened reply message.
                    MultiplexMsg::Opened {
                        client_port,
                        server_port,
                    } => {
                        match client_ports.remove(&client_port).ok_or(
                            MultiplexRunError::ProtocolError(
                                "Service opened response for no port".to_owned(),
                            ),
                        )? {
                            ClientPortState::Connecting {
                                recv_buffer,
                                response_tx,
                                service,
                            } => {
                                let scd = ServiceConnectData {
                                    service,
                                    client_port,
                                    server_port,
                                };
                                response_tx.send(scd);
                                client_ports
                                    .insert(client_port, ClientPortState::Connected(recv_buffer));
                            }
                            ClientPortState::Connected(_) => {
                                return Err(MultiplexRunError::ProtocolError(
                                    "Service opened response for connected port".to_owned(),
                                ));
                            }
                        }
                    }

                    // Process service opening refused reply message.
                    MultiplexMsg::Refused { client_port } => {
                        match client_ports.remove(&client_port).ok_or(
                            MultiplexRunError::ProtocolError(
                                "Service refused response for no port".to_owned(),
                            ),
                        )? {
                            ClientPortState::Connecting { .. } => {
                                port_pool.release(client_port);
                            }
                            ClientPortState::Connected(_) => {
                                return Err(MultiplexRunError::ProtocolError(
                                    "Service refused response for connected port".to_owned(),
                                ));
                            }
                        }
                    }

                    // Process pause sending data message.
                    MultiplexMsg::Pause { port: u32 } => {}
                }
            }

            // Process receive error.
            Some(LoopEvent::ReceiveMsg(Err(rx_err))) => {
                return Err(MultiplexRunError::ReceiveError(rx_err));
            }

            // Process service connect request.
            Some(LoopEvent::ServiceConnectRequest {
                service,
                recv_buffer,
                response_tx,
            }) => {
                let client_port = port_pool.allocate();
                client_ports.insert(
                    client_port,
                    ClientPortState::Connecting {
                        recv_buffer,
                        response_tx,
                        service,
                    },
                );
                transport_send(MultiplexMsg::Open {
                    service,
                    client_port,
                })
                .await?;
            }

            // Process service connect response.
            Some(LoopEvent::ServiceConnectResponse(ServiceConnectResponse { data, decision })) => {
                match decision {
                    ServiceConnectDecision::Accepted { enqueuer } => {
                        server_ports.insert(data.server_port, enqueuer);
                        transport_send(MultiplexMsg::Opened {
                            client_port: data.client_port,
                            server_port: data.server_port,
                        })
                        .await?;
                    }
                    ServiceConnectDecision::Rejected => {
                        transport_send(MultiplexMsg::Refused {
                            client_port: data.client_port,
                        })
                        .await?;
                        port_pool.release(data.server_port);
                    }
                }
            }

            None => return Ok(()),
        }
    }
}

// Each channel will do the serialization, but for ease of use the multiplexor will have the
// serializer.

/// Sends data into a multiplex channel.
pub struct MultiplexSender<SinkItem> {}

impl<SinkItem> Sink<SinkItem> for MultiplexSender<SinkItem> {}
