use crate::ensure_newline;
use crate::subscription::BoxedSubscription;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::Notify;
use tokio_util::codec::{BytesCodec, Framed};
use tracing::{debug, warn};

/// A TCP console to process both strongly typed and free form messages.
/// Free form messages are sent to all known subscriptions in random order until the _first_ success.
///
/// This console only allows message from localhost.
pub struct Console<Services, A> {
    inner: Arc<Inner<Services>>,
    bind_address: Option<A>,
    stop: Arc<Notify>,
}

struct Inner<Services> {
    subscriptions: HashMap<Services, BoxedSubscription>,
    welcome: String,
    accept_only_localhost: bool,
}

impl<Services, A> Console<Services, A> {
    pub(crate) fn new(
        subscriptions: HashMap<Services, BoxedSubscription>,
        bind_address: A,
        welcome: String,
        accept_only_localhost: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                subscriptions,
                welcome,
                accept_only_localhost,
            }),
            bind_address: Some(bind_address),
            stop: Arc::new(Notify::new()),
        }
    }
}
impl<Services, A> Console<Services, A>
where
    Services: DeserializeOwned + Eq + Hash + Debug + Send + Sync + 'static,
    A: ToSocketAddrs + 'static,
{
    /// Spawn the console by opening a TCP socket at the specified address.
    pub async fn spawn(&mut self) -> Result<(), Error> {
        let Some(bind_address) = self.bind_address.take() else {
            warn!("Console has already started");
            return Err(Error::AlreadyStarted);
        };

        let listener = TcpListener::bind(bind_address).await?;
        let inner = self.inner.clone();
        let stop = self.stop.clone();

        tokio::spawn(async move {
            debug!(
                "Listening on {:?}",
                listener.local_addr().expect("Local address must be known")
            );

            loop {
                // Keep accepting console sessions,
                // verify that they satisfy the requirements,
                // if so, spawn a task to handle the session.

                let stream = tokio::select! {
                    _ = stop.notified() => {
                        debug!("Stopping console");
                        return;
                    }
                    Ok((stream, _)) = listener.accept() => {
                        stream
                    }
                };

                debug!("New console connection.");

                let Ok(addr) = stream.peer_addr() else {
                    warn!("Could not get peer address. Closing the connection.");
                    continue;
                };
                if inner.accept_only_localhost && !addr.ip().is_loopback() {
                    warn!("Only connection from the localhost are allowed. Connected peer address {addr}. Closing the connection.");
                    continue;
                }

                tokio::spawn(Self::handle_console_session(
                    stream,
                    inner.clone(),
                    stop.clone(),
                ));
            }
        });

        Ok(())
    }

    /// Stop the console and break all the current connections.
    pub fn stop(&self) {
        self.stop.notify_waiters();
    }

    /// Internal function handling a remote console session.
    async fn handle_console_session(
        stream: TcpStream,
        inner: Arc<Inner<Services>>,
        stop: Arc<Notify>,
    ) {
        let Ok(addr) = stream.peer_addr() else {
            warn!("Could not get peer address. Closing the session.");
            return;
        };

        debug!("Connected to {addr}");

        let mut bytes_stream = Framed::new(stream, BytesCodec::new());

        debug!("Welcoming {addr}");
        let bytes: Bytes = inner.welcome.as_bytes().to_vec().into();
        let _ = bytes_stream.send(bytes).await;
        debug!("Finished welcoming {addr}");

        loop {
            let bytes = tokio::select! {
                _ = stop.notified() => {
                    debug!("Stopping session for {addr}");
                    return;
                }
                result = bytes_stream.next() => match result {
                    Some(Ok(bytes)) => {
                        bytes.freeze()
                    }
                    Some(Err(err)) => {
                        warn!("Error while receiving bytes: {err}. Received bytes will not be processed");
                        continue;
                    }
                    None => {
                        // Connection closed.
                        debug!("Connection closed by {addr}");
                        return;
                    }
                }
            };

            match bcs::from_bytes::<Message<Services>>(bytes.as_ref()) {
                Ok(Message { service_id, bytes }) => {
                    // Message is strongly typed.

                    debug!("Received message for {service_id:?}");

                    if let Some(subscription) = inner.subscriptions.get(&service_id) {
                        debug!("Found subscription for service {service_id:?}");

                        match subscription.handle(bytes).await {
                            Ok(None) => {}
                            Ok(Some(bytes)) => {
                                let _ = bytes_stream.send(bytes).await;
                            }
                            Err(err) => warn!("Error handling message: {err}"),
                        }
                    } else {
                        warn!("No subscription found for service {service_id:?}. Ignoring the message.");
                    }
                }
                Err(_err) => {
                    // Message is not strongly typed and probably came from netcat or a similar client.
                    // Try all subscriptions to make sense of it until the FIRST success.

                    let text = String::from_utf8_lossy(bytes.as_ref()).trim().to_string();
                    debug!("Received message is not typed. Treating it as text: {text}");

                    for (service_id, subscription) in &inner.subscriptions {
                        debug!("[{service_id:?}] request to process text message: `{text}`");

                        match subscription.weak_handle(&text).await {
                            Ok(None) => {
                                continue;
                            }
                            Ok(Some(message)) => {
                                debug!("[{service_id:?}] Message processed");
                                let vec: Bytes = ensure_newline(message).as_bytes().to_vec().into();
                                let _ = bytes_stream.send(vec).await;
                                break;
                            }
                            Err(err) => {
                                warn!("Service {service_id:?} failed to handle message: {err}");
                                continue;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A wrapper struct to pass strongly-typed messages on [Console].
#[derive(Serialize, Deserialize)]
pub(crate) struct Message<Services> {
    service_id: Services,
    bytes: Bytes,
}

impl<Services> Message<Services> {
    /// Creates a new [Message] with any serializable payload.
    pub(crate) fn new(service_id: Services, message: &impl Serialize) -> Result<Self, Error> {
        Ok(Self {
            service_id,
            bytes: Bytes::from(bcs::to_bytes(message)?),
        })
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Subscription cannot be registered: service id `{0}` is already in use")]
    ServiceIdUsed(String),
    #[error("Console bind address is not specified")]
    NoBindAddress,
    #[error("Console had already started")]
    AlreadyStarted,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serde error: {0}")]
    Serde(#[from] bcs::Error),
}
