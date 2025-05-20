// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL protocol clients.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::{Arc, Mutex};

use fidl_next_codec::{Encode, EncodeError, EncoderExt};

use crate::lockers::Lockers;
use crate::{decode_header, encode_header, ProtocolError, SendFuture, Transport, TransportExt};

use super::lockers::LockerError;

struct Shared<T: Transport> {
    responses: Mutex<Lockers<T::RecvBuffer>>,
}

impl<T: Transport> Shared<T> {
    fn new() -> Self {
        Self { responses: Mutex::new(Lockers::new()) }
    }
}

/// A sender for a client endpoint.
pub struct ClientSender<T: Transport> {
    shared: Arc<Shared<T>>,
    sender: T::Sender,
}

impl<T: Transport> ClientSender<T> {
    /// Closes the channel from the client end.
    pub fn close(&self) {
        T::close(&self.sender);
    }

    /// Send a request.
    pub fn send_one_way<M>(
        &self,
        ordinal: u64,
        request: M,
    ) -> Result<SendFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        self.send_message(0, ordinal, request)
    }

    /// Send a request and await for a response.
    pub fn send_two_way<M>(
        &self,
        ordinal: u64,
        request: M,
    ) -> Result<ResponseFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        let index = self.shared.responses.lock().unwrap().alloc(ordinal);

        // Send with txid = index + 1 because indices start at 0.
        match self.send_message(index + 1, ordinal, request) {
            Ok(future) => Ok(ResponseFuture {
                shared: &self.shared,
                index,
                state: ResponseFutureState::Sending(future),
            }),
            Err(e) => {
                self.shared.responses.lock().unwrap().free(index);
                Err(e)
            }
        }
    }

    fn send_message<M>(
        &self,
        txid: u32,
        ordinal: u64,
        message: M,
    ) -> Result<SendFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        let mut buffer = T::acquire(&self.sender);
        encode_header::<T>(&mut buffer, txid, ordinal)?;
        buffer.encode_next(message)?;
        Ok(T::send(&self.sender, buffer))
    }
}

impl<T: Transport> Clone for ClientSender<T> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone(), sender: self.sender.clone() }
    }
}

enum ResponseFutureState<'a, T: Transport> {
    Sending(SendFuture<'a, T>),
    Receiving,
    // We store the completion state locally so that we can free the locker during poll, instead of
    // waiting until the future is dropped.
    Completed,
}

/// A future for a request pending a response.
pub struct ResponseFuture<'a, T: Transport> {
    shared: &'a Shared<T>,
    index: u32,
    state: ResponseFutureState<'a, T>,
}

impl<T: Transport> Drop for ResponseFuture<'_, T> {
    fn drop(&mut self) {
        let mut responses = self.shared.responses.lock().unwrap();
        match self.state {
            // SAFETY: The future was canceled before it could be sent. The transaction ID was never
            // used, so it's safe to immediately reuse.
            ResponseFutureState::Sending(_) => responses.free(self.index),
            ResponseFutureState::Receiving => {
                if responses.get(self.index).unwrap().cancel() {
                    responses.free(self.index);
                }
            }
            // We already freed the slot when we completed.
            ResponseFutureState::Completed => (),
        }
    }
}

impl<T: Transport> ResponseFuture<'_, T> {
    fn poll_receiving(&mut self, cx: &mut Context<'_>) -> Poll<<Self as Future>::Output> {
        let mut responses = self.shared.responses.lock().unwrap();
        if let Some(ready) = responses.get(self.index).unwrap().read(cx.waker()) {
            responses.free(self.index);
            self.state = ResponseFutureState::Completed;
            Poll::Ready(Ok(ready))
        } else {
            Poll::Pending
        }
    }
}

impl<T: Transport> Future for ResponseFuture<'_, T> {
    type Output = Result<T::RecvBuffer, T::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We treat the state as pinned as long as it is sending.
        let this = unsafe { Pin::into_inner_unchecked(self) };

        match &mut this.state {
            ResponseFutureState::Sending(future) => {
                // SAFETY: Because the state is sending, we always treat its future as pinned.
                let pinned = unsafe { Pin::new_unchecked(future) };
                match pinned.poll(cx) {
                    // The send has not completed yet. Leave the state as sending.
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        // The send completed successfully. Change the state to receiving and poll
                        // for receiving.
                        this.state = ResponseFutureState::Receiving;
                        this.poll_receiving(cx)
                    }
                    Poll::Ready(Err(e)) => {
                        // The send completed unsuccessfully. We can safely free the cell and set
                        // our state to completed.

                        this.shared.responses.lock().unwrap().free(this.index);
                        this.state = ResponseFutureState::Completed;
                        Poll::Ready(Err(e))
                    }
                }
            }
            ResponseFutureState::Receiving => this.poll_receiving(cx),
            // We could reach here if this future is polled after completion, but that's not
            // supposed to happen.
            ResponseFutureState::Completed => unreachable!(),
        }
    }
}

/// A type which handles incoming events for a client.
pub trait ClientHandler<T: Transport> {
    /// Handles a received client event.
    ///
    /// The client cannot handle more messages until `on_event` completes. If `on_event` may block,
    /// perform asynchronous work, or take a long time to process a message, it should offload work
    /// to an async task.
    fn on_event(&mut self, sender: &ClientSender<T>, ordinal: u64, buffer: T::RecvBuffer);
}

/// A client for an endpoint.
///
/// It must be actively polled to receive events and two-way message responses.
pub struct Client<T: Transport> {
    sender: ClientSender<T>,
    receiver: T::Receiver,
}

impl<T: Transport> Client<T> {
    /// Creates a new client from a transport.
    pub fn new(transport: T) -> Self {
        let (sender, receiver) = transport.split();
        let shared = Arc::new(Shared::new());
        Self { sender: ClientSender { shared, sender }, receiver }
    }

    /// Returns the sender for the client.
    pub fn sender(&self) -> &ClientSender<T> {
        &self.sender
    }

    /// Runs the client with the provided handler.
    pub async fn run<H>(&mut self, mut handler: H) -> Result<(), ProtocolError<T::Error>>
    where
        H: ClientHandler<T>,
    {
        let result = self.run_to_completion(&mut handler).await;
        self.sender.shared.responses.lock().unwrap().wake_all();

        result
    }

    /// Runs the client with the [`IgnoreEvents`] handler.
    pub async fn run_sender(&mut self) -> Result<(), ProtocolError<T::Error>> {
        self.run(IgnoreEvents).await
    }

    async fn run_to_completion<H>(&mut self, handler: &mut H) -> Result<(), ProtocolError<T::Error>>
    where
        H: ClientHandler<T>,
    {
        while let Some(mut buffer) =
            T::recv(&mut self.receiver).await.map_err(ProtocolError::TransportError)?
        {
            let (txid, ordinal) =
                decode_header::<T>(&mut buffer).map_err(ProtocolError::InvalidMessageHeader)?;
            if txid == 0 {
                handler.on_event(&self.sender, ordinal, buffer);
            } else {
                let mut responses = self.sender.shared.responses.lock().unwrap();
                let locker = responses
                    .get(txid - 1)
                    .ok_or_else(|| ProtocolError::UnrequestedResponse(txid))?;

                match locker.write(ordinal, buffer) {
                    // Reader didn't cancel
                    Ok(false) => (),
                    // Reader canceled, we can drop the entry
                    Ok(true) => responses.free(txid - 1),
                    Err(LockerError::NotWriteable) => {
                        return Err(ProtocolError::UnrequestedResponse(txid));
                    }
                    Err(LockerError::MismatchedOrdinal { expected, actual }) => {
                        return Err(ProtocolError::InvalidResponseOrdinal { expected, actual });
                    }
                }
            }
        }

        Ok(())
    }
}

/// A client handler which ignores any incoming events.
pub struct IgnoreEvents;

impl<T: Transport> ClientHandler<T> for IgnoreEvents {
    fn on_event(&mut self, _: &ClientSender<T>, _: u64, _: T::RecvBuffer) {}
}
