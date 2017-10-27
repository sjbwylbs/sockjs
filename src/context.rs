use std;
use std::collections::VecDeque;

use bytes::Bytes;
use futures::{Async, Future, Poll, Stream};
use futures::sync::oneshot::Sender;
use futures::sync::mpsc::{unbounded, UnboundedSender, UnboundedReceiver};
use actix::dev::*;

use session::{Session, Message};

pub enum SockJSChannel {
    Acquired(UnboundedSender<Message>),
    Released,
    Message(String),
}

/// SockJS Context
pub struct SockJSContext<A> where A: Session, A::Context: AsyncContext<A>
{
    act: A,
    state: ActorState,
    modified: bool,
    wait: ActorWaitCell<A>,
    items: ActorItemsCell<A>,
    address: ActorAddressCell<A>,
    rx: UnboundedReceiver<SockJSChannel>,
    tx: Option<UnboundedSender<Message>>,
    buf: VecDeque<Message>,
}

impl<A> ActorContext for SockJSContext<A> where A: Session<Context=Self>
{
    /// Stop actor execution
    fn stop(&mut self) {
        self.address.close();
        if self.state == ActorState::Running {
            self.state = ActorState::Stopping;
        }
    }

    /// Terminate actor execution
    fn terminate(&mut self) {
        self.address.close();
        self.items.close();
        self.state = ActorState::Stopped;
    }

    /// Actor execution state
    fn state(&self) -> ActorState {
        self.state
    }
}

impl<A> AsyncContext<A> for SockJSContext<A> where A: Session<Context=Self>
{
    fn spawn<F>(&mut self, fut: F) -> SpawnHandle
        where F: ActorFuture<Item=(), Error=(), Actor=A> + 'static
    {
        self.modified = true;
        self.items.spawn(fut)
    }

    fn wait<F>(&mut self, fut: F)
        where F: ActorFuture<Item=(), Error=(), Actor=A> + 'static
    {
        self.modified = true;
        self.wait.add(fut);
    }

    fn cancel_future(&mut self, handle: SpawnHandle) -> bool {
        self.modified = true;
        self.items.cancel_future(handle)
    }
}

impl<A> AsyncContextApi<A> for SockJSContext<A> where A: Session<Context=Self> {
    fn address_cell(&mut self) -> &mut ActorAddressCell<A> {
        &mut self.address
    }
}

impl<A> SockJSContext<A> where A: Session<Context=Self>
{
    #[doc(hidden)]
    pub fn subscriber<M>(&mut self) -> Box<Subscriber<M>>
        where A: Handler<M>,
              M: ResponseType + 'static
    {
        Box::new(self.address.unsync_address())
    }

    /// Manually expire a session
    pub fn expire(&mut self) {
        unimplemented!()
    }

    /// Send message to peer
    pub fn send<M>(&mut self, message: M) where M: Into<Message>
    {
        let msg = message.into();
        if let Some(ref mut tx) = self.tx {
            match tx.unbounded_send(msg) {
                Ok(()) => return,
                Err(err) => self.buf.push_back(err.into_inner())
            }
        } else {
            self.buf.push_back(msg);
            return
        }
        self.tx.take();
    }

    /// Close session
    pub fn close(&mut self, code: usize, reason: String) {
        unimplemented!()
    }

    /// Check if transport connected to peer
    pub fn connected(&mut self) -> bool {
        unimplemented!()
    }
}

impl<A> SockJSContext<A> where A: Session<Context=Self>
{
    pub(crate) fn start() -> (SyncAddress<A>, UnboundedSender<SockJSChannel>)
    {
        let (tx, rx) = unbounded();

        let mut ctx = SockJSContext {
            act: A::default(),
            state: ActorState::Started,
            modified: false,
            items: ActorItemsCell::default(),
            address: ActorAddressCell::default(),
            wait: ActorWaitCell::default(),
            tx: None,
            rx: rx,
            buf: VecDeque::new(),
        };
        let addr = ctx.address();
        Arbiter::handle().spawn(ctx);
        (addr, tx)
    }
}

#[doc(hidden)]
impl<A> Future for SockJSContext<A> where A: Session<Context=Self>
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ctx: &mut SockJSContext<A> = unsafe {
            std::mem::transmute(self as &mut SockJSContext<A>)
        };

        // update state
        match self.state {
            ActorState::Started => {
                Actor::started(&mut self.act, ctx);
                self.state = ActorState::Running;
            },
            ActorState::Stopping => {
                Actor::stopping(&mut self.act, ctx);
            }
            _ => ()
        }

        let mut prep_stop = false;
        loop {
            self.modified = false;

            // check wait futures
            if self.wait.poll(&mut self.act, ctx) {
                return Ok(Async::NotReady)
            }

            // incoming messages
            self.address.poll(&mut self.act, ctx);

            // spawned futures and streams
            self.items.poll(&mut self.act, ctx);

            // sockjs channel
            loop {
                match self.rx.poll() {
                    Ok(Async::Ready(Some(msg))) => {
                        match msg {
                            SockJSChannel::Acquired(tx) => {
                                while let Some(msg) = self.buf.pop_front() {
                                    let _ = tx.unbounded_send(msg);
                                };
                                self.tx = Some(tx);
                                self.act.acquired(ctx);
                            }
                            SockJSChannel::Released => self.act.released(ctx),
                            SockJSChannel::Message(s) => {
                                let fut = ResponseFuture{
                                    resp: Handler::handle(
                                        &mut self.act, Message::Str(s), ctx)};
                                self.spawn(fut);
                            }
                        }
                        continue
                    },
                    Ok(Async::Ready(None)) =>
                        break,
                    Ok(Async::NotReady) =>
                        break,
                    Err(_) => {},
                }
            }

            // are we done
            if self.modified {
                continue
            }

            // check state
            match self.state {
                ActorState::Stopped => {
                    self.state = ActorState::Stopped;
                    Actor::stopped(&mut self.act, ctx);
                    return Ok(Async::Ready(()))
                },
                ActorState::Stopping => {
                    if prep_stop {
                        if self.address.connected() || !self.items.is_empty() {
                            self.state = ActorState::Running;
                            continue
                        } else {
                            self.state = ActorState::Stopped;
                            Actor::stopped(&mut self.act, ctx);
                            return Ok(Async::Ready(()))
                        }
                    } else {
                        Actor::stopping(&mut self.act, ctx);
                        prep_stop = true;
                        continue
                    }
                },
                ActorState::Running => {
                    if !self.address.connected() && self.items.is_empty() {
                        self.state = ActorState::Stopping;
                        Actor::stopping(&mut self.act, ctx);
                        prep_stop = true;
                        continue
                    }
                },
                _ => (),
            }

            return Ok(Async::NotReady)
        }
    }
}

impl<A> std::fmt::Debug for SockJSContext<A> where A: Session<Context=Self> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "SockJSContext({:?}: actor:{:?}) {{ state: {:?}, connected: {}, items: {} }}",
               self as *const _,
               &self.act as *const _,
               self.state, "-", self.items.is_empty())
    }
}

impl<A> ToEnvelope<A> for SockJSContext<A>
    where A: Session<Context=SockJSContext<A>>,
{
    fn pack<M>(msg: M, tx: Option<Sender<Result<M::Item, M::Error>>>) -> Envelope<A>
        where A: Handler<M>,
              M: ResponseType + Send + 'static,
              M::Item: Send,
              M::Error: Send
    {
        RemoteEnvelope::new(msg, tx).into()
    }
}


struct ResponseFuture<A, M> where A: Actor, M: ResponseType {
    resp: Response<A, M>
}

impl<A, M> ActorFuture for ResponseFuture<A, M> where A: Actor, M: ResponseType {
    type Item = ();
    type Error = ();
    type Actor = A;

    fn poll(&mut self, act: &mut A, ctx: &mut A::Context) -> Poll<Self::Item, Self::Error>
    {
        match self.resp.poll_response(act, ctx) {
            Ok(Async::Ready(_)) => Ok(Async::Ready(())),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => Err(()),
        }
    }
}
