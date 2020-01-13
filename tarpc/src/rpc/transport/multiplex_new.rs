#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(unused_imports)]
//! Multiplex transport.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak, RwLock};
use std::error::Error;
use std::mem::drop;
use std::collections::HashSet;

use async_std::task;

use futures::prelude::*;
use futures::channel::mpsc::{channel, Receiver, SendError, Sender};
use futures::lock::Mutex as AsyncMutex;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::task::Context;
use futures::task::Poll;
use futures::{pin_mut, select};
use futures::executor::block_on;
use futures::channel::oneshot;
use pin_project::pin_project;

use rand::prelude::*;
use tokio_serde::{Serializer, Deserializer};

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
    Hangup { port: u32 },
    /// Data for specified port.
    Data { port: u32, content: SerializedContent },
}

pub struct Multiplexer<ContentSerializer, ContentDeserializer> {
    content_serializer: ContentSerializer,
    content_deserializer: ContentDeserializer,

}

pub enum MultiplexTransportState {
    Ok,
    SendError,
    ReceiveError,
    ReceiverClosed
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
}

/// Creates a new channel receiver buffer.
fn channel_receiver_buffer<SerializedContent>(cfg: ChannelReceiverBufferCfg) -> 
        (ChannelReceiverBufferEnqueuer<SerializedContent>, ChannelReceiverBufferDequeuer<SerializedContent>) {
    
    assert!(cfg.resume_length > 0);
    assert!(cfg.pause_length > cfg.resume_length);
    assert!(cfg.block_length > cfg.pause_length);

    let state = AsyncMutex::new(ChannelReceiverBufferState {
        buffer: VecDeque::new(),
        item_enqueued: None,
        item_dequeued: None
    });

    let enqueuer = ChannelReceiverBufferEnqueuer {
        state: state.clone(),
        cfg: cfg.clone()
    };
    let dequeuer =  ChannelReceiverBufferDequeuer {
        state: state.clone(),
        cfg: cfg.clone()
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

impl<SerializedContent> ChannelReceiverBufferDequeuer<SerializedContent> {
    /// Dequeues an item from the receive queue.
    /// Blocks until an item becomes available.
    /// Notifies the resume notify channel when the resume queue length has been reached from above.
    async fn dequeue(&self) -> SerializedContent {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let state = self.state.lock().await;
            if state.buffer.is_empty() {
                let (tx, rx) = oneshot::channel();
                state.item_enqueued = Some(tx);
                rx_opt = Some(rx);
                continue;
            }

            let item = state.buffer.pop_front().unwrap();
            if let Some(tx) = state.item_dequeued.take() {
                let _ = tx.send(());
            }

            if state.buffer.len() == self.cfg.resume_length {
                self.cfg.resume_notify_tx.send(self.cfg.resume_notify_port).await;
            }

            return item;
        }
    }
}


#[derive(Clone)]
struct ServiceConnectData {
    service: u32,
    client_port: u32,
    server_port: u32,
}

struct ServiceConnectRequest<SerializedContent> {
    receiver_buffer_cfg: Option<ChannelReceiverBufferCfg>,
    data: ServiceConnectData,
    response_tx: Sender<ServiceConnectResponse<SerializedContent>>,
    accepted: bool
}

struct ServiceConnectResponse<SerializedContent> {
    data: ServiceConnectData,
    decision: ServiceConnectDecision<SerializedContent>,
}

enum ServiceConnectDecision<SerializedContent> {
    Accepted {enqueuer: ChannelReceiverBufferEnqueuer<SerializedContent> },
    Rejected
}

impl<SerializedContent> ServiceConnectRequest<SerializedContent> {
    pub async fn accept(self) {
        let (buf_enqueuer, buf_dequeuer) = channel_receiver_buffer(self.receiver_buffer_cfg.take().unwrap());
        self.response_tx.send(ServiceConnectResponse {
            data: self.data.clone(),
            decision: ServiceConnectDecision::Accepted {enqueuer: buf_enqueuer}
        }).await;
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
                let _ = self.response_tx.send(ServiceConnectResponse {
                    data: self.data.clone(),
                    decision: ServiceConnectDecision::Rejected
                }).await;
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
    rng: ThreadRng
}

impl NumberAllocator {
    /// Creates a new number allocator.
    fn new() -> NumberAllocator {
        NumberAllocator {
            used: HashSet::new(),
            rng: thread_rng()
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


enum LoopEvent<S, R, SerializedContent> {
    SendMsg(S),
    ReceiveMsg(R),
    ServiceConnectResponse (ServiceConnectResponse<SerializedContent>),
}

async fn run<SerializedContent, TransportSink, TransportStream, TransportStreamError>(
    cfg: Cfg,
    transport_tx: TransportSink,
    transport_rx: TransportStream,
    transport_state: Arc<RwLock<MultiplexTransportState>>,
    send_rx: Receiver<MultiplexMsg<SerializedContent>>,
    accept_tx: Arc<Mutex<HashMap<u32, Sender<ServiceConnectRequest<SerializedContent>>>>>,
) 
where TransportSink: Sink<MultiplexMsg<SerializedContent>>,
      TransportStream: Stream<Item=Result<MultiplexMsg<SerializedContent>, TransportStreamError>> {
    // Three things can happen:
    // 1. Some channel wants to send a message.
    // 2. Some data has been received.
    // 3. Some channel has fallen below the low watermark.
    // 4. Someone wants to create a channel?
    // For now assume that we have established channels.
    // 

    let mut port_pool = NumberAllocator::new();

    let (port_receive_resume_tx, port_receive_resume_rx) = channel(100);

    assert!(cfg.channel_rx_queue_length >= 2, "Channel receive queue length must be at least 2");
    let channel_receiver_buffer_cfg = ChannelReceiverBufferCfg {
        resume_length: cfg.channel_rx_queue_length / 2,
        pause_length: cfg.channel_rx_queue_length,
        block_length: cfg.channel_rx_queue_length * 2,
        resume_notify_tx: port_receive_resume_tx,
        resume_notify_port: 0
    };

    pin_mut!(transport_tx);

    let (service_connect_response_tx, service_connect_response_rx) = channel(100);
    let service_connect_response_rx = service_connect_response_rx.map(LoopEvent::ServiceConnectResponse);

    let send_rx = send_rx.map(LoopEvent::SendMsg);
    let transport_rx = transport_rx.map(LoopEvent::ReceiveMsg);
    let event_rx = futures::stream::select(send_rx, futures::stream::select(transport_rx, service_connect_response_rx));
    pin_mut!(event_rx);

    loop {
       
        match event_rx.next().await {
            // Process send queue.
            Some(LoopEvent::SendMsg(msg)) => {
                if let Err(err) = transport_tx.send(msg).await {
                    let ts = transport_state.write().unwrap();
                    *ts = MultiplexTransportState::SendError;
                    break;
                }
            },

            // Process received message.
            Some(LoopEvent::ReceiveMsg(Ok(msg))) => {
                match msg {
                    // Process service open request.
                    MultiplexMsg::Open {service, client_port} => {
                        // Allocate port and build service connect request.
                        let server_port = port_pool.allocate();                        
                        let scr = ServiceConnectRequest {
                            receiver_buffer_cfg: Some(ChannelReceiverBufferCfg {
                                resume_notify_port: server_port,
                                .. channel_receiver_buffer_cfg.clone()
                            }),
                            data: ServiceConnectData {
                                service, client_port, server_port
                            },
                            response_tx: service_connect_response_tx.clone(),
                            accepted: false
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
                }
            },

            // Process receive error.
            Some(LoopEvent::ReceiveMsg(Err(rx_err))) => {
                let ts = transport_state.write().unwrap();
                *ts = MultiplexTransportState::ReceiveError;
                break;
            },

            // Process service connect response.
            Some(LoopEvent::ServiceConnectResponse(scr)) => {

            },

            None => break
        }
    }
}

// Each channel will do the serialization, but for ease of use the multiplexor will have the 
// serializer.

/// Sends data into a multiplex channel.
pub struct MultiplexSender<SinkItem> {

}

impl<SinkItem> Sink<SinkItem> for MultiplexSender<SinkItem> {

}



