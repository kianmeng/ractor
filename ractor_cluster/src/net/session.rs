// Copyright (c) Sean Lawlor
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree.

//! TCP session actor which is managing the specific communication to a node

// TODO: RUSTLS + Tokio : https://github.com/tokio-rs/tls/blob/master/tokio-rustls/examples/server/src/main.rs

use std::convert::TryInto;
use std::net::SocketAddr;

use bytes::Bytes;
use prost::Message;
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use ractor::{SpawnErr, SupervisionEvent};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::ErrorKind;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::RactorMessage;

/// Helper method to read exactly `len` bytes from the stream into a pre-allocated buffer
/// of bytes
async fn read_n_bytes(stream: &mut OwnedReadHalf, len: usize) -> Result<Vec<u8>, tokio::io::Error> {
    let mut buf = vec![0u8; len];
    let mut c_len = 0;
    stream.readable().await?;
    while c_len < len {
        let n = stream.read(&mut buf[c_len..]).await?;
        if n == 0 {
            // EOF
            return Err(tokio::io::Error::new(
                tokio::io::ErrorKind::UnexpectedEof,
                "EOF",
            ));
        }
        c_len += n;
    }
    Ok(buf)
}

// ========================= Node Session actor ========================= //

/// Represents a bi-directional tcp connection along with send + receive operations
///
/// The [Session] actor supervises two child actors, [SessionReader] and [SessionWriter]. Should
/// either the reader or writer exit, they will terminate the entire session.
pub struct Session {
    pub(crate) handler: ActorRef<crate::node::NodeSession>,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) local_addr: SocketAddr,
}

impl Session {
    pub(crate) async fn spawn_linked(
        handler: ActorRef<crate::node::NodeSession>,
        stream: TcpStream,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
        supervisor: ActorCell,
    ) -> Result<ActorRef<Self>, SpawnErr> {
        match Actor::spawn_linked(
            None,
            Session {
                handler,
                peer_addr,
                local_addr,
            },
            stream,
            supervisor,
        )
        .await
        {
            Err(err) => {
                log::error!("Failed to spawn session writer actor: {}", err);
                Err(err)
            }
            Ok((a, _)) => {
                // return the actor handle
                Ok(a)
            }
        }
    }
}

/// The node connection messages
#[derive(RactorMessage)]
pub enum SessionMessage {
    /// Send a message over the channel
    Send(crate::protocol::NetworkMessage),

    /// An object was received on the channel
    ObjectAvailable(crate::protocol::NetworkMessage),
}

/// The node session's state
pub struct SessionState {
    writer: ActorRef<SessionWriter>,
    reader: ActorRef<SessionReader>,
}

#[async_trait::async_trait]
impl Actor for Session {
    type Msg = SessionMessage;
    type Arguments = TcpStream;
    type State = SessionState;

    async fn pre_start(
        &self,
        myself: ActorRef<Self>,
        stream: TcpStream,
    ) -> Result<Self::State, ActorProcessingErr> {
        let (read, write) = stream.into_split();
        // spawn writer + reader child actors
        let (writer, _) =
            Actor::spawn_linked(None, SessionWriter, write, myself.get_cell()).await?;
        let (reader, _) = Actor::spawn_linked(
            None,
            SessionReader {
                session: myself.clone(),
            },
            read,
            myself.get_cell(),
        )
        .await?;

        Ok(Self::State { writer, reader })
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        log::info!("TCP Session closed for {}", self.peer_addr);
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            Self::Msg::Send(msg) => {
                log::debug!(
                    "SEND: {} -> {} - '{:?}'",
                    self.local_addr,
                    self.peer_addr,
                    msg
                );
                let _ = state.writer.cast(SessionWriterMessage::WriteObject(msg));
            }
            Self::Msg::ObjectAvailable(msg) => {
                log::debug!(
                    "RECEIVE {} <- {} - '{:?}'",
                    self.local_addr,
                    self.peer_addr,
                    msg
                );
                let _ = self
                    .handler
                    .cast(crate::node::NodeSessionMessage::MessageReceived(msg));
            }
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self>,
        message: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // sockets open, they close, the world goes round... If a reader or writer exits for any reason, we'll start the shutdown procedure
        // which requires that all actors exit
        match message {
            SupervisionEvent::ActorPanicked(actor, panic_msg) => {
                if actor.get_id() == state.reader.get_id() {
                    log::error!("TCP Session's reader panicked with '{}'", panic_msg);
                } else if actor.get_id() == state.writer.get_id() {
                    log::error!("TCP Session's writer panicked with '{}'", panic_msg);
                } else {
                    log::error!("TCP Session received a child panic from an unknown child actor ({}) - '{}'", actor.get_id(), panic_msg);
                }
                myself.stop(Some("child_panic".to_string()));
            }
            SupervisionEvent::ActorTerminated(actor, _, exit_reason) => {
                if actor.get_id() == state.reader.get_id() {
                    log::debug!("TCP Session's reader exited");
                } else if actor.get_id() == state.writer.get_id() {
                    log::debug!("TCP Session's writer exited");
                } else {
                    log::warn!("TCP Session received a child exit from an unknown child actor ({}) - '{:?}'", actor.get_id(), exit_reason);
                }
                myself.stop(Some("child_terminate".to_string()));
            }
            _ => {
                // all ok
            }
        }
        Ok(())
    }
}

// ========================= Node Session writer ========================= //

struct SessionWriter;

struct SessionWriterState {
    writer: Option<OwnedWriteHalf>,
}

#[derive(crate::RactorMessage)]
enum SessionWriterMessage {
    /// Write an object over the wire
    WriteObject(crate::protocol::NetworkMessage),
}

#[async_trait::async_trait]
impl Actor for SessionWriter {
    type Msg = SessionWriterMessage;
    type Arguments = OwnedWriteHalf;
    type State = SessionWriterState;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self>,
        writer: OwnedWriteHalf,
    ) -> Result<Self::State, ActorProcessingErr> {
        // OK we've established connection, now we can process requests

        Ok(Self::State {
            writer: Some(writer),
        })
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // drop the channel to close it should we be exiting
        drop(state.writer.take());
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SessionWriterMessage::WriteObject(msg) if state.writer.is_some() => {
                if let Some(stream) = &mut state.writer {
                    stream.writable().await.unwrap();

                    let encoded_data = msg.encode_length_delimited_to_vec();
                    let length = encoded_data.len();
                    let length_bytes: [u8; 8] = (length as u64).to_be_bytes();
                    log::trace!("Writing 8 length bytes");
                    if let Err(write_err) = stream.write_all(&length_bytes).await {
                        log::warn!("Error writing to the stream '{}'", write_err);
                    } else {
                        log::trace!("Wrote length, writing payload (len={})", length);
                        // now send the object
                        if let Err(write_err) = stream.write_all(&encoded_data).await {
                            log::warn!("Error writing to the stream '{}'", write_err);
                            myself.stop(Some("channel_closed".to_string()));
                            return Ok(());
                        }
                        // flush the stream
                        stream.flush().await.unwrap();
                    }
                }
            }
            _ => {
                // no-op, wait for next send request
            }
        }
        Ok(())
    }
}

// ========================= Node Session reader ========================= //

struct SessionReader {
    session: ActorRef<Session>,
}

/// The node connection messages
pub enum SessionReaderMessage {
    /// Wait for an object from the stream
    WaitForObject,

    /// Read next object off the stream
    ReadObject(u64),
}

impl ractor::Message for SessionReaderMessage {}

struct SessionReaderState {
    reader: Option<OwnedReadHalf>,
}

#[async_trait::async_trait]
impl Actor for SessionReader {
    type Msg = SessionReaderMessage;
    type Arguments = OwnedReadHalf;
    type State = SessionReaderState;

    async fn pre_start(
        &self,
        myself: ActorRef<Self>,
        reader: OwnedReadHalf,
    ) -> Result<Self::State, ActorProcessingErr> {
        // start waiting for the first object on the network
        let _ = myself.cast(SessionReaderMessage::WaitForObject);
        Ok(Self::State {
            reader: Some(reader),
        })
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // drop the channel to close it should we be exiting
        drop(state.reader.take());
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            Self::Msg::WaitForObject if state.reader.is_some() => {
                if let Some(stream) = &mut state.reader {
                    match read_n_bytes(stream, 8).await {
                        Ok(buf) => {
                            let length = u64::from_be_bytes(buf.try_into().unwrap());
                            log::trace!("Payload length message ({}) received", length);
                            let _ = myself.cast(SessionReaderMessage::ReadObject(length));
                            return Ok(());
                        }
                        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
                            log::trace!("Error (EOF) on stream");
                            // EOF, close the stream by dropping the stream
                            drop(state.reader.take());
                            myself.stop(Some("channel_closed".to_string()));
                        }
                        Err(_other_err) => {
                            log::trace!("Error ({:?}) on stream", _other_err);
                            // some other TCP error, more handling necessary
                        }
                    }
                }

                let _ = myself.cast(SessionReaderMessage::WaitForObject);
            }
            Self::Msg::ReadObject(length) if state.reader.is_some() => {
                if let Some(stream) = &mut state.reader {
                    match read_n_bytes(stream, length as usize).await {
                        Ok(buf) => {
                            log::trace!("Payload of length({}) received", buf.len());
                            // NOTE: Our implementation writes 2 messages when sending something over the wire, the first
                            // is exactly 8 bytes which constitute the length of the payload message (u64 in big endian format),
                            // followed by the payload. This tells our TCP reader how much data to read off the wire

                            // [buf] here should contain the exact amount of data to decode an object properly.
                            let bytes = Bytes::from(buf);
                            match crate::protocol::NetworkMessage::decode_length_delimited(bytes) {
                                Ok(msg) => {
                                    // we decoded a message, pass it up the chain
                                    let _ = self.session.cast(SessionMessage::ObjectAvailable(msg));
                                }
                                Err(decode_err) => {
                                    log::error!(
                                        "Error decoding network message: '{}'. Discarding",
                                        decode_err
                                    );
                                }
                            }
                        }
                        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
                            // EOF, close the stream by dropping the stream
                            drop(state.reader.take());
                            myself.stop(Some("channel_closed".to_string()));
                            return Ok(());
                        }
                        Err(_other_err) => {
                            // TODO: some other TCP error, more handling necessary
                        }
                    }
                }

                // we've read the object, now wait for next object
                let _ = myself.cast(SessionReaderMessage::WaitForObject);
            }
            _ => {
                // no stream is available, keep looping until one is available
                let _ = myself.cast(SessionReaderMessage::WaitForObject);
            }
        }
        Ok(())
    }
}
