// Copyright (c) 2017 The Rouille developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::error::Error;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use mio::{Events, Poll, Ready, PollOpt};
use mio::tcp::{TcpListener, TcpStream};
use num_cpus;
use slab::Slab;

use socket_handler::TaskPool;
use socket_handler::SocketHandler;
use socket_handler::Update as SocketHandlerUpdate;

use Request;
use Response;

/// A listening server.
///
/// This struct is the more manual server creation API of rouille and can be used as an alternative
/// to the `start_server` function.
///
/// The `start_server` function is just a shortcut for `Server::new` followed with `run`. See the
/// documentation of the `start_server` function for more details about the handler.
///
/// # Example
///
/// ```no_run
/// use rouille::Server;
/// use rouille::Response;
///
/// let server = Server::new("localhost:0", |request| {
///     Response::text("hello world")
/// }).unwrap();
/// println!("Listening on {:?}", server.server_addr());
/// server.run();
/// ```
pub struct Server<F> {
    inner: Arc<ThreadsShare<F>>,
    local_events: Mutex<Events>,
}

// Data shared between threads.
struct ThreadsShare<F> {
    // The main poll event.
    poll: Poll,
    // Storage for all the objects registered towards the `Poll`.
    sockets: Mutex<Slab<Socket>>,
    // The function that handles requests.
    handler: AssertUnwindSafe<F>,
    // Pool used to dispatch tasks.
    task_pool: TaskPool,
}

enum Socket {
    Listener(TcpListener),
    Stream {
        stream: TcpStream,
        handler: SocketHandler,
        update: SocketHandlerUpdate,
    },
}

impl<F> Server<F> where F: Send + Sync + 'static + Fn(&Request) -> Response {
    /// Builds a new `Server` object.
    ///
    /// After this function returns, the HTTP server is listening.
    ///
    /// Returns an error if there was an error while creating the listening socket, for example if
    /// the port is already in use.
    pub fn new<A>(addr: A, handler: F) -> Result<Server<F>, Box<Error + Send + Sync>>
        where A: ToSocketAddrs,
              F: Fn(&Request) -> Response + Send + 'static
    {
        let server = Server::init(handler)?;

        for addr in addr.to_socket_addrs()? {
            server.add_listener(&addr)?;
        }

        Ok(server)
    }

    // Builds a new `Server` but without any listener.
    fn init(handler: F) -> Result<Server<F>, Box<Error + Send + Sync>>
        where F: Fn(&Request) -> Response + Send + 'static
    {
        let share = Arc::new(ThreadsShare {
            poll: Poll::new()?,
            sockets: Mutex::new(Slab::new()),
            handler: AssertUnwindSafe(handler),       // TODO: using AssertUnwindSafe here is wrong, but unwind safety has some usability problems in Rust in general
            task_pool: TaskPool::new(),
        });

        for _ in 0 .. num_cpus::get() - 1 {
            let share = share.clone();
            thread::spawn(move || {
                // Each thread has its own local MIO events.
                let mut events = Events::with_capacity(128);

                // TODO: The docs say that two events can be generated, one for read and one for
                //       write, presumably even if we pass one_shot(). Is this code ready for this
                //       situation?

                loop {
                    one_poll(&share, &mut events);
                }
            });
        }

        Ok(Server {
            inner: share,
            local_events: Mutex::new(Events::with_capacity(128)),
        })
    }

    // Adds a new listening addr to the server.
    fn add_listener(&self, addr: &SocketAddr) -> Result<(), Box<Error + Send + Sync>> {
        let listener = TcpListener::bind(addr)?;

        let mut slab = self.inner.sockets.lock().unwrap();
        let entry = slab.vacant_entry();

        self.inner.poll.register(&listener, entry.key().into(),
                                 Ready::readable(), PollOpt::edge() | PollOpt::oneshot())?;
    
        entry.insert(Socket::Listener(listener));
        
        Ok(())
    }

    /// Returns the address of the listening socket.
    #[inline]
    pub fn server_addr(&self) -> SocketAddr {
        unimplemented!()        // FIXME: restore?
        //self.server.server_addr()
    }

    /// Runs the server forever, or until the listening socket is somehow force-closed by the
    /// operating system.
    #[inline]
    pub fn run(self) {
        let mut local_events = self.local_events.lock().unwrap();
        loop {
            one_poll(&self.inner, &mut local_events);
        }
    }

    /// Processes all the client requests waiting to be processed, then returns.
    ///
    /// This function executes very quickly, as each client requests that needs to be processed
    /// is processed in a separate thread.
    #[inline]
    pub fn poll(&self) {
        let mut local_events = self.local_events.lock().unwrap();
        one_poll(&self.inner, &mut local_events);
    }
}

fn one_poll<F>(share: &Arc<ThreadsShare<F>>, events: &mut Events)
    where F: Fn(&Request) -> Response + Send + Sync + 'static
{
    share.poll.poll(events, None).expect("Error with the system selector");

    for event in events.iter() {
        // We handle reading before writing, as handling reading can generate data to write.

        if event.readiness().is_readable() {
            let socket = {
                let mut slab = share.sockets.lock().unwrap();
                if !slab.contains(event.token().into()) {
                    continue;
                }
                slab.remove(event.token().into())
            };

            handle_read(share, socket);
        }

        if event.readiness().is_writable() {
            let socket = {
                let mut slab = share.sockets.lock().unwrap();
                if !slab.contains(event.token().into()) {
                    continue;
                }
                slab.remove(event.token().into())
            };

            handle_write(share, socket);
        }
    }
}

fn handle_read<F>(share: &Arc<ThreadsShare<F>>, socket: Socket)
    where F: Fn(&Request) -> Response + Send + Sync + 'static
{
    match socket {
        Socket::Listener(listener) => {
            // Call `accept` repeatidely and register the newly-created sockets,
            // until `WouldBlock` is returned.
            loop {
                match listener.accept() {
                    Ok((stream, client_addr)) => {
                        let mut slab = share.sockets.lock().unwrap();
                        let entry = slab.vacant_entry();
                        share.poll.register(&stream, entry.key().into(), Ready::readable(),
                                                PollOpt::edge() | PollOpt::oneshot())
                            .expect("Error while registering TCP stream");
                        let share = share.clone();
                        entry.insert(Socket::Stream {
                            stream: stream,
                            handler: SocketHandler::new(client_addr, share.task_pool.clone(),
                                                        move |rq| (share.handler)(&rq)),
                            update: SocketHandlerUpdate::empty(),
                        });
                    },
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {        
                        // Handle errors with the listener by returning without re-registering it.
                        // This drops the listener.
                        return;
                    },
                };
            };

            // Re-register the listener for the next time.
            let mut slab = share.sockets.lock().unwrap();
            let entry = slab.vacant_entry();
            share.poll.reregister(&listener, entry.key().into(), Ready::readable(),
                                    PollOpt::edge() | PollOpt::oneshot())
                .expect("Error while reregistering TCP listener");
            entry.insert(Socket::Listener(listener));
        },

        Socket::Stream { mut stream, mut handler, mut update } => {
            // Read into `update.pending_read_buffer` until `WouldBlock` is returned.
            loop {
                let old_pr_len = update.pending_read_buffer.len();
                update.pending_read_buffer.resize(old_pr_len + 256, 0);

                match stream.read(&mut update.pending_read_buffer[old_pr_len..]) {
                    Ok(0) => {
                        update.pending_read_buffer.resize(old_pr_len, 0);
                        break;
                    },
                    Ok(n) => {
                        update.pending_read_buffer.resize(old_pr_len + n, 0);
                    },
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                        update.pending_read_buffer.resize(old_pr_len, 0);
                    },
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        update.pending_read_buffer.resize(old_pr_len, 0);
                        break;
                    },
                    Err(_) => {
                        // Handle errors with the stream by returning without re-registering it.
                        // This drops the stream.
                        return;
                    },
                };
            }

            // Dispatch to handler.
            handler.update(&mut update);
            update.new_data_start = update.pending_read_buffer.len();

            // Re-register stream for next time.
            let mut ready = Ready::empty();
            if update.accepts_read {
                ready = ready | Ready::readable();
            }
            if !update.pending_write_buffer.is_empty() {
                ready = ready | Ready::writable();
            }

            let mut slab = share.sockets.lock().unwrap();
            let entry = slab.vacant_entry();

            let mut insert_entry = false;

            if let Some(registration) = update.registration.take() {
                share.poll.register(&*registration, entry.key().into(),
                                    Ready::readable() | Ready::writable(),
                                    PollOpt::edge() | PollOpt::oneshot())
                    .expect("Error while registering registration");
                insert_entry = true;
            }

            if !ready.is_empty() {
                share.poll.reregister(&stream, entry.key().into(), ready,
                                        PollOpt::edge() | PollOpt::oneshot())
                    .expect("Error while reregistering TCP stream");
                insert_entry = true;
            }

            if insert_entry {
                entry.insert(Socket::Stream { stream, handler, update });
            }
        },
    }
}

fn handle_write<F>(share: &ThreadsShare<F>, socket: Socket) {
    // Write events can't happen for listeners.
    let (mut stream, handler, mut update) = match socket {
        Socket::Listener(_) => unreachable!(),
        Socket::Stream { stream, handler, update } => (stream, handler, update),
    };

    // Write from `update.pending_write_buffer` to `stream`.
    while !update.pending_write_buffer.is_empty() {
        match stream.write(&update.pending_write_buffer) {
            Ok(0) => {
                let _ = stream.flush();
                break;
            },
            Ok(n) => {
                // TODO: more efficient
                update.pending_write_buffer = update.pending_write_buffer[n..].to_owned();
                let _ = stream.flush();
            },
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {},
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                let _ = stream.flush();
                break;
            },
            Err(_) => {
                // Handle errors with the stream by returning without re-registering it. This
                // drops the stream.
                return;
            },
        };
    };

    // Re-register the stream for the next event.
    let mut ready = Ready::empty();
    if update.accepts_read {
        ready = ready | Ready::readable();
    }
    if !update.pending_write_buffer.is_empty() {
        ready = ready | Ready::writable();
    }
    if !ready.is_empty() {
        let mut slab = share.sockets.lock().unwrap();
        let entry = slab.vacant_entry();
        share.poll.reregister(&stream, entry.key().into(), ready,
                              PollOpt::edge() | PollOpt::oneshot())
            .expect("Error while reregistering TCP stream");
        entry.insert(Socket::Stream { stream, handler, update });
    }
}