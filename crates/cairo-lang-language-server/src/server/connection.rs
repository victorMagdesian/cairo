use std::sync::{Arc, Weak};

use lsp_server as lsp;
use lsp_types::notification::Notification;
use lsp_types::request::Request;

type ConnectionSender = crossbeam::channel::Sender<lsp::Message>;
type ConnectionReceiver = crossbeam::channel::Receiver<lsp::Message>;

/// A builder for `Connection` that handles LSP initialization.
pub struct ConnectionInitializer {
    connection: lsp::Connection,
    /// None in tests, Some(_) otherwise
    threads: Option<lsp::IoThreads>,
}

/// Handles inbound and outbound messages with the client.
pub struct Connection {
    sender: Arc<ConnectionSender>,
    receiver: ConnectionReceiver,
    /// None in tests, Some(_) otherwise
    threads: Option<lsp::IoThreads>,
}

impl ConnectionInitializer {
    /// Create a new LSP server connection over stdin/stdout.
    pub fn stdio() -> Self {
        let (connection, threads) = lsp::Connection::stdio();
        Self { connection, threads: Some(threads) }
    }

    /// Create a new LSP server connection in memory.
    pub fn memory() -> (Self, lsp::Connection) {
        let (server, client) = lsp::Connection::memory();
        (Self { connection: server, threads: None }, client)
    }

    /// Starts the initialization process with the client by listening for an initialization
    /// request. Returns a request ID that should be passed into `initialize_finish` later,
    /// along with the initialization parameters that were provided.
    pub fn initialize_start(
        &self,
    ) -> anyhow::Result<(lsp::RequestId, lsp_types::InitializeParams)> {
        let (id, params) = self.connection.initialize_start()?;
        Ok((id, serde_json::from_value(params)?))
    }

    /// Finishes the initialization process with the client,
    /// returning an initialized `Connection`.
    pub fn initialize_finish(
        self,
        id: lsp::RequestId,
        server_capabilities: lsp_types::ServerCapabilities,
    ) -> anyhow::Result<Connection> {
        let initialize_result =
            lsp_types::InitializeResult { capabilities: server_capabilities, server_info: None };
        self.connection.initialize_finish(id, serde_json::to_value(initialize_result).unwrap())?;
        let Self { connection: lsp::Connection { sender, receiver }, threads } = self;
        Ok(Connection { sender: Arc::new(sender), receiver, threads })
    }
}

impl Connection {
    /// Make a new `ClientSender` for sending messages to the client.
    pub fn make_sender(&self) -> ClientSender {
        ClientSender { weak_sender: Arc::downgrade(&self.sender) }
    }

    /// An iterator over incoming messages from the client.
    pub fn incoming(&self) -> crossbeam::channel::Iter<'_, lsp::Message> {
        self.receiver.iter()
    }

    /// Check and respond to any incoming shutdown requests; returns`true` if the server should be
    /// shutdown.
    pub fn handle_shutdown(&self, message: &lsp::Message) -> anyhow::Result<bool> {
        match message {
            lsp::Message::Request(lsp::Request { id, method, .. })
                if method == lsp_types::request::Shutdown::METHOD =>
            {
                self.sender.send(lsp::Response::new_ok(id.clone(), ()).into())?;
                tracing::info!("Shutdown request received. Waiting for an exit notification...");
                match self.receiver.recv_timeout(std::time::Duration::from_secs(30))? {
                    lsp::Message::Notification(lsp::Notification { method, .. })
                        if method == lsp_types::notification::Exit::METHOD =>
                    {
                        tracing::info!("Exit notification received. Server shutting down...");
                        Ok(true)
                    }
                    message => anyhow::bail!(
                        "Server received unexpected message {message:?} while waiting for exit \
                         notification"
                    ),
                }
            }
            lsp::Message::Notification(lsp::Notification { method, .. })
                if method == lsp_types::notification::Exit::METHOD =>
            {
                tracing::error!(
                    "Server received an exit notification before a shutdown request was sent. \
                     Exiting..."
                );
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Join the I/O threads that underpin this connection.
    /// This is guaranteed to be nearly immediate since
    /// we close the only active channels to these threads prior
    /// to joining them.
    pub fn close(self) -> anyhow::Result<()> {
        drop(
            Arc::into_inner(self.sender)
                .expect("the client sender shouldn't have more than one strong reference"),
        );
        drop(self.receiver);

        if let Some(threads) = self.threads {
            threads.join()?;
        }
        Ok(())
    }
}

/// A weak reference to an underlying sender channel, used for communication with the client.
/// If the `Connection` that created this `ClientSender` is dropped, any `send` calls will throw
/// an error.
#[derive(Clone, Debug)]
pub struct ClientSender {
    weak_sender: Weak<ConnectionSender>,
}

// note: additional wrapper functions for senders may be implemented as needed.
impl ClientSender {
    pub(crate) fn send(&self, msg: lsp::Message) -> anyhow::Result<()> {
        let Some(sender) = self.weak_sender.upgrade() else {
            anyhow::bail!("The connection with the client has been closed");
        };

        Ok(sender.send(msg)?)
    }
}