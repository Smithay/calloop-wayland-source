// SPDX-License-Identifier: MIT

//! Utilities for using an [`EventQueue`] from wayland-client with an event loop
//! that performs polling with [`calloop`](https://crates.io/crates/calloop).
//!
//! # Example
//!
//! ```no_run,rust
//! use calloop::EventLoop;
//! use calloop_wayland_source::WaylandSource;
//! use wayland_client::{Connection, QueueHandle};
//!
//! // Create a Wayland connection and a queue.
//! let connection = Connection::connect_to_env().unwrap();
//! let event_queue = connection.new_event_queue();
//! let queue_handle = event_queue.handle();
//!
//! // Create the calloop event loop to drive everytihng.
//! let mut event_loop: EventLoop<()> = EventLoop::try_new().unwrap();
//! let loop_handle = event_loop.handle();
//!
//! // Insert the wayland source into the calloop's event loop.
//! WaylandSource::new(event_queue).unwrap().insert(loop_handle).unwrap();
//!
//! // This will start dispatching the event loop and processing pending wayland requests.
//! while let Ok(_) = event_loop.dispatch(None, &mut ()) {
//!     // Your logic here.
//! }
//! ```

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use calloop::generic::{FdWrapper, Generic};
use calloop::{
    EventSource, InsertError, Interest, LoopHandle, Mode, Poll, PostAction, Readiness,
    RegistrationToken, Token, TokenFactory,
};
use rustix::io::Errno;
use wayland_backend::client::{ReadEventsGuard, WaylandError};
use wayland_client::{DispatchError, EventQueue};

#[cfg(feature = "log")]
use log::error as log_error;
#[cfg(not(feature = "log"))]
use std::eprintln as log_error;

/// An adapter to insert an [`EventQueue`] into a calloop
/// [`EventLoop`](calloop::EventLoop).
///
/// This type implements [`EventSource`] which generates an event whenever
/// events on the event queue need to be dispatched. The event queue available
/// in the callback calloop registers may be used to dispatch pending
/// events using [`EventQueue::dispatch_pending`].
///
/// [`WaylandSource::insert`] can be used to insert this source into an event
/// loop and automatically dispatch pending events on the event queue.
#[derive(Debug)]
pub struct WaylandSource<D> {
    queue: EventQueue<D>,
    fd: Generic<FdWrapper<RawFd>>,
    read_guard: Option<ReadEventsGuard>,
    /// Calloop's before_will_sleep method allows
    /// skipping the sleeping by returning a `Token`.
    /// We cannot produce this on the fly, so store it here instead
    fake_token: Option<Token>,
    // Some calloop event handlers don't support error handling, so we have to store the error
    // for a short time until we reach a method which allows it
    stored_error: Result<(), io::Error>,
}

impl<D> WaylandSource<D> {
    /// Wrap an [`EventQueue`] as a [`WaylandSource`].
    ///
    /// If this returns None, that means that acquiring a read guard failed.
    /// See [EventQueue::prepare_read] for details
    /// This guard is only used to get the wayland file descriptor
    #[must_use]
    pub fn new(queue: EventQueue<D>) -> Option<WaylandSource<D>> {
        let guard = queue.prepare_read()?;
        let fd = Generic::new(
            // Safety: `connection_fd` returns the wayland socket fd,
            // and EventQueue (eventually) owns this socket
            // fd is only used in calloop, which guarantees
            // that the source is unregistered before dropping it, so the
            // fd cannot be used after being freed
            // Wayland-backend should probably document some guarantees here to make this sound,
            // but we know that they are unlikely to make the queue have a different socket/fd
            // - there is no public API to do so
            unsafe { FdWrapper::new(guard.connection_fd().as_raw_fd()) },
            Interest::READ,
            Mode::Level,
        );
        drop(guard);

        Some(WaylandSource { queue, fd, read_guard: None, fake_token: None, stored_error: Ok(()) })
    }

    /// Access the underlying event queue
    ///
    /// Note that you should be careful when interacting with it if you invoke
    /// methods that interact with the wayland socket (such as `dispatch()`
    /// or `prepare_read()`). These may interfere with the proper waking up
    /// of this event source in the event loop.
    pub fn queue(&mut self) -> &mut EventQueue<D> {
        &mut self.queue
    }

    /// Insert this source into the given event loop.
    ///
    /// This adapter will pass the event loop's shared data as the `D` type for
    /// the event loop.
    pub fn insert(self, handle: LoopHandle<D>) -> Result<RegistrationToken, InsertError<Self>>
    where
        D: 'static,
    {
        handle.insert_source(self, |_, queue, data| queue.dispatch_pending(data))
    }
}

impl<D> EventSource for WaylandSource<D> {
    type Error = calloop::Error;
    type Event = ();
    /// The underlying event queue.
    ///
    /// You should call [`EventQueue::dispatch_pending`] inside your callback
    /// using this queue.
    type Metadata = EventQueue<D>;
    type Ret = Result<usize, DispatchError>;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn process_events<F>(
        &mut self,
        _: Readiness,
        _: Token,
        mut callback: F,
    ) -> Result<PostAction, Self::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        let queue = &mut self.queue;
        // Take the stored error
        std::mem::replace(&mut self.stored_error, Ok(()))?;

        // Because our polling is based on a `Generic` source, in theory we might want
        // to to call the process_events handler on fd. However,
        // we know that Generic's `process_events` call is a no-op, so we can just
        // handle the event ourselves

        // Dispatch any pending events in the queue
        Self::loop_callback_pending(queue, &mut callback)?;

        // Once dispatching is finished, flush the responses to the compositor
        if let Err(WaylandError::Io(e)) = queue.flush() {
            if e.kind() != io::ErrorKind::WouldBlock {
                // in case of error, forward it and fast-exit
                return Err(e.into());
            }
            // WouldBlock error means the compositor could not process all
            // our messages quickly. Either it is slowed
            // down or we are a spammer. Should not really
            // happen, if it does we do nothing and will flush again later
        }

        Ok(PostAction::Continue)
    }

    fn register(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.fake_token = Some(token_factory.token());
        self.fd.register(poll, token_factory)
    }

    fn reregister(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.fd.reregister(poll, token_factory)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.fd.unregister(poll)
    }

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        debug_assert!(self.read_guard.is_none());

        // flush the display before starting to poll
        if let Err(WaylandError::Io(err)) = self.queue.flush() {
            if err.kind() != io::ErrorKind::WouldBlock {
                // in case of error, don't prepare a read, if the error is persistent, it'll
                // trigger in other wayland methods anyway
                log_error!("Error trying to flush the wayland display: {}", err);
                return Err(err.into());
            }
        }

        self.read_guard = self.queue.prepare_read();
        match self.read_guard {
            Some(_) => Ok(None),
            // If getting the guard failed, that means that there are
            // events in the queue, and so we need to handle the events instantly
            // rather than waiting on an event in polling. We tell calloop this
            // by returning Some here. Note that the readiness value is
            // never used, so we just need some marker
            None => Ok(Some((Readiness::EMPTY, self.fake_token.unwrap()))),
        }
    }

    fn before_handle_events(&mut self, events: calloop::EventIterator<'_>) {
        let guard = self.read_guard.take();
        if events.count() > 0 {
            // Read events from the socket if any are available
            if let Some(guard) = guard {
                if let Err(WaylandError::Io(err)) = guard.read() {
                    // If some other thread read events before us, concurrently, that's an expected
                    // case, so this error isn't an issue. Other error kinds do need to be returned,
                    // however
                    if err.kind() != io::ErrorKind::WouldBlock {
                        // before_handle_events doesn't allow returning errors
                        // For now, cache it in self until process_events is called
                        self.stored_error = Err(err);
                    }
                }
            }
        }
        // Otherwise, drop the guard if we have it, as we don't want to do any
        // reading when we didn't get any events
    }
}

impl<D> WaylandSource<D> {
    /// Loop over the callback until all pending messages have been dispatched.
    fn loop_callback_pending<F>(queue: &mut EventQueue<D>, callback: &mut F) -> io::Result<()>
    where
        F: FnMut((), &mut EventQueue<D>) -> Result<usize, DispatchError>,
    {
        // Loop on the callback until no pending events are left.
        loop {
            match callback((), queue) {
                // No more pending events.
                Ok(0) => break Ok(()),
                Ok(_) => continue,
                Err(DispatchError::Backend(WaylandError::Io(err))) => {
                    return Err(err);
                },
                Err(DispatchError::Backend(WaylandError::Protocol(err))) => {
                    log_error!("Protocol error received on display: {}", err);

                    break Err(Errno::PROTO.into());
                },
                Err(DispatchError::BadMessage { interface, sender_id, opcode }) => {
                    log_error!(
                        "Bad message on interface \"{}\": (sender_id: {}, opcode: {})",
                        interface,
                        sender_id,
                        opcode,
                    );

                    break Err(Errno::PROTO.into());
                },
            }
        }
    }
}
