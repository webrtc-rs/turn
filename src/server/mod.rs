#[cfg(test)]
mod server_test;

pub mod config;
pub mod request;

use crate::allocation::allocation_manager::*;
use crate::auth::AuthHandler;
use crate::error::*;
use crate::proto::lifetime::DEFAULT_LIFETIME;
use config::*;
use request::*;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use tokio::sync::{watch, Mutex};
use tokio::time::{Duration, Instant};
use util::Conn;

const INBOUND_MTU: usize = 1500;

/// The protocol to communicate between the [`Server`]'s public methods
/// and the threads spawned in the [`read_loop`] method.
enum Command {
    /// Command to delete [`crate::allocation::Allocation`] by provided
    /// `username`.
    DeleteAllocation(String),
}

/// Server is an instance of the TURN Server
pub struct Server {
    auth_handler: Arc<dyn AuthHandler + Send + Sync>,
    realm: String,
    channel_bind_timeout: Duration,
    pub(crate) nonces: Arc<Mutex<HashMap<String, Instant>>>,
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    commanders: HashMap<SocketAddr, Mutex<Sender<Command>>>,
}

impl Server {
    /// creates the TURN server
    pub async fn new(config: ServerConfig) -> Result<Self> {
        config.validate()?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let mut s = Server {
            auth_handler: config.auth_handler,
            realm: config.realm,
            channel_bind_timeout: config.channel_bind_timeout,
            nonces: Arc::new(Mutex::new(HashMap::new())),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            commanders: HashMap::new(),
        };

        if s.channel_bind_timeout == Duration::from_secs(0) {
            s.channel_bind_timeout = DEFAULT_LIFETIME;
        }

        for p in config.conn_configs.into_iter() {
            let nonces = Arc::clone(&s.nonces);
            let auth_handler = Arc::clone(&s.auth_handler);
            let realm = s.realm.clone();
            let channel_bind_timeout = s.channel_bind_timeout;
            let shutdown_rx = shutdown_rx.clone();
            let conn = p.conn;
            let allocation_manager = Arc::new(Manager::new(ManagerConfig {
                relay_addr_generator: p.relay_addr_generator,
            }));

            let (commander_tx, commander_rx) = mpsc::channel::<Command>();
            s.commanders
                .insert(conn.local_addr().await.unwrap(), Mutex::new(commander_tx));

            tokio::spawn({
                let allocation_manager = Arc::clone(&allocation_manager);

                async move {
                    Server::read_loop(
                        conn,
                        allocation_manager,
                        nonces,
                        auth_handler,
                        realm,
                        channel_bind_timeout,
                        shutdown_rx,
                        commander_rx,
                    )
                    .await;
                }
            });
        }

        Ok(s)
    }

    async fn read_loop(
        conn: Arc<dyn Conn + Send + Sync>,
        allocation_manager: Arc<Manager>,
        nonces: Arc<Mutex<HashMap<String, Instant>>>,
        auth_handler: Arc<dyn AuthHandler + Send + Sync>,
        realm: String,
        channel_bind_timeout: Duration,
        mut shutdown_rx: watch::Receiver<bool>,
        commander_rx: Receiver<Command>,
    ) {
        let mut buf = vec![0u8; INBOUND_MTU];

        loop {
            let (n, addr) = tokio::select! {
                v = conn.recv_from(&mut buf) => {
                    match v {
                        Ok(v) => v,
                        Err(err) => {
                            log::debug!("exit read loop on error: {}", err);
                            break;
                        }
                    }
                },
                did_change = shutdown_rx.changed() => {
                    if did_change.is_err() || *shutdown_rx.borrow() {
                        // if did_change.is_err, sender was dropped, or if
                        // bool is set to true, that means we're shutting down.
                        break
                    } else {
                        continue;
                    }
                }
            };

            'commander: loop {
                let command = commander_rx.try_recv();

                match command {
                    Ok(command) => match command {
                        Command::DeleteAllocation(name) => {
                            allocation_manager.delete_allocation_by_username(name).await;
                        }
                    },
                    Err(_) => break 'commander,
                }
            }

            let mut r = Request {
                conn: Arc::clone(&conn),
                src_addr: addr,
                buff: buf[..n].to_vec(),
                allocation_manager: Arc::clone(&allocation_manager),
                nonces: Arc::clone(&nonces),
                auth_handler: Arc::clone(&auth_handler),
                realm: realm.clone(),
                channel_bind_timeout,
            };

            if let Err(err) = r.handle_request().await {
                log::error!("error when handling datagram: {}", err);
            }
        }

        let _ = allocation_manager.close().await;
        let _ = conn.close().await;
    }

    /// Deletes the [`crate::allocation::Allocation`] by provided [`Conn`]
    /// address and `username`.
    pub async fn delete_allocation(&self, addr: SocketAddr, username: String) {
        let commander = self.commanders.get(&addr).unwrap().lock().await;
        commander.send(Command::DeleteAllocation(username)).unwrap();
    }

    /// Close stops the TURN Server. It cleans up any associated state and closes all connections it is managing
    pub async fn close(&self) -> Result<()> {
        let mut shutdown_tx = self.shutdown_tx.lock().await;
        if let Some(tx) = shutdown_tx.take() {
            // errors if there are no receivers, but that's irrelevant.
            let _ = tx.send(true);
            // wait for all receivers to drop/close.
            tx.closed().await;
        }

        Ok(())
    }
}
