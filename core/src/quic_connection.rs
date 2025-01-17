use crate::{
    quic_connection_utils::{QuicConnectionError, QuicConnectionParameters, QuicConnectionUtils},
    structures::rotating_queue::RotatingQueue,
};
use anyhow::Context;
use futures::FutureExt;
use log::warn;
use quinn::{Connection, Endpoint};
use solana_sdk::pubkey::Pubkey;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

pub type EndpointPool = RotatingQueue<Endpoint>;

#[derive(Clone)]
#[warn(clippy::rc_clone_in_vec_init)]
pub struct QuicConnection {
    connection: Arc<RwLock<Option<Connection>>>,
    last_stable_id: Arc<AtomicU64>,
    endpoint: Endpoint,
    identity: Pubkey,
    socket_address: SocketAddr,
    connection_params: QuicConnectionParameters,
    exit_signal: Arc<AtomicBool>,
    timeout_counters: Arc<AtomicU64>,
    has_connected_once: Arc<AtomicBool>,
}

impl QuicConnection {
    pub fn new(
        identity: Pubkey,
        endpoint: Endpoint,
        socket_address: SocketAddr,
        connection_params: QuicConnectionParameters,
        exit_signal: Arc<AtomicBool>,
    ) -> Self {
        Self {
            connection: Arc::new(RwLock::new(None)),
            last_stable_id: Arc::new(AtomicU64::new(0)),
            endpoint,
            identity,
            socket_address,
            connection_params,
            exit_signal,
            timeout_counters: Arc::new(AtomicU64::new(0)),
            has_connected_once: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn connect(&self) -> Option<Connection> {
        QuicConnectionUtils::connect(
            self.identity,
            true,
            self.endpoint.clone(),
            self.socket_address,
            self.connection_params.connection_timeout,
            self.connection_params.connection_retry_count,
            self.exit_signal.clone(),
        )
        .await
    }

    pub async fn get_connection(&self) -> Option<Connection> {
        // get new connection reset if necessary
        let last_stable_id = self.last_stable_id.load(Ordering::Relaxed) as usize;
        let conn = self.connection.read().await.clone();
        match conn {
            Some(connection) => {
                if connection.stable_id() == last_stable_id {
                    let current_stable_id = connection.stable_id();
                    // problematic connection
                    let mut conn = self.connection.write().await;
                    let connection = conn.clone().expect("Connection cannot be None here");
                    // check may be already written by another thread
                    if connection.stable_id() != current_stable_id {
                        Some(connection)
                    } else {
                        let new_conn = self.connect().await;
                        if let Some(new_conn) = new_conn {
                            *conn = Some(new_conn);
                            conn.clone()
                        } else {
                            // could not connect
                            None
                        }
                    }
                } else {
                    Some(connection.clone())
                }
            }
            None => {
                let connection = self.connect().await;
                *self.connection.write().await = connection.clone();
                self.has_connected_once.store(true, Ordering::Relaxed);
                connection
            }
        }
    }

    pub async fn send_transaction(&self, tx: Vec<u8>) {
        let connection_retry_count = self.connection_params.connection_retry_count;
        for _ in 0..connection_retry_count {
            if self.exit_signal.load(Ordering::Relaxed) {
                // return
                return;
            }

            let mut do_retry = false;
            let connection = self.get_connection().await;

            if let Some(connection) = connection {
                let current_stable_id = connection.stable_id() as u64;
                match QuicConnectionUtils::open_unistream(
                    connection,
                    self.connection_params.unistream_timeout,
                )
                .await
                {
                    Ok(send_stream) => {
                        match QuicConnectionUtils::write_all(
                            send_stream,
                            &tx,
                            self.identity,
                            self.connection_params,
                        )
                        .await
                        {
                            Ok(()) => {
                                // do nothing
                            }
                            Err(QuicConnectionError::ConnectionError { retry }) => {
                                do_retry = retry;
                            }
                            Err(QuicConnectionError::TimeOut) => {
                                self.timeout_counters.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(QuicConnectionError::ConnectionError { retry }) => {
                        do_retry = retry;
                    }
                    Err(QuicConnectionError::TimeOut) => {
                        self.timeout_counters.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if do_retry {
                    self.last_stable_id
                        .store(current_stable_id, Ordering::Relaxed);
                    break;
                }
            } else {
                warn!(
                    "Could not establish connection with {}",
                    self.identity.to_string()
                );
                break;
            }
            if !do_retry {
                break;
            }
        }
    }

    pub fn get_timeout_count(&self) -> u64 {
        self.timeout_counters.load(Ordering::Relaxed)
    }

    pub fn reset_timeouts(&self) {
        self.timeout_counters.store(0, Ordering::Relaxed);
    }

    pub fn has_connected_atleast_once(&self) -> bool {
        self.has_connected_once.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub struct QuicConnectionPool {
    connections: Vec<QuicConnection>,
    // counting semaphore is ideal way to manage backpressure on the connection
    // because a connection can create only N unistream connections
    transactions_in_sending_semaphore: Vec<Arc<Semaphore>>,
}

pub struct PooledConnection {
    pub connection: QuicConnection,
    pub permit: OwnedSemaphorePermit,
}

impl QuicConnectionPool {
    pub fn new(
        identity: Pubkey,
        endpoints: EndpointPool,
        socket_address: SocketAddr,
        connection_parameters: QuicConnectionParameters,
        exit_signal: Arc<AtomicBool>,
        nb_connection: usize,
        max_number_of_unistream_connection: usize,
    ) -> Self {
        let mut connections = vec![];
        // should not clone connection each time but create a new one
        for _ in 0..nb_connection {
            connections.push(QuicConnection::new(
                identity,
                endpoints.get().expect("Should get and endpoint"),
                socket_address,
                connection_parameters,
                exit_signal.clone(),
            ));
        }
        Self {
            connections,
            transactions_in_sending_semaphore: {
                // should create a new semaphore each time so avoid vec[elem;count]
                let mut v = Vec::with_capacity(nb_connection);
                (0..nb_connection).for_each(|_| {
                    v.push(Arc::new(Semaphore::new(max_number_of_unistream_connection)))
                });
                v
            },
        }
    }

    pub async fn get_pooled_connection(&self) -> anyhow::Result<PooledConnection> {
        let (permit, index, _others) = futures::future::select_all(
            self.transactions_in_sending_semaphore
                .iter()
                .map(|x| x.clone().acquire_owned().boxed()),
        )
        .await;
        drop(_others);

        // establish a connection if the connection has not yet been used
        let connection = self.connections[index].clone();
        if !connection.has_connected_atleast_once() {
            connection.get_connection().await;
        }
        let permit = permit.context("Cannot aquire permit, connection pool erased")?;
        Ok(PooledConnection { connection, permit })
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}
