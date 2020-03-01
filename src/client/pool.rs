use std::sync::{Arc, Weak};

use fnv::FnvHashMap;
use tokio::stream;
use tokio::sync::{broadcast, mpsc};

use crate::client::single::{connect_internal, ConnectionContext};
use crate::client::MessageSender;
use crate::event::Event;
use crate::stream::rate_limits::RateLimiter;
use crate::stream::{RespondWithErrors, SentClientMessage};
use crate::util::InternalSender;
use crate::EventChannelError;
use crate::{ClientMessage, Error, TwitchClientConfig};
use crate::{MessageResponse, MessageSendError};
use futures_core::Stream;
use tokio::sync::broadcast::RecvError;

/// Create a connection pool
pub async fn connect(
    cfg: &Arc<TwitchClientConfig>,
    pool_cfg: PoolConfig,
) -> Result<ConnectionPoolHandle, Error> {
    let (message_sender, mut message_receiver) =
        mpsc::channel::<SentClientMessage>(cfg.channel_buffer);
    let rate_limiter = Arc::new(RateLimiter::from(&cfg.rate_limiter));
    let (event_sender, event_receiver) = broadcast::channel(cfg.channel_buffer);

    let mut default_connections = vec![];
    for _ in 0..pool_cfg.init_connections {
        let conn = new_connection(&cfg, &rate_limiter, &event_sender).await?;
        default_connections.push(Arc::new(conn));
    }

    let pool = ConnectionPool {
        whisper_connection: default_connections[0].clone(),
        channel_connections_map: Default::default(),
        event_sender: event_sender.clone(),
        event_receiver,
        connections: default_connections,
    };

    {
        // capture variables for spawned task
        let cfg = cfg.clone();
        let event_sender = event_sender.clone();
        tokio::spawn(async move {
            use futures_util::stream::StreamExt;

            let mut pool = pool;
            while let Some(SentClientMessage {
                message: client_message,
                responder,
            }) = message_receiver.recv().await
            {
                match &client_message {
                    ClientMessage::PrivMsg { channel, .. } => {
                        if let Some(handle) = pool.get_channel_connection(channel) {
                            handle
                                .send(client_message)
                                .await
                                .respond_with_errors(responder);
                        } else {
                            responder
                                .send(Err(MessageSendError::ChannelNotJoined(client_message)))
                                .ok();
                        }
                    }
                    ClientMessage::Whisper { .. } => {
                        pool.whisper_connection
                            .send(client_message)
                            .await
                            .respond_with_errors(responder);
                    }
                    ClientMessage::Ping | ClientMessage::Pong => {
                        pool.whisper_connection
                            .send(client_message)
                            .await
                            .respond_with_errors(responder);
                    }
                    ClientMessage::Part(channel) => {
                        if let Some(handle) = pool.get_channel_connection(channel) {
                            handle
                                .send(client_message)
                                .await
                                .respond_with_errors(responder);
                        } else {
                            responder
                                .send(Err(MessageSendError::ChannelNotJoined(client_message)))
                                .ok();
                        }
                    }
                    ClientMessage::Join(channel) => {
                        // already joined this channel
                        if let Some(connection) = pool.get_channel_connection(channel) {
                            connection
                                .send(client_message)
                                .await
                                .respond_with_errors(responder);
                        } else {
                            // get connection with the lowest amount of joined channels
                            let handle = stream::iter(&pool.connections)
                                .filter_map(|handle| {
                                    let threshold = pool_cfg.threshold;
                                    async move {
                                        let count =
                                            handle.context.joined_channels.read().await.len();
                                        if count <= threshold as usize {
                                            Some((handle, count))
                                        } else {
                                            None
                                        }
                                    }
                                })
                                .collect::<Vec<_>>()
                                .await
                                .into_iter()
                                .min_by_key(|(_handle, joined_count)| *joined_count)
                                .map(|(handle, _)| handle);

                            if let Some(channel_handle) = handle {
                                debug!("Joining channel on existing connection.");
                                channel_handle
                                    .send(client_message)
                                    .await
                                    .respond_with_errors(responder);
                            } else {
                                debug!("Adding new connection to the pool.");
                                let conn_result =
                                    new_connection(&cfg, &rate_limiter, &event_sender)
                                        .await
                                        .map_err(|e| {
                                            MessageSendError::NewConnectionFailed(format!("{}", e))
                                        });
                                match conn_result {
                                    Ok(conn) => {
                                        let channel = channel.clone();
                                        conn.send(client_message)
                                            .await
                                            .respond_with_errors(responder);
                                        let arc = Arc::new(conn);
                                        let weak = Arc::downgrade(&arc);
                                        pool.connections.push(arc);
                                        pool.channel_connections_map.insert(channel.clone(), weak);
                                    }
                                    Err(_) => conn_result.respond_with_errors(responder),
                                }
                            }
                        }
                    }
                    ClientMessage::Nick(_) => {
                        responder
                            .send(Err(MessageSendError::UnsupportedMessage(
                                "NICK is sent automatically in managed connection pools.",
                            )))
                            .ok();
                    }
                    ClientMessage::Pass(_) => {
                        responder
                            .send(Err(MessageSendError::UnsupportedMessage(
                                "PASS is sent automatically in managed connection pools.",
                            )))
                            .ok();
                    }
                    ClientMessage::CapRequest(_) => {
                        responder
                            .send(Err(MessageSendError::UnsupportedMessage(
                                "CAP REQs are sent automatically in managed connection pools.",
                            )))
                            .ok();
                    }
                    ClientMessage::Close => {
                        for connection in &pool.connections {
                            if let Err(e) = connection.send(ClientMessage::Close).await {
                                responder.send(Err(e)).ok();
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    let pool_handle = ConnectionPoolHandle {
        event_sender,
        message_sender: MessageSender::from(message_sender),
    };

    Ok(pool_handle)
}

async fn new_connection(
    cfg: &Arc<TwitchClientConfig>,
    rate_limiter: &Arc<RateLimiter>,
    event_sender: &broadcast::Sender<Result<Event, Error>>,
) -> Result<ConnectionHandle, Error> {
    let (sender, context) = connect_internal(
        cfg,
        rate_limiter.clone(),
        InternalSender(event_sender.clone()),
    )
    .await?;

    Ok(ConnectionHandle { sender, context })
}

/// Connection pool settings
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Number of initially created connections
    pub init_connections: u32,
    /// Maximum number of connections
    pub connection_limit: u32,
    /// When all connections reach this number of joined channels, a new connection
    /// will be created
    pub threshold: u32,
}

struct ConnectionHandle {
    sender: MessageSender,
    context: Arc<ConnectionContext>,
}

impl ConnectionHandle {
    async fn send(&self, msg: ClientMessage) -> Result<MessageResponse, MessageSendError> {
        self.sender.clone().send(msg).await
    }
}

impl Drop for ConnectionHandle {
    fn drop(&mut self) {
        let mut sender = self.sender.clone();
        tokio::spawn(async move {
            sender.send(ClientMessage::Close).await.ok();
        });
    }
}

/// Handle to a connection pool
#[derive(Clone, Debug)]
pub struct ConnectionPoolHandle {
    event_sender: broadcast::Sender<Result<Event, Error>>,
    message_sender: MessageSender,
}

impl ConnectionPoolHandle {
    /// Subscribe to a receiver for messages
    pub fn subscribe_events(&self) -> impl Stream<Item = Result<Event, Error>> {
        use tokio::stream::StreamExt;
        self.event_sender.subscribe().map(|result| match result {
            Ok(event) => event,
            Err(recv_err) => Err(match recv_err {
                RecvError::Closed => EventChannelError::Closed,
                RecvError::Lagged(_) => EventChannelError::Overflow,
            }
            .into()),
        })
    }

    /// Get an owned sender for messages
    pub fn clone_sender(&self) -> MessageSender {
        self.message_sender.clone()
    }

    /// Get a reference to a message sender
    pub fn sender(&self) -> &MessageSender {
        &self.message_sender
    }
}

struct ConnectionPool {
    /// sender for the event broadcast channel
    event_sender: broadcast::Sender<Result<Event<String>, Error>>,
    /// receiver for the event broadcast channel
    event_receiver: broadcast::Receiver<Result<Event<String>, Error>>,
    /// default connections as specified in `init_connections`
    connections: Vec<Arc<ConnectionHandle>>,
    /// connection for whispers
    whisper_connection: Arc<ConnectionHandle>,
    /// weak connection handles for individual channels
    channel_connections_map: FnvHashMap<String, Weak<ConnectionHandle>>,
}

impl ConnectionPool {
    fn get_channel_connection(&self, channel: &str) -> Option<Arc<ConnectionHandle>> {
        self.channel_connections_map
            .get(channel)
            .and_then(|weak| weak.upgrade())
    }
}
