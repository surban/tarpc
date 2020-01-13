#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(unused_imports)]
//! Multiplex transport.

use async_std::task;
use futures::channel::mpsc::{channel, Receiver, SendError, Sender};
use futures::lock::Mutex;
use futures::prelude::*;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::task::Context;
use futures::task::Poll;
use futures::{pin_mut, select};
use pin_project::pin_project;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use async_std::sync as async_channel;

use crate::Transport;
use std::error::Error;

type ChannelId = u32;

pub trait InnerSerializer<T, I> {
    type Error;
    fn serialize(&mut self, item: &T) -> Result<I, Self::Error>;
}

pub trait InnerDeserializer<T, I> {
    type Error;
    fn deserialize(&mut self, src: &I) -> Result<T, Self::Error>;
}

#[derive(Debug)]
pub enum MultiplexRequestMsg<IntSer> {
    Open,
    Request(IntSer),
    Close,
}

#[derive(Debug)]
pub enum MultiplexResponseMsg<IntSer> {
    Opened,
    Refused,
    Response(IntSer),
    Pause,
    Resume,
    Closed,
}

trait RequestChannelReceivePartTrait<IntSer> {
    fn take_response(
        &mut self,
        rsp: MultiplexResponseMsg<IntSer>,
    ) -> Box<dyn Future<Output = bool>>;
}

// Nah, seems nice to have a channel part that cares about the channel and registers with the multiplexor.
// How to handle construction?
// The user instantiates a channel and gives the multiplexor as argument.
// The channel registers itself with the multiplexor and gives the receive part to the multiplexor.
// How is the connection established?
// In the initializer.
// How is the connection closed? Best would be if user drops the sender/receiver.
// Yes, no problem, we can catch that and send a close message.
// Can we have a channel that is one-way closed?
// Yes.
// So all the user gets is a Sender/Receiver => for now, yes.

struct RequestChannelReceivePart<StreamItem, IntDeserializer> {
    upper_tx: Sender<StreamItem>,
    int_deserializer: IntDeserializer,
}

struct RequestChannelSendPart<SinkItem, IntSerializer> {
    upper_rx: Receiver<SinkItem>,
    int_serializer: IntSerializer,
    send_lock: Arc<Mutex<()>>,
}

#[derive(Debug)]
enum RequestChannelState {
    Closed,
    Connecting,
    Connected,
}

// Shall we have the upper transport or just provide the sink?
// Just make sure we are being called.
//

//impl<IntSer, StreamItem, SinkItem, IntSerializer, IntDeserializer> RequestChannelReceivePartTrait<IntSer>
//    for RequestChannelReceivePart<StreamItem, IntDeserializer>
//where
//    IntDeserializer: InnerDeserializer<SinkItem, IntSer>,
//{
//    fn take_response(&mut self, rsp: MultiplexResponseMsg<IntSer>) -> Box<Future<Output=bool>> {
//        Box::new(async {
//            match &rsp.msg {
//                MultiplexResponseMsg::Opened => {
//                    self.state = RequestChannelState::Connected;
//                    true
//                },
//                MultiplexResponseMsg::Refused => {
//                    // Connection refused
//                    false
//                }
//                MultiplexResponseMsg::Response(rsp) => {
//                    match self.int_deserializer.deserialize(rsp) {
//                        Ok(rsp) => {
//                            match self.upper_tx.send(rsp).await {
//                                Ok(()) => (),
//                                Err(err) => {
//                                    // Upper transport closed, need to close channel.
//                                    false
//                                }
//                            }
//                        },
//                        Err(err) => {
//                            // Deserialization failed, close channel.
//                            false
//                        }
//                    }
//                }
//                MultiplexResponseMsg::Pause => {
//                    // Acquire mutex.
//                    // Where will we hold the lock?
//                    // The sender thread will have one... but where is it?
//                }
//                MultiplexResponseMsg::Resume => {}
//                MultiplexResponseMsg::Closed => {}
//            }
//        })
//    }
//}
//
//
//#[derive(Debug)]
//enum ResponseChannel<IntSer> {
//    Closed,
//    Listening,
//    Connected {
//        rx_queue: Sender<IntSer>
//    }
//}
//
//struct MultiplexorClient<IntSer, T>
//{
//    transport: T,
//    req_tx_queue: Sender<MultiplexRequest<IntSer>>,
//    req_control_queue: Sender<MultiplexRequest<IntSer>>,
//    req_channel: HashMap<ChannelId, Box<dyn RequestChannelTrait<IntSer>>>,
//
//}

// How can we add a channel to the HashMap?
// Not a problem, just put it behind a mutex.
// So transport will be owned by the task.
// No need to keep it here.

#[derive(Debug)]
enum LowerMultiplexorError {
    ChannelAlreadyInUse(ChannelId),
    ConnectionClosed,
}

#[derive(Debug)]
enum MultiplexorError<SerErr, DeserErr> {
    ChannelAlreadyInUse(ChannelId),
    ConnectionClosed,
    ConnectionRefused(ChannelId),
    SerializationError(SerErr),
    DeserializationError(DeserErr),
    UnexpectedResponse,
}

impl<SerErr, DeserErr> From<LowerMultiplexorError> for MultiplexorError<SerErr, DeserErr> {
    fn from(err: LowerMultiplexorError) -> Self {
        match err {
            LowerMultiplexorError::ChannelAlreadyInUse(id) => Self::ChannelAlreadyInUse(id),
            LowerMultiplexorError::ConnectionClosed => Self::ConnectionClosed,
        }
    }
}

#[derive(Debug)]
pub enum MultiplexorChannelRegisterError {
    ChannelAlreadyInUse(ChannelId),
    TransportClosed
}

#[derive(Debug)]
pub enum MultiplexorConnectError<SerErr, DeserErr> {
    ChannelAlreadyInUse(ChannelId),
    ConnectionRefused(ChannelId),
    TransportClosed,
    UnexpectedResponse,
    SerializationError(SerErr),
    DeserializationError(DeserErr)
}

impl<SerErr, DeserErr> From<MultiplexorChannelRegisterError> for MultiplexorConnectError<SerErr, DeserErr> {
    fn from(err: MultiplexorChannelRegisterError) -> Self {
        match err {
            MultiplexorChannelRegisterError::ChannelAlreadyInUse(id) => Self::ChannelAlreadyInUse(id),
            MultiplexorChannelRegisterError::TransportClosed => Self::TransportClosed
        }
    }
}

impl<SerErr, DeserErr> From<MultiplexorSendError<SerErr>> for MultiplexorConnectError<SerErr, DeserErr> {
    fn from(err: MultiplexorSendError<SerErr>) -> Self {
        match err {
            MultiplexorSendError::TransportClosed => Self::TransportClosed,
            MultiplexorSendError::SerializationError(err) => Self::SerializationError(err),
        }
    }
}


#[derive(Debug)]
pub enum MultiplexorSendError<SerErr> {
    TransportClosed,
    SerializationError(SerErr),
}

impl<SerErr> From<SendError> for MultiplexorSendError<SerErr> {
    fn from(err: SendError) -> Self {
        assert!(!err.is_full(), "Must not send into non-ready channel.");
        MultiplexorSendError::TransportClosed
    }
}

//fn request_channel<SinkItem, StreamItem, IntSer, T>(channel: ChannelId, lm: &mut LowerMultiplexor<IntSer, T>)
//    -> Result<(Sender<SinkItem>, Receiver<StreamItem>), LowerMultiplexorError> {
//
//    let (tx, rx) = lm.register(channel)
//
//
//}


#[derive(Debug)]
pub enum Msg<IntSer> {
    Open,
    Opened,
    Refused,
    Msg (IntSer),
    Pause,
    Resume, 
    Finish
}

#[derive(Debug)]
pub enum MultiplexResponseMsg<IntSer> {
    Opened,
    Refused,
    Response(IntSer),
    Pause,
    Resume,
    Closed,
}


struct MultiplexPack<IntSer> {
    channel: ChannelId,
    content: Msg<IntSer>,
}



pub struct MultiplexChannel<SinkItem, StreamItem, SerErr, DeserErr> {
    sink: Box<dyn Sink<SinkItem, Error = MultiplexorError<SerErr, DeserErr>>>,
    stream: Box<dyn Stream<Item = Result<StreamItem, MultiplexorError<SerErr, DeserErr>>>>,
}

impl<SinkItem, StreamItem, SerErr, DeserErr> fmt::Debug
    for MultiplexChannel<SinkItem, StreamItem, SerErr, DeserErr>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MultiplexChannel")
    }
}

#[pin_project]
pub struct MultiplexChannelSink<Item, SerErr> {
    #[pin]
    sink: Pin<Box<dyn Sink<Item, Error = MultiplexorSendError<SerErr>>>>,
    channel: ChannelId,
}

impl<Item, SerErr> fmt::Debug for MultiplexChannelSink<Item, SerErr> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MultiplexChannelSink {}", &self.channel)
    }
}

impl<Item, SerErr> Sink<Item> for MultiplexChannelSink<Item, SerErr> {
    type Error = MultiplexorSendError<SerErr>;
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

use std::marker::PhantomData;

pub struct MultiplexChannelSink2<Item, SerErr> {
    sink: Pin<Box<dyn Sink<Item, Error = MultiplexorSendError<SerErr>>>>,
    channel: ChannelId,
}

impl<Item, IntSer, Ser, SerErr, DeserErr> MultiplexChannelSink2<Item, SerErr> 
where Ser: InnerSerializer<MultiplexRequestMsg<Item>, IntSer, Error = SerErr> + 'static,
{
    async fn new(mut int_ch: InternalChannelWrapper<Item, IntSer, Ser>) -> Result<MultiplexChannelSink2<Item, SerErr>, 
        MultiplexorConnectError<SerErr, DeserErr>> {

        let msg = MultiplexRequestMsg::Open;
        int_ch.send(msg).await?;

        // Now wait for the reply message.
        let reply_ser_msg = rx.next().await.ok_or(MultiplexorConnectError::TransportClosed)?;
        let reply_msg = deserializer
            .deserialize(&reply_ser_msg)
            .map_err(MultiplexorConnectError::DeserializationError)?;
        match reply_msg {
            MultiplexResponseMsg::Opened => (),
            MultiplexResponseMsg::Refused => return Err(MultiplexorConnectError::ConnectionRefused(id)),
            _ => return Err(MultiplexorConnectError::UnexpectedResponse),
        }            

    }
}

struct InternalChannelWrapper<IntSer, SinkItem, StreamItem, Ser, Deser> 
{
    id: ChannelId,
    lower_tx: Sender<MultiplexPack<IntSer>>,
    lower_rx: Receiver<MultiplexPack<IntSer>>,
    serializer: Ser,
    deserializer: Deser,
    _sink_item: PhantomData<SinkItem>,
    _stream_item: PhantomData<StreamItem>,
}

struct InternalChannelReceiverBackend<IntSer, SinkItem, StreamItem, Ser, Deser> {
    rx_queue: Sender<MultiplexPack<IntSer>>,
    lower_tx: Sender<MultiplexPack<IntSer>>,

}

use futures::channel::oneshot;

// So how will this object be accessed externally?
// Using an Arc<_>.
struct ChannelReceiverBuffer<Item, IntSer> {
    state: Mutex<ChannelReceiverBufferState<Item, IntSer>>,
    resume_length: usize,
    pause_length: usize,
    block_length: usize,
}

struct ChannelReceiverBufferState<Item, IntSer> {
    buffer: VecDeque<Item>,
    item_enqueued: Option<oneshot::Sender<()>>,
    item_dequeued: Option<oneshot::Sender<()>>,
    flow_control_tx: Sender<MultiplexPack<IntSer>>
}

use std::mem;

impl<Item, IntSer> ChannelReceiverBuffer<Item, IntSer> {
    async fn enqueue(&self, item: Item) {
        let mut rx_opt = None;
        loop {
            if let Some(rx) = rx_opt {
                let _ = rx.await;
            }

            let state = self.state.lock().await;
            if state.buffer.len() >= self.block_length {
                let (tx, rx) = oneshot::channel();
                state.item_dequeued = Some(tx);
                rx_opt = Some(rx);
                continue;
            }

            state.buffer.push_back(item);
            if let Some(tx) = state.item_enqueued.take() {
                let _ = tx.send(());
            }
            
            if state.buffer.len() == self.pause_length {
                state.flow_control_tx.send(M)
            }
        }
    }
}


impl<IntSer, SinkItem, StreamItem, Ser, Deser, SerErr, DeserErr> InternalChannelWrapper<IntSer, SinkItem, StreamItem, Ser, Deser> 
where Ser:   InnerSerializer<MultiplexRequestMsg<SinkItem>, IntSer, Error = SerErr> + 'static,
      Deser: InnerDeserializer<MultiplexResponseMsg<StreamItem>, IntSer, Error=DeserErr> + 'static,
{
    async fn send(&mut self, msg: MultiplexRequestMsg<SinkItem>) -> Result<(), MultiplexorSendError<SerErr>> {
        let ser_msg = self.serializer
            .serialize(&msg)
            .map_err(MultiplexorSendError::SerializationError);
        let pack = MultiplexPack {
            channel: self.id,
            content: ser_msg?,
        };
        self.lower_tx.send(pack).await?;        
        Ok(())
    }

    //async fn push_recived(&)

    // But how to trigger the high watermark?
    // Have a separate task running?
    // No, move it to a lower layer.
    // No, moving it to a lower layer is a bad idea.
    // Keep it at the channel level.
    // So how to implement it at the channel level without delay?
    // Well, need something that we can use to immediatley call into the channel.

}


struct LowerMultiplexor<IntSer> {
    tx: Sender<MultiplexPack<IntSer>>,
    rx_channels: Weak<Mutex<HashMap<ChannelId, async_channel::Sender<MultiplexPack<IntSer>>>>>,
}

impl<IntSer> LowerMultiplexor<IntSer>
where
    IntSer: Send + 'static,
{
    pub fn new<T>(transport: T) -> LowerMultiplexor<IntSer>
    where
        T: Sink<MultiplexPack<IntSer>> + Stream<Item = MultiplexPack<IntSer>> + Send + 'static,
    {
        let rx_channels = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = channel(100);

        let rx_channels_task = rx_channels.clone();
        task::spawn(async move { Self::tx_rx_task(transport, rx, rx_channels_task).await });

        LowerMultiplexor {
            tx,
            rx_channels: Arc::downgrade(&rx_channels),
        }
    }

    fn map_serialization_error<SerErr, DeserErr>(
        err: SerErr,
    ) -> MultiplexorError<SerErr, DeserErr> {
        MultiplexorError::SerializationError(err)
    }

    fn map_deserialization_error<SerErr, DeserErr>(
        err: DeserErr,
    ) -> MultiplexorError<SerErr, DeserErr> {
        MultiplexorError::DeserializationError(err)
    }

    pub async fn request_channel<SinkItem, StreamItem, Ser, Deser, SerErr, DeserErr>(
        &mut self,
        id: ChannelId,
        buffer: usize,
        mut serializer: Ser,
        mut deserializer: Deser,
    ) -> Result<
        MultiplexChannel<SinkItem, StreamItem, SerErr, DeserErr>,
        MultiplexorConnectError<SerErr, DeserErr>
    >
    where
        SinkItem: 'static,
        Ser: InnerSerializer<MultiplexRequestMsg<SinkItem>, IntSer, Error = SerErr> + 'static,
        Deser: InnerDeserializer<MultiplexResponseMsg<StreamItem>, IntSer, Error = DeserErr>,
        SerErr: 'static,
    {
        let (mut tx, mut rx) = self.register(id, buffer, serializer).await?;

        let msg = MultiplexRequestMsg::Open;
        tx.send(msg).await?;

        // Now wait for the reply message.
        let reply_ser_msg = rx.next().await.ok_or(MultiplexorConnectError::TransportClosed)?;
        let reply_msg = deserializer
            .deserialize(&reply_ser_msg)
            .map_err(MultiplexorConnectError::DeserializationError)?;
        match reply_msg {
            MultiplexResponseMsg::Opened => (),
            MultiplexResponseMsg::Refused => return Err(MultiplexorConnectError::ConnectionRefused(id)),
            _ => return Err(MultiplexorConnectError::UnexpectedResponse),
        }

        // Okay, channel is open.
        // Create the proxy channels.

        let send_semaphore = Arc::new(Mutex::new(()));
        let send_semaphore_tx = send_semaphore.clone();
        let tx_adapted = tx.with(move |item| {
            let send_semaphore_tx_async = send_semaphore_tx.clone();
            let tx_serialized = serializer
                .serialize(&item)
                .map_err(MultiplexorSendError::SerializationError);
            async move {
                let _send_permit = send_semaphore_tx_async.lock().await;
                let tx_pack = MultiplexPack {
                    channel: id,
                    content: tx_serialized?,
                };
                Ok::<_, MultiplexorSendError<SerErr>>(tx_pack)
            }
        });
        let tx_channel = MultiplexChannelSink {
            sink: Box::pin(tx_adapted),
            channel: id,
        };

        // let (tx_upper, rx_upper) = channel(1);
        // task::spawn(async move {
        //     loop {
        //         match rx_upper.next().await {
        //             Some(upper_msg) => {
        //                 let ser_upper_msg = codec.serialize(&upper_msg).map_err(Self::map_serialization_error);//.unwrap();
        //                 //let msg = MultiplexRequestMsg::Request(ser_upper_msg);
        //             }
        //         }
        //     }
        // });

        //tx.send()
        todo!()
    }

    async fn register<SinkItem, Ser, SerErr>(
        &mut self,
        id: ChannelId,
        buffer: usize,
        serializer: Ser,
    ) -> Result<
        (
            InternalChannelWrapper<SinkItem, IntSer, Ser>,
            async_channel::Receiver<MultiplexPack<IntSer>>,
        ),
        MultiplexorChannelRegisterError,
    > 
    where Ser: InnerSerializer<MultiplexRequestMsg<SinkItem>, IntSer, Error = SerErr> + 'static, 
    {
        // Allocate receiver channel and add it to receive channel table.
        let (receiver_tx, receiver_rx) = async_channel::channel(buffer);
        match self.rx_channels.upgrade() {
            Some(rx_channels) => {
                let mut rx_channels = rx_channels.lock().await;
                if rx_channels.contains_key(&id) {
                    return Err(MultiplexorChannelRegisterError::ChannelAlreadyInUse(id));
                } else {
                    rx_channels.insert(id, receiver_tx);
                }
            }
            None => {
                return Err(MultiplexorChannelRegisterError::TransportClosed);
            }
        }

        // Allocate sender for transmit queue.
        let sender_tx = self.tx.clone();

        let wrapper = InternalChannelWrapper {
            id,
            lower_tx: sender_tx,
            lower_rx: receiver_rx,
            serializer,
            deserializer,
            _sink_item: PhantomData,
            _stream_item: PhantomData,
        };
        Ok(wrapper)
    }

    async fn tx_rx_task<T>(
        transport: T,
        tx_queue: Receiver<MultiplexPack<IntSer>>,
        rx_channels: Arc<Mutex<HashMap<ChannelId, async_channel::Sender<IntSer>>>>,
    ) where
        T: Sink<MultiplexPack<IntSer>> + Stream<Item = MultiplexPack<IntSer>> + Send,
    {
        pin_mut!(transport, tx_queue);
        loop {
            let mut transport_next = transport.next().fuse();
            let mut tx_queue_next = tx_queue.next().fuse();
            select! {
                rx_pkt = transport_next => {
                    match rx_pkt {
                        Some(MultiplexPack {channel, content}) => {
                            let rx_channels = rx_channels.lock().await;
                            match rx_channels.get(&channel) {
                                Some(rx_channel) => {
                                    let _ = rx_channel.send(content).await;
                                },
                                None => {
                                    // Warn that we have no channel?
                                }
                            }
                        },
                        None => break
                    }
                },
                tx_pkt = tx_queue_next => {
                    match tx_pkt {
                        Some(tx_pkt) => { let _ = transport.send(tx_pkt).await; }
                        None => break
                    }
                }
            }
        }
    }
}

//#[pin_project]
//#[derive(Debug)]
//pub struct MultiplexItem<Item> {
//    #[pin]
//    pub channel: u32,
//    #[pin]
//    pub item: Item,
//}

// So the transport could take any item that we pass, but it must be able to serialize and deserialize it.
// If we have something that takes a dyn, will it be possible to serialize/deserialize it?
// So serde will not happily serialize or deserialize this thingy...
// Okay, alternative is our own transport layer below TARPC.
// This will allow
// Or not?
// What if we manually implement Serialize and Deserialize for our type?
// We know exactly what types we need and we could do the dispatch dynamically, can't we?

// Okay, let's think through that idea.
// 1. Our struct has a HashMap from channel id to type.
// 2. Question is how a serde Deserializer works.
//

// Okay, let's design the transport at the lower level.
// Assume serde serialization to make progress.
// So we want a multiplexer now at a lower level.
// Transport is TCP/IP-centric.
// So define our own transport?
// Yes, probably.
// Hmm, TARPC even does not allow accessing the context when handling a request.

// So how would we set up multiple channels?
// Server can obtain transport during listening for TCP connections.
// Client should also be able to setup multiple transports.

// Is compatibility with an existing transport important?
// I don't think so, because the existing transport is TCP specific and we want other protocols
// like Bluetooth and serial.

// So let's use a super generic underlying transport, it's only property must be that it delivers
// packets and does so in-order and reliably.

// Do we want to define serialization here?
// We should definitely not fix it, because Bincode might be better for some applications than JSON.

// So we need a codec, which isn't a problem.
// And now the big question is how to do the packaging?
// The one question is what we get from the inner serializer?
// I.e. if it gives us a composable JSON format, we could insert it into the packaged format.
// Let's check what serde_json is able of returning.
// It would be possible with serde_json, however this approach is not portable across serializers...
// But maybe possible to salvage that.
// I.e. we have an inner serializer that serializes all inner messages to a type IS.
// Then we have an outer serializer that accepts objects which contain IS and serializes them to outer type OS.
// It would be possible with serde.
// Maybe this approach can even be generialized by serializing into an enum?
// Or we just force the user to map from ClientMessage<T> to a enum for all channels?
// Can this be auto-generated?
// Maybe the derive macro could be extended to do that?
// But it's tricky, so avoid it.

// Okay, let's do the inner/outer serializer thing that will work nicely for our application with serde.

// So we need to define a new trait that serializes into the intermediate representation.

// Okay, we have the traits, what is next?
// The next is to define the multiplexor channel and the multiplexor transport.
// Let us see what a channel must implement for tarpc.
// Only needs Stream and Sink from futures, as before.
// Okay, so next?
// We need to define the message again using I.
// Then we need to see how to define the connection to the underlying transport.
// Okay, cool.

// Let us define a trait for the underlying transport.
// Should it be a simple a Stream/Sink?
// Yes, why not?

// So one global transmit queue?
// Yes.

//
//
//struct MultiplexRequestChannel<ReqItem> {
//    id: ChannelId,
//    send_permit: Arc<Mutex<()>>,
//    tx: Sender<dyn SendableItem>
//    //send_tx: Sender<>
//    // Now this is difficult in another sense.
//    // We don't know how to build a sender/receiver that can join all the channels...
//    // Hmm, difficulties...
//    // So possibility 1: do serialization here.
//    // Which is against the principles of TARPC, because in could do in-memory transport without
//    // the need for serialization.
//    // Okay, so let's move this idea into a lower layer, i.e. below serialization.
//    // It will mean that serialization is required, because we are dealing with buffers of some kind.
//    // Or can't we just trick around here and do some dyn casting?
//    // Hmmm... we need a type to put the messages into.
//    // A channel could send a dyn something
//}
//

//
//
//#[derive(Debug)]
//pub struct Multiplexer<InnerTransport, Item> {
//    inner: InnerTransport,
//    req_channel: HashMap<u32, Channel<Item>>
//}
//
//#[derive(Debug)]
//pub struct MultiplexChannel<InnerTransport, Item> {
//    multiplexer: Multiplexer<InnerTransport>,
//    channel_id: u32,
//    rx_queue: Receiver<Item>
//}
//
//
//impl<Item, InnerTransport> Stream for MultiplexChannel<InnerTransport, Item>
//where
//    InnerTransport: Stream<Item = MultiplexItem<Item>>,
//{
//    type Item = Item;
//
//    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
//        let this = self.project();
//        this.rx_queue.next()
//    }
//}
//
//impl<Item, InnerTransport> Sink<Item> for MultiplexChannel<InnerTransport>
//where
//    InnerTransport: Sink<MultiplexItem<Item>>,
//{
//    type Error = io::Error;
//    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
//        unimplemented!()
//    }
//
//    fn start_send(self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
//        unimplemented!()
//    }
//
//    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
//        unimplemented!()
//    }
//
//    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
//        unimplemented!()
//    }
//}
