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

pub enum MultiplexMsg<Content> {
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
    /// No more data will be sent to specifed remote port.
    Finish { port: u32 },
    /// Acknowledgement that Finish has been received for the specified remote port.
    FinishAck { port: u32 },
    /// Not interested on receiving any more data from specified remote port.
    Hangup { port: u32, graceful: bool },
    /// Data for specified port.
    Data {
        port: u32,
        content: Content,
    },
}

pub struct Multiplexer<ContentSerializer, ContentDeserializer> {
    content_serializer: ContentSerializer,
    content_deserializer: ContentDeserializer,
}

#[derive(Debug, Clone)]
pub enum MultiplexRunError<TransportSinkError, TransportStreamError> {
    SendError(TransportSinkError),
    ReceiveError(TransportStreamError),
    ReceiverClosed,
    ProtocolError(String),
}

#[derive(Debug, Clone)]
pub enum ChannelSendError {
    /// Other side closed receiving end of channel.
    Closed { 
        /// True if other side still processes items send up until now.
        gracefully: bool,
    },
    /// The multiplexer encountered an error or was terminated.
    MultiplexerError
}

impl From<mpsc::SendError> for ChannelSendError
{
    fn from(err: mpsc::SendError) -> Self {
        if err.is_disconnected() {
            return Self::MultiplexerError;
        }
        panic!("Error sending data to multiplexer for unknown reasons.");
    }
}


#[derive(Debug, Clone)]
pub enum ChannelReceiveError {
    /// The multiplexer encountered an error or was terminated.
    MultiplexerError
}



/// Allocates numbers randomly and uniquely.
struct NumberAllocator {
    used: HashSet<u32>,
    rng: ThreadRng,
}

/// Number allocated exhausted.
#[derive(Debug, Clone)]
struct NumberAllocatorExhaustedError;

impl NumberAllocator {
    /// Creates a new number allocator.
    fn new() -> NumberAllocator {
        NumberAllocator {
            used: HashSet::new(),
            rng: thread_rng(),
        }
    }

    /// Allocates a random, unique number.
    fn allocate(&mut self) -> Result<u32, NumberAllocatorExhaustedError> {
        if self.used.len() >= std::u32::MAX / 2 {
            return Err(NumberAllocatorExhaustedError);
        }
        loop {
            let cand = self.rng.gen();
            if !self.used.contains(&cand) {
                self.used.insert(cand);
                return Ok(cand);
            }
        }
    }

    /// Releases a previously allocated number.
    /// Panics when the number is currently not allocated.
    fn release(&mut self, number: u32) {
        if !self.used.remove(&number) {
            panic!("NumberAllocator cannot release number {} that is currently not allocated.", number);
        }
    }
}

/// Sets the send lock state of a channel.
struct ChannelSendLockAuthority {
    state: Arc<AsyncMutex<ChannelSendLockState>>,
}

/// Gets the send lock state of a channel.
struct ChannelSendLockRequester {
    state: Arc<AsyncMutex<ChannelSendLockState>>,
}

enum ChannelSendLockCloseReason {
    Closed {gracefully: bool},
    Dropped
}

/// Internal state of a channel send lock.
struct ChannelSendLockState {
    send_allowed: bool,
    close_reason: Option<ChannelSendLockCloseReason>,
    notify_tx: Option<oneshot::Sender<()>>,
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

        if let Some(tx) = state.notify_tx.take() {
            let _ = tx.send(());
        }
    }

    /// Closes the channel.
    async fn close(mut self, gracefully: bool) {
        let mut state = self.state.lock().await;
        state.send_allowed = false;
        state.close_reason = Some(ChannelSendLockCloseReason::Closed {gracefully});

        if let Some(tx) = state.notify_tx.take() {
            let _ = tx.send(());
        }        
    }
}

impl Drop for ChannelSendLockAuthority {
    fn drop(&mut self) {
        block_on(async {
            let mut state = self.state.lock().await;
            if state.close_reason.is_none() {
                state.send_allowed = false;            
                state.close_reason = Some(ChannelSendLockCloseReason::Dropped);

                if let Some(tx) = state.notify_tx.take() {
                    let _ = tx.send(());
                }                    
            }
        });
    }
}

impl ChannelSendLockRequester {
    /// Blocks until sending on the channel is allowed.
    async fn request(&self) -> Result<(), ChannelSendError> {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let mut state = self.state.lock().await;
            if state.send_allowed {
                return Ok(());
            } 
            match &state.close_reason {
                Some (ChannelSendLockCloseReason::Closed {gracefully}) => return Err(ChannelSendError::Closed {gracefully: gracefully.clone()}),
                Some (ChannelSendLockCloseReason::Dropped) => return Err(ChannelSendError::MultiplexerError),
                None => ()                
            }

            let (tx, rx) = oneshot::channel();
            state.notify_tx = Some(tx);
            rx_opt = Some(rx);
        }
    }
}

/// Creates a channel send lock.
fn channel_send_lock() -> (ChannelSendLockAuthority, ChannelSendLockRequester) {
    let state = Arc::new(Mutex::new(ChannelSendLockState {
        send_allowed: true,
        close_reason: None,
        notify_tx: None,
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
struct ChannelReceiverBufferEnqueuer<Content> {
    state: Arc<AsyncMutex<ChannelReceiverBufferState<Content>>>,
    cfg: ChannelReceiverBufferCfg<Content>,
}

/// Channel receiver buffer item dequeuer.
struct ChannelReceiverBufferDequeuer<Content> {
    state: Arc<AsyncMutex<ChannelReceiverBufferState<Content>>>,
    cfg: ChannelReceiverBufferCfg<Content>,
}

/// Channel receive buffer configuration.
#[derive(Clone)]
struct ChannelReceiverBufferCfg<Content> {
    /// Buffer length that will trigger sending resume notification.
    resume_length: usize,
    /// Buffer length that will trigger sending pause notification.
    pause_length: usize,
    /// When buffer length reaches this value, enqueue function will block.
    block_length: usize,
    /// Resume notification sender.
    resume_notify_tx: mpsc::Sender<ChannelMsg<Content>>,
    /// Local port for resume notification.
    local_port: u32,
}

enum ChannelReceiverBufferCloseReason {
    Closed,
    Dropped
}

/// Internal state of channel receiver buffer.
struct ChannelReceiverBufferState<Content> {
    buffer: VecDeque<Content>,
    close_reason: Option<ChannelReceiverBufferCloseReason>,
    item_enqueued: Option<oneshot::Sender<()>>,
    item_dequeued: Option<oneshot::Sender<()>>,
}

impl<Content> ChannelReceiverBufferCfg<Content> {
    /// Creates a new channel receiver buffer and returns the associated
    /// enqueuer and dequeuer.
    fn instantiate(self) -> (
        ChannelReceiverBufferEnqueuer<Content>,
        ChannelReceiverBufferDequeuer<Content>,
    ) {
        assert!(self.resume_length > 0);
        assert!(self.pause_length > self.resume_length);
        assert!(self.block_length > self.pause_length);
    
        let state = AsyncMutex::new(ChannelReceiverBufferState {
            buffer: VecDeque::new(),
            close_reason: None,
            item_enqueued: None,
            item_dequeued: None,
        });
    
        let enqueuer = ChannelReceiverBufferEnqueuer {
            state: state.clone(),
            cfg: self.clone(),
        };
        let dequeuer = ChannelReceiverBufferDequeuer {
            state: state.clone(),
            cfg: self,
        };
        (enqueuer, dequeuer)
    }
}

impl<Content> ChannelReceiverBufferEnqueuer<Content> {
    /// Enqueues an item into the receive queue.
    /// Blocks when the block queue length has been reached.
    /// Returns true when the pause queue length has been reached from below.
    async fn enqueue(&self, item: Content) -> bool {
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

    /// Indicates that the receive stream is finished.
    async fn close(mut self) {
        let state = self.state.lock().await;
        state.close_reason = Some(ChannelReceiverBufferCloseReason::Closed);
        if let Some(tx) = state.item_enqueued.take() {
            let _ = tx.send(());
        }            
    }
}

impl<Content> Drop for ChannelReceiverBufferEnqueuer<Content> {
    fn drop(&mut self) {
        block_on(async move {
            let state = self.state.lock().await;
            if state.close_reason.is_none() {
                state.close_reason = Some(ChannelReceiverBufferCloseReason::Dropped);
                if let Some(tx) = state.item_enqueued.take() {
                    let _ = tx.send(());
                }            
            }
        });
    }
}

impl<Content> ChannelReceiverBufferDequeuer<Content> {
    /// Dequeues an item from the receive queue.
    /// Blocks until an item becomes available.
    /// Notifies the resume notify channel when the resume queue length has been reached from above.
    async fn dequeue(&self) -> Option<Result<Content, ChannelReceiveError>> {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let state = self.state.lock().await;
            if state.buffer.is_empty() {
                match &state.close_reason {
                    Some (ChannelReceiverBufferCloseReason::Closed) => return None,
                    Some (ChannelReceiverBufferCloseReason::Dropped) => return Some(Err(ChannelReceiveError::MultiplexerError)),
                    None => {                
                        let (tx, rx) = oneshot::channel();
                        state.item_enqueued = Some(tx);
                        rx_opt = Some(rx);
                        continue;
                    }
                }
            }

            let item = state.buffer.pop_front().unwrap();
            if let Some(tx) = state.item_dequeued.take() {
                let _ = tx.send(());
            }

            if state.buffer.len() == self.cfg.resume_length {
                self.cfg
                    .resume_notify_tx
                    .send(ChannelMsg::ReceiveBufferReachedResumeLength {local_port: self.cfg.local_port})
                    .await;
            }

            return Some(Ok(item));
        }
    }
}

struct ChannelData<Content> {
    local_port: u32,
    remote_port: u32,
    tx: mpsc::Sender<ChannelMsg<Content>>,
    tx_lock: ChannelSendLockRequester,
    rx_buffer: ChannelReceiverBufferDequeuer<Content>,
}

impl<Content> ChannelData<Content> {
    fn instantiate(self) -> (ChannelSender<Content>, ChannelReceiver<Content>) {
        let ChannelData {
            local_port, remote_port, tx, tx_lock, rx_buffer
        } = self;
        
        let sender = ChannelSender::new(
            local_port,
            remote_port,
            tx.clone(),
            tx_lock
        );
        let receiver = ChannelReceiver::new(
            local_port,
            remote_port,
            tx,
            rx_buffer
        );
        (sender, receiver)
    }
}

#[pin_project]
pub struct ChannelSender<Content> {
    local_port: u32,
    remote_port: u32,
    #[pin]
    sink: Box<
        dyn Sink<
            Content,
            Error = ChannelSendError,
        >,
    >,
    #[pin]
    drop_tx: mpsc::Sender<ChannelMsg<Content>>,
}

impl<Content>
    ChannelSender<Content>
{
    fn new(
        local_port: u32,
        remote_port: u32,
        tx: mpsc::Sender<ChannelMsg<Content>>,
        tx_lock: ChannelSendLockRequester,
    ) -> ChannelSender<Content>
    where
        Content: 'static,
    {
        let drop_tx = tx.clone();
        let adapted_tx = tx.with(|item: Content| {
            async move {
                tx_lock.request().await?;
                let msg = ChannelMsg::SendMsg {
                    remote_port,
                    content: item,
                };
                Ok::<
                    ChannelMsg<Content>,
                    ChannelSendError,
                >(msg)
            }
        });

        ChannelSender {
            local_port,
            remote_port,
            sink: Box::new(adapted_tx),
            drop_tx
        }
    }
}

impl<Item> Sink<Item> for ChannelSender<Item>
{
    type Error = ChannelSendError;
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
impl<Item> PinnedDrop
for ChannelSender<Item> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        block_on(async move {
            this.drop_tx.send(ChannelMsg::SenderDropped {local_port: *this.local_port}).await;
        });
    }
}

#[pin_project]
pub struct ChannelReceiver<Content> {
    local_port: u32,
    remote_port: u32,
    closed: bool,
    #[pin]
    stream: Box<dyn Stream<Item=Result<Content, ChannelReceiveError>>>,
    #[pin]
    drop_tx: mpsc::Sender<ChannelMsg<Content>>,
}

impl<Content> ChannelReceiver<Content> {
    fn new(
        local_port: u32,
        remote_port: u32,        
        tx: mpsc::Sender<ChannelMsg<Content>>,
        rx_buffer: ChannelReceiverBufferDequeuer<Content>,
    ) -> ChannelReceiver<Content> 
    where Content: 'static {
        let stream =
            stream::repeat(()).then(|_| async move {
                rx_buffer.dequeue().await
            }).take_while(|opt_item| future::ready(opt_item.is_some()))
            .map(|opt_item| opt_item.unwrap());

        ChannelReceiver {
            local_port,
            remote_port,
            closed: false,
            drop_tx: tx,
            stream: Box::new(stream),
        }       
    }

    /// Prevents the remote endpoint from sending new messages into this channel while
    /// allowing in-flight messages to be received.
    pub fn close(&mut self) {
        if !self.closed {
            block_on(async {
                self.drop_tx.send(ChannelMsg::ReceiverClosed {local_port: self.local_port, gracefully: true}).await;
            });        
            self.closed = true;
        }
    }
}

impl<Item> Stream for ChannelReceiver<Item>
{
    type Item = Result<Item, ChannelReceiveError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.stream.poll_next(cx)        
    }
}

#[pinned_drop]
impl<Item> PinnedDrop for ChannelReceiver<Item> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if !*this.closed {
            block_on(async {
                this.drop_tx.send(ChannelMsg::ReceiverClosed {local_port: *this.local_port, gracefully: false}).await;
            });
        }
    }
}

/// A service request by the remote endpoint.
struct RemoteConnectToServiceRequest<Content> {
    channel_data: Option<ChannelData<Content>>,
    response_tx: mpsc::Sender<ChannelMsg<Content>>,
}

impl<Content> RemoteConnectToServiceRequest<Content> {
    fn new(channel_data: ChannelData<Content>, response_tx: mpsc::Sender<ChannelMsg<Content>>)
        -> RemoteConnectToServiceRequest<Content> 
    {
        RemoteConnectToServiceRequest {
            channel_data: Some(channel_data),
            response_tx
        }
    }

    /// Accepts the service request and returns a pair of channel sender and receiver.
    pub fn accept(self) -> (ChannelSender<Content>, ChannelReceiver<Content>) {
        let channel_data = self.channel_data.take().unwrap();
        block_on(async {
            let _ = self.response_tx.send(ChannelMsg::Accepted {local_port: channel_data.local_port}).await;
        });
        channel_data.instantiate()
    }

    /// Rejects the service request, optionally providing the specified reason to the remote endpoint.
    pub fn reject(self, reason: Option<Content>) {
        let channel_data = self.channel_data.take().unwrap();
        block_on(async {
            let _ = self.response_tx.send(ChannelMsg::Rejected {local_port: channel_data.local_port, reason}).await;
        });
    }
}

impl<Content> Drop for RemoteConnectToServiceRequest<Content> {
    fn drop(&mut self) {
        if let Some(channel_data) = self.channel_data.take() {
            block_on(async {
                let _ = self.response_tx.send(ChannelMsg::Rejected {local_port: channel_data.local_port, reason: None}).await;
            });    
        }
    }
}

#[derive(Clone, Debug)]
pub struct Cfg {
    pub channel_rx_queue_length: usize,
}


struct PortData<Content> {
    local_port: u32,
    tx_lock: ChannelSendLockAuthority,
    rx_buffer: Option<ChannelReceiverBufferEnqueuer<Content>>,
    tx_finished: bool,
    tx_finished_ack: bool,
    rx_finished: bool,
    state: PortState
}

enum PortState {
    LocalRequestingRemoteService,
    RemoteRequestingLocalService {remote_port: u32},
    Connected {remote_port: u32},
}

impl<Content> PortData<Content> {
    fn remote_port(&self) -> u32 {
        match &self.state {
            PortState::Connected {remote_port} => remote_port,
            _ => panic!("Port unexpectedly not in connected state.")
        }
    }
}


enum ChannelMsg<Content> {
    /// Send message with content.
    SendMsg {
        remote_port: u32,
        content: Content,
    },
    /// Sender has been dropped.
    SenderDropped { local_port: u32 },
    /// Receiver has been dropped.
    ReceiverClosed { local_port: u32, gracefully: bool },
    /// Connection has been accepted.
    Accepted {local_port: u32},
    /// Connection has been rejected.
    Rejected {local_port: u32, reason: Option<Content>},
    /// The channel receive buffer has reached the resume length from above.
    ReceiveBufferReachedResumeLength {local_port: u32},
}

struct ConnectToRemoteServiceRequest<Content> {
    service: Content,
    response_tx: oneshot::Sender<ConnectToRemoteServiceResponse<Content>>,
}

enum ConnectToRemoteServiceResponse<Content> {
    Accepted {
        sender: ChannelSender<Content>,
        receiver: ChannelReceiver<Content>,
    },
    Rejected {
        reason: Option<Content>
    }
}

enum LoopEvent<Content, TransportStreamError> {
    /// A message from a local channel.
    ChannelMsg (ChannelMsg<Content>),
    /// Received message from transport.
    ReceiveMsg(Result<MultiplexMsg<Content>, TransportStreamError>),
    /// Connect to service with specified id on the remote side.
    ConnectToRemoteServiceRequest (ConnectToRemoteServiceRequest<Content>),
}

async fn run<
    Content,
    TransportSink,
    TransportStream,
    TransportSinkError,
    TransportStreamError,
>(
    cfg: Cfg,
    transport_tx: TransportSink,
    transport_rx: TransportStream,
    connect_rx: mpsc::Receiver<ConnectToRemoteServiceRequest<Content>>,
    serve_tx: mpsc::Sender<(Content, RemoteConnectToServiceRequest<Content>)>,
) -> Result<(), MultiplexRunError<TransportSinkError, TransportStreamError>>
where
    TransportSink: Sink<MultiplexMsg<Content>, Error = TransportSinkError>,
    TransportStream: Stream<Item = Result<MultiplexMsg<Content>, TransportStreamError>>,
{
    let mut port_pool = NumberAllocator::new();

    let (channel_tx, channel_rx) = mpsc::channel(1);

    assert!(
        cfg.channel_rx_queue_length >= 2,
        "Channel receive queue length must be at least 2"
    );
    let channel_receiver_buffer_cfg = ChannelReceiverBufferCfg {
        resume_length: cfg.channel_rx_queue_length / 2,
        pause_length: cfg.channel_rx_queue_length,
        block_length: cfg.channel_rx_queue_length * 2,
        resume_notify_tx: channel_tx.clone(),
        local_port: 0,
    };

    pin_mut!(transport_tx);

    // Create event stream.
    let transport_rx = transport_rx.map(LoopEvent::ReceiveMsg);
    let channel_rx = channel_rx.map(LoopEvent::ChannelMsg);
    let connect_rx = connect_rx.map(LoopEvent::ConnectToRemoteServiceRequest);
    let event_rx = stream::select(transport_rx, stream::select(channel_rx, connect_rx));
    pin_mut!(event_rx);

    let mut ports: HashMap<u32, PortData<Content>> = HashMap::new();

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
            // Send data from channel.
            Some(LoopEvent::ChannelMsg(ChannelMsg::SendMsg {remote_port, content})) => {
                transport_send(MultiplexMsg::Data {port: remote_port, content}).await?;
            }

            // Local channel sender has been dropped.
            Some(LoopEvent::ChannelMsg(ChannelMsg::SenderDropped {local_port})) => {
                let port = ports.get_mut(&local_port).expect("SenderDropped for non-existing port.");
                port.tx_finished = true;
                transport_send(MultiplexMsg::Finish {port: port.remote_port()}).await?;
                // if port.tx_finished && port.rx_fin) {
                //     drop(port);
                //     ports.remove(&local_port);
                //     port_pool.release(local_port);
                // }
            }

            // Local channel receiver has been dropped.
            Some(LoopEvent::ChannelMsg(ChannelMsg::ReceiverClosed {local_port, gracefully})) => {
                let port = ports.get_mut(&local_port).expect("ReceiverClosed for non-existing port.");
                port.rx_buffer = None;
                
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
