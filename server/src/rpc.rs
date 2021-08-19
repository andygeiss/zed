use super::{
    auth::{self, PeerExt as _},
    db::{ChannelId, UserId},
    AppState,
};
use anyhow::anyhow;
use async_std::task;
use async_tungstenite::{
    tungstenite::{protocol::Role, Error as WebSocketError, Message as WebSocketMessage},
    WebSocketStream,
};
use futures::{future::BoxFuture, FutureExt};
use postage::prelude::Stream as _;
use sha1::{Digest as _, Sha1};
use std::{
    any::{Any, TypeId},
    collections::{HashMap, HashSet},
    future::Future,
    mem,
    sync::Arc,
    time::Instant,
};
use surf::StatusCode;
use tide::log;
use tide::{
    http::headers::{HeaderName, CONNECTION, UPGRADE},
    Request, Response,
};
use time::OffsetDateTime;
use zrpc::{
    auth::random_token,
    proto::{self, AnyTypedEnvelope, EnvelopedMessage},
    ConnectionId, Peer, TypedEnvelope,
};

type ReplicaId = u16;

type MessageHandler = Box<
    dyn Send
        + Sync
        + Fn(Box<dyn AnyTypedEnvelope>, Arc<Server>) -> BoxFuture<'static, tide::Result<()>>,
>;

#[derive(Default)]
struct ServerBuilder {
    handlers: HashMap<TypeId, MessageHandler>,
}

impl ServerBuilder {
    pub fn on_message<F, Fut, M>(mut self, handler: F) -> Self
    where
        F: 'static + Send + Sync + Fn(Box<TypedEnvelope<M>>, Arc<Server>) -> Fut,
        Fut: 'static + Send + Future<Output = tide::Result<()>>,
        M: EnvelopedMessage,
    {
        let prev_handler = self.handlers.insert(
            TypeId::of::<M>(),
            Box::new(move |envelope, server| {
                let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                (handler)(envelope, server).boxed()
            }),
        );
        if prev_handler.is_some() {
            panic!("registered a handler for the same message twice");
        }

        self
    }

    pub fn build(self, rpc: &Arc<Peer>, state: &Arc<AppState>) -> Arc<Server> {
        Arc::new(Server {
            rpc: rpc.clone(),
            state: state.clone(),
            handlers: self.handlers,
        })
    }
}

pub struct Server {
    rpc: Arc<Peer>,
    state: Arc<AppState>,
    handlers: HashMap<TypeId, MessageHandler>,
}

impl Server {
    pub fn handle_connection<Conn>(
        self: &Arc<Self>,
        connection: Conn,
        addr: String,
        user_id: UserId,
    ) -> impl Future<Output = ()>
    where
        Conn: 'static
            + futures::Sink<WebSocketMessage, Error = WebSocketError>
            + futures::Stream<Item = Result<WebSocketMessage, WebSocketError>>
            + Send
            + Unpin,
    {
        let this = self.clone();
        async move {
            let (connection_id, handle_io, mut incoming_rx) =
                this.rpc.add_connection(connection).await;
            this.state
                .rpc
                .write()
                .await
                .add_connection(connection_id, user_id);

            let handle_io = handle_io.fuse();
            futures::pin_mut!(handle_io);
            loop {
                let next_message = incoming_rx.recv().fuse();
                futures::pin_mut!(next_message);
                futures::select_biased! {
                    message = next_message => {
                        if let Some(message) = message {
                            let start_time = Instant::now();
                            log::info!("RPC message received: {}", message.payload_type_name());
                            if let Some(handler) = this.handlers.get(&message.payload_type_id()) {
                                if let Err(err) = (handler)(message, this.clone()).await {
                                    log::error!("error handling message: {:?}", err);
                                } else {
                                    log::info!("RPC message handled. duration:{:?}", start_time.elapsed());
                                }
                            } else {
                                log::warn!("unhandled message: {}", message.payload_type_name());
                            }
                        } else {
                            log::info!("rpc connection closed {:?}", addr);
                            break;
                        }
                    }
                    handle_io = handle_io => {
                        if let Err(err) = handle_io {
                            log::error!("error handling rpc connection {:?} - {:?}", addr, err);
                        }
                        break;
                    }
                }
            }

            if let Err(err) = this.rpc.sign_out(connection_id, &this.state).await {
                log::error!("error signing out connection {:?} - {:?}", addr, err);
            }
        }
    }
}

#[derive(Default)]
pub struct State {
    connections: HashMap<ConnectionId, Connection>,
    pub worktrees: HashMap<u64, Worktree>,
    channels: HashMap<ChannelId, Channel>,
    next_worktree_id: u64,
}

struct Connection {
    user_id: UserId,
    worktrees: HashSet<u64>,
    channels: HashSet<ChannelId>,
}

pub struct Worktree {
    host_connection_id: Option<ConnectionId>,
    guest_connection_ids: HashMap<ConnectionId, ReplicaId>,
    active_replica_ids: HashSet<ReplicaId>,
    access_token: String,
    root_name: String,
    entries: HashMap<u64, proto::Entry>,
}

#[derive(Default)]
struct Channel {
    connection_ids: HashSet<ConnectionId>,
}

impl Worktree {
    pub fn connection_ids(&self) -> Vec<ConnectionId> {
        self.guest_connection_ids
            .keys()
            .copied()
            .chain(self.host_connection_id)
            .collect()
    }

    fn host_connection_id(&self) -> tide::Result<ConnectionId> {
        Ok(self
            .host_connection_id
            .ok_or_else(|| anyhow!("host disconnected from worktree"))?)
    }
}

impl Channel {
    fn connection_ids(&self) -> Vec<ConnectionId> {
        self.connection_ids.iter().copied().collect()
    }
}

impl State {
    // Add a new connection associated with a given user.
    pub fn add_connection(&mut self, connection_id: ConnectionId, user_id: UserId) {
        self.connections.insert(
            connection_id,
            Connection {
                user_id,
                worktrees: Default::default(),
                channels: Default::default(),
            },
        );
    }

    // Remove the given connection and its association with any worktrees.
    pub fn remove_connection(&mut self, connection_id: ConnectionId) -> Vec<u64> {
        let mut worktree_ids = Vec::new();
        if let Some(connection) = self.connections.remove(&connection_id) {
            for channel_id in connection.channels {
                if let Some(channel) = self.channels.get_mut(&channel_id) {
                    channel.connection_ids.remove(&connection_id);
                }
            }
            for worktree_id in connection.worktrees {
                if let Some(worktree) = self.worktrees.get_mut(&worktree_id) {
                    if worktree.host_connection_id == Some(connection_id) {
                        worktree_ids.push(worktree_id);
                    } else if let Some(replica_id) =
                        worktree.guest_connection_ids.remove(&connection_id)
                    {
                        worktree.active_replica_ids.remove(&replica_id);
                        worktree_ids.push(worktree_id);
                    }
                }
            }
        }
        worktree_ids
    }

    fn join_channel(&mut self, connection_id: ConnectionId, channel_id: ChannelId) {
        if let Some(connection) = self.connections.get_mut(&connection_id) {
            connection.channels.insert(channel_id);
            self.channels
                .entry(channel_id)
                .or_default()
                .connection_ids
                .insert(connection_id);
        }
    }

    // Add the given connection as a guest of the given worktree
    pub fn join_worktree(
        &mut self,
        connection_id: ConnectionId,
        worktree_id: u64,
        access_token: &str,
    ) -> Option<(ReplicaId, &Worktree)> {
        if let Some(worktree) = self.worktrees.get_mut(&worktree_id) {
            if access_token == worktree.access_token {
                if let Some(connection) = self.connections.get_mut(&connection_id) {
                    connection.worktrees.insert(worktree_id);
                }

                let mut replica_id = 1;
                while worktree.active_replica_ids.contains(&replica_id) {
                    replica_id += 1;
                }
                worktree.active_replica_ids.insert(replica_id);
                worktree
                    .guest_connection_ids
                    .insert(connection_id, replica_id);
                Some((replica_id, worktree))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn user_id_for_connection(&self, connection_id: ConnectionId) -> tide::Result<UserId> {
        Ok(self
            .connections
            .get(&connection_id)
            .ok_or_else(|| anyhow!("unknown connection"))?
            .user_id)
    }

    fn read_worktree(
        &self,
        worktree_id: u64,
        connection_id: ConnectionId,
    ) -> tide::Result<&Worktree> {
        let worktree = self
            .worktrees
            .get(&worktree_id)
            .ok_or_else(|| anyhow!("worktree not found"))?;

        if worktree.host_connection_id == Some(connection_id)
            || worktree.guest_connection_ids.contains_key(&connection_id)
        {
            Ok(worktree)
        } else {
            Err(anyhow!(
                "{} is not a member of worktree {}",
                connection_id,
                worktree_id
            ))?
        }
    }

    fn write_worktree(
        &mut self,
        worktree_id: u64,
        connection_id: ConnectionId,
    ) -> tide::Result<&mut Worktree> {
        let worktree = self
            .worktrees
            .get_mut(&worktree_id)
            .ok_or_else(|| anyhow!("worktree not found"))?;

        if worktree.host_connection_id == Some(connection_id)
            || worktree.guest_connection_ids.contains_key(&connection_id)
        {
            Ok(worktree)
        } else {
            Err(anyhow!(
                "{} is not a member of worktree {}",
                connection_id,
                worktree_id
            ))?
        }
    }
}

pub fn build_server(state: &Arc<AppState>, rpc: &Arc<Peer>) -> Arc<Server> {
    ServerBuilder::default()
        .on_message(share_worktree)
        .on_message(join_worktree)
        .on_message(update_worktree)
        .on_message(close_worktree)
        .on_message(open_buffer)
        .on_message(close_buffer)
        .on_message(update_buffer)
        .on_message(buffer_saved)
        .on_message(save_buffer)
        .on_message(get_channels)
        .on_message(get_users)
        .on_message(join_channel)
        .on_message(send_channel_message)
        .build(rpc, state)
}

pub fn add_routes(app: &mut tide::Server<Arc<AppState>>, rpc: &Arc<Peer>) {
    let server = build_server(app.state(), rpc);
    app.at("/rpc").with(auth::VerifyToken).get(move |request: Request<Arc<AppState>>| {
        let user_id = request.ext::<UserId>().copied();
        let server = server.clone();
        async move {
            const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

            let connection_upgrade = header_contains_ignore_case(&request, CONNECTION, "upgrade");
            let upgrade_to_websocket = header_contains_ignore_case(&request, UPGRADE, "websocket");
            let upgrade_requested = connection_upgrade && upgrade_to_websocket;

            if !upgrade_requested {
                return Ok(Response::new(StatusCode::UpgradeRequired));
            }

            let header = match request.header("Sec-Websocket-Key") {
                Some(h) => h.as_str(),
                None => return Err(anyhow!("expected sec-websocket-key"))?,
            };

            let mut response = Response::new(StatusCode::SwitchingProtocols);
            response.insert_header(UPGRADE, "websocket");
            response.insert_header(CONNECTION, "Upgrade");
            let hash = Sha1::new().chain(header).chain(WEBSOCKET_GUID).finalize();
            response.insert_header("Sec-Websocket-Accept", base64::encode(&hash[..]));
            response.insert_header("Sec-Websocket-Version", "13");

            let http_res: &mut tide::http::Response = response.as_mut();
            let upgrade_receiver = http_res.recv_upgrade().await;
            let addr = request.remote().unwrap_or("unknown").to_string();
            let user_id = user_id.ok_or_else(|| anyhow!("user_id is not present on request. ensure auth::VerifyToken middleware is present"))?;
            task::spawn(async move {
                if let Some(stream) = upgrade_receiver.await {
                    let stream = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
                    server.handle_connection(stream, addr, user_id).await;
                }
            });

            Ok(response)
        }
    });
}

async fn share_worktree(
    mut request: Box<TypedEnvelope<proto::ShareWorktree>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let mut state = server.state.rpc.write().await;
    let worktree_id = state.next_worktree_id;
    state.next_worktree_id += 1;
    let access_token = random_token();
    let worktree = request
        .payload
        .worktree
        .as_mut()
        .ok_or_else(|| anyhow!("missing worktree"))?;
    let entries = mem::take(&mut worktree.entries)
        .into_iter()
        .map(|entry| (entry.id, entry))
        .collect();
    state.worktrees.insert(
        worktree_id,
        Worktree {
            host_connection_id: Some(request.sender_id),
            guest_connection_ids: Default::default(),
            active_replica_ids: Default::default(),
            access_token: access_token.clone(),
            root_name: mem::take(&mut worktree.root_name),
            entries,
        },
    );

    server
        .rpc
        .respond(
            request.receipt(),
            proto::ShareWorktreeResponse {
                worktree_id,
                access_token,
            },
        )
        .await?;
    Ok(())
}

async fn join_worktree(
    request: Box<TypedEnvelope<proto::OpenWorktree>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let worktree_id = request.payload.worktree_id;
    let access_token = &request.payload.access_token;

    let mut state = server.state.rpc.write().await;
    if let Some((peer_replica_id, worktree)) =
        state.join_worktree(request.sender_id, worktree_id, access_token)
    {
        let mut peers = Vec::new();
        if let Some(host_connection_id) = worktree.host_connection_id {
            peers.push(proto::Peer {
                peer_id: host_connection_id.0,
                replica_id: 0,
            });
        }
        for (peer_conn_id, peer_replica_id) in &worktree.guest_connection_ids {
            if *peer_conn_id != request.sender_id {
                peers.push(proto::Peer {
                    peer_id: peer_conn_id.0,
                    replica_id: *peer_replica_id as u32,
                });
            }
        }

        broadcast(request.sender_id, worktree.connection_ids(), |conn_id| {
            server.rpc.send(
                conn_id,
                proto::AddPeer {
                    worktree_id,
                    peer: Some(proto::Peer {
                        peer_id: request.sender_id.0,
                        replica_id: peer_replica_id as u32,
                    }),
                },
            )
        })
        .await?;
        server
            .rpc
            .respond(
                request.receipt(),
                proto::OpenWorktreeResponse {
                    worktree_id,
                    worktree: Some(proto::Worktree {
                        root_name: worktree.root_name.clone(),
                        entries: worktree.entries.values().cloned().collect(),
                    }),
                    replica_id: peer_replica_id as u32,
                    peers,
                },
            )
            .await?;
    } else {
        server
            .rpc
            .respond(
                request.receipt(),
                proto::OpenWorktreeResponse {
                    worktree_id,
                    worktree: None,
                    replica_id: 0,
                    peers: Vec::new(),
                },
            )
            .await?;
    }

    Ok(())
}

async fn update_worktree(
    request: Box<TypedEnvelope<proto::UpdateWorktree>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    {
        let mut state = server.state.rpc.write().await;
        let worktree = state.write_worktree(request.payload.worktree_id, request.sender_id)?;
        for entry_id in &request.payload.removed_entries {
            worktree.entries.remove(&entry_id);
        }

        for entry in &request.payload.updated_entries {
            worktree.entries.insert(entry.id, entry.clone());
        }
    }

    broadcast_in_worktree(request.payload.worktree_id, &request, &server).await?;
    Ok(())
}

async fn close_worktree(
    request: Box<TypedEnvelope<proto::CloseWorktree>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let connection_ids;
    {
        let mut state = server.state.rpc.write().await;
        let worktree = state.write_worktree(request.payload.worktree_id, request.sender_id)?;
        connection_ids = worktree.connection_ids();
        if worktree.host_connection_id == Some(request.sender_id) {
            worktree.host_connection_id = None;
        } else if let Some(replica_id) = worktree.guest_connection_ids.remove(&request.sender_id) {
            worktree.active_replica_ids.remove(&replica_id);
        }
    }

    broadcast(request.sender_id, connection_ids, |conn_id| {
        server.rpc.send(
            conn_id,
            proto::RemovePeer {
                worktree_id: request.payload.worktree_id,
                peer_id: request.sender_id.0,
            },
        )
    })
    .await?;

    Ok(())
}

async fn open_buffer(
    request: Box<TypedEnvelope<proto::OpenBuffer>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let receipt = request.receipt();
    let worktree_id = request.payload.worktree_id;
    let host_connection_id = server
        .state
        .rpc
        .read()
        .await
        .read_worktree(worktree_id, request.sender_id)?
        .host_connection_id()?;

    let response = server
        .rpc
        .forward_request(request.sender_id, host_connection_id, request.payload)
        .await?;
    server.rpc.respond(receipt, response).await?;
    Ok(())
}

async fn close_buffer(
    request: Box<TypedEnvelope<proto::CloseBuffer>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let host_connection_id = server
        .state
        .rpc
        .read()
        .await
        .read_worktree(request.payload.worktree_id, request.sender_id)?
        .host_connection_id()?;

    server
        .rpc
        .forward_send(request.sender_id, host_connection_id, request.payload)
        .await?;

    Ok(())
}

async fn save_buffer(
    request: Box<TypedEnvelope<proto::SaveBuffer>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let host;
    let guests;
    {
        let state = server.state.rpc.read().await;
        let worktree = state.read_worktree(request.payload.worktree_id, request.sender_id)?;
        host = worktree.host_connection_id()?;
        guests = worktree
            .guest_connection_ids
            .keys()
            .copied()
            .collect::<Vec<_>>();
    }

    let sender = request.sender_id;
    let receipt = request.receipt();
    let response = server
        .rpc
        .forward_request(sender, host, request.payload.clone())
        .await?;

    broadcast(host, guests, |conn_id| {
        let response = response.clone();
        let server = &server;
        async move {
            if conn_id == sender {
                server.rpc.respond(receipt, response).await
            } else {
                server.rpc.forward_send(host, conn_id, response).await
            }
        }
    })
    .await?;

    Ok(())
}

async fn update_buffer(
    request: Box<TypedEnvelope<proto::UpdateBuffer>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    broadcast_in_worktree(request.payload.worktree_id, &request, &server).await
}

async fn buffer_saved(
    request: Box<TypedEnvelope<proto::BufferSaved>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    broadcast_in_worktree(request.payload.worktree_id, &request, &server).await
}

async fn get_channels(
    request: Box<TypedEnvelope<proto::GetChannels>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let user_id = server
        .state
        .rpc
        .read()
        .await
        .user_id_for_connection(request.sender_id)?;
    let channels = server.state.db.get_channels_for_user(user_id).await?;
    server
        .rpc
        .respond(
            request.receipt(),
            proto::GetChannelsResponse {
                channels: channels
                    .into_iter()
                    .map(|chan| proto::Channel {
                        id: chan.id.to_proto(),
                        name: chan.name,
                    })
                    .collect(),
            },
        )
        .await?;
    Ok(())
}

async fn get_users(
    request: Box<TypedEnvelope<proto::GetUsers>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let user_id = server
        .state
        .rpc
        .read()
        .await
        .user_id_for_connection(request.sender_id)?;
    let receipt = request.receipt();
    let user_ids = request.payload.user_ids.into_iter().map(UserId::from_proto);
    let users = server
        .state
        .db
        .get_users_by_ids(user_id, user_ids)
        .await?
        .into_iter()
        .map(|user| proto::User {
            id: user.id.to_proto(),
            github_login: user.github_login,
            avatar_url: String::new(),
        })
        .collect();
    server
        .rpc
        .respond(receipt, proto::GetUsersResponse { users })
        .await?;
    Ok(())
}

async fn join_channel(
    request: Box<TypedEnvelope<proto::JoinChannel>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let user_id = server
        .state
        .rpc
        .read()
        .await
        .user_id_for_connection(request.sender_id)?;
    let channel_id = ChannelId::from_proto(request.payload.channel_id);
    if !server
        .state
        .db
        .can_user_access_channel(user_id, channel_id)
        .await?
    {
        Err(anyhow!("access denied"))?;
    }

    server
        .state
        .rpc
        .write()
        .await
        .join_channel(request.sender_id, channel_id);
    let messages = server
        .state
        .db
        .get_recent_channel_messages(channel_id, 50)
        .await?
        .into_iter()
        .map(|msg| proto::ChannelMessage {
            id: msg.id.to_proto(),
            body: msg.body,
            timestamp: msg.sent_at.unix_timestamp() as u64,
            sender_id: msg.sender_id.to_proto(),
        })
        .collect();
    server
        .rpc
        .respond(request.receipt(), proto::JoinChannelResponse { messages })
        .await?;
    Ok(())
}

async fn send_channel_message(
    request: Box<TypedEnvelope<proto::SendChannelMessage>>,
    server: Arc<Server>,
) -> tide::Result<()> {
    let channel_id = ChannelId::from_proto(request.payload.channel_id);
    let user_id;
    let connection_ids;
    {
        let state = server.state.rpc.read().await;
        user_id = state.user_id_for_connection(request.sender_id)?;
        if let Some(channel) = state.channels.get(&channel_id) {
            connection_ids = channel.connection_ids();
        } else {
            return Ok(());
        }
    }

    let timestamp = OffsetDateTime::now_utc();
    let message_id = server
        .state
        .db
        .create_channel_message(channel_id, user_id, &request.payload.body, timestamp)
        .await?;
    let message = proto::ChannelMessageSent {
        channel_id: channel_id.to_proto(),
        message: Some(proto::ChannelMessage {
            sender_id: user_id.to_proto(),
            id: message_id.to_proto(),
            body: request.payload.body,
            timestamp: timestamp.unix_timestamp() as u64,
        }),
    };
    broadcast(request.sender_id, connection_ids, |conn_id| {
        server.rpc.send(conn_id, message.clone())
    })
    .await?;

    Ok(())
}

async fn broadcast_in_worktree<T: proto::EnvelopedMessage>(
    worktree_id: u64,
    request: &TypedEnvelope<T>,
    server: &Arc<Server>,
) -> tide::Result<()> {
    let connection_ids = server
        .state
        .rpc
        .read()
        .await
        .read_worktree(worktree_id, request.sender_id)?
        .connection_ids();

    broadcast(request.sender_id, connection_ids, |conn_id| {
        server
            .rpc
            .forward_send(request.sender_id, conn_id, request.payload.clone())
    })
    .await?;

    Ok(())
}

pub async fn broadcast<F, T>(
    sender_id: ConnectionId,
    receiver_ids: Vec<ConnectionId>,
    mut f: F,
) -> anyhow::Result<()>
where
    F: FnMut(ConnectionId) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let futures = receiver_ids
        .into_iter()
        .filter(|id| *id != sender_id)
        .map(|id| f(id));
    futures::future::try_join_all(futures).await?;
    Ok(())
}

fn header_contains_ignore_case<T>(
    request: &tide::Request<T>,
    header_name: HeaderName,
    value: &str,
) -> bool {
    request
        .header(header_name)
        .map(|h| {
            h.as_str()
                .split(',')
                .any(|s| s.trim().eq_ignore_ascii_case(value.trim()))
        })
        .unwrap_or(false)
}
