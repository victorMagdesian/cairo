use std::any::TypeId;

use anyhow::Result;
use lsp_server::{Notification, RequestId};
use rustc_hash::FxHashMap;
use serde_json::Value;

use super::api;
use super::schedule::Task;
use crate::server::connection::ClientSender;

type ResponseBuilder<'s> = Box<dyn FnOnce(lsp_server::Response) -> Task<'s>>;

pub struct Client<'s> {
    notifier: Notifier,
    responder: Responder,
    pub(super) requester: Requester<'s>,
}

#[derive(Clone)]
pub struct Notifier(ClientSender);

#[derive(Clone)]
pub struct Responder(ClientSender);

pub struct Requester<'s> {
    sender: ClientSender,
    next_request_id: i32,
    response_handlers: FxHashMap<RequestId, ResponseBuilder<'s>>,
}

impl<'s> Client<'s> {
    pub fn new(sender: ClientSender) -> Self {
        Self {
            notifier: Notifier(sender.clone()),
            responder: Responder(sender.clone()),
            requester: Requester {
                sender,
                next_request_id: 1,
                response_handlers: FxHashMap::default(),
            },
        }
    }

    pub fn notifier(&self) -> Notifier {
        self.notifier.clone()
    }

    pub fn responder(&self) -> Responder {
        self.responder.clone()
    }
}

impl Notifier {
    pub fn notify<N>(&self, params: N::Params) -> Result<()>
    where
        N: lsp_types::notification::Notification,
    {
        let method = N::METHOD.to_string();

        let message = lsp_server::Message::Notification(Notification::new(method, params));

        self.0.send(message)
    }
}

impl Responder {
    pub fn respond<R>(&self, id: RequestId, result: Result<R, api::Error>) -> Result<()>
    where
        R: serde::Serialize,
    {
        self.0.send(
            match result {
                Ok(res) => lsp_server::Response::new_ok(id, res),
                Err(api::Error { code, error }) => {
                    lsp_server::Response::new_err(id, code as i32, format!("{error}"))
                }
            }
            .into(),
        )
    }
}

impl<'s> Requester<'s> {
    /// Sends a request of kind `R` to the client, with associated parameters.
    /// The task provided by `response_handler` will be dispatched as soon as the response
    /// comes back from the client.
    pub fn request<R>(
        &mut self,
        params: R::Params,
        response_handler: impl Fn(R::Result) -> Task<'s> + 'static,
    ) -> Result<()>
    where
        R: lsp_types::request::Request,
    {
        let serialized_params = serde_json::to_value(params)?;

        self.response_handlers.insert(
            self.next_request_id.into(),
            Box::new(move |response: lsp_server::Response| {
                match (response.error, response.result) {
                    (Some(err), _) => {
                        tracing::error!(
                            "Got an error from the client (code {}): {}",
                            err.code,
                            err.message
                        );
                        Task::nothing()
                    }
                    (None, Some(response)) => match serde_json::from_value(response) {
                        Ok(response) => response_handler(response),
                        Err(error) => {
                            tracing::error!("Failed to deserialize response from server: {error}");
                            Task::nothing()
                        }
                    },
                    (None, None) => {
                        if TypeId::of::<R::Result>() == TypeId::of::<()>() {
                            // We can't call `response_handler(())` directly here, but
                            // since we _know_ the type expected is `()`, we can use
                            // `from_value(Value::Null)`. `R::Result` implements `DeserializeOwned`,
                            // so this branch works in the general case but we'll only
                            // hit it if the concrete type is `()`, so the `unwrap()` is safe here.
                            response_handler(serde_json::from_value(Value::Null).unwrap());
                        } else {
                            tracing::error!(
                                "Server response was invalid: did not contain a result or error"
                            );
                        }
                        Task::nothing()
                    }
                }
            }),
        );

        self.sender.send(lsp_server::Message::Request(lsp_server::Request {
            id: self.next_request_id.into(),
            method: R::METHOD.into(),
            params: serialized_params,
        }))?;

        self.next_request_id += 1;

        Ok(())
    }

    pub fn pop_response_task(&mut self, response: lsp_server::Response) -> Task<'s> {
        if let Some(handler) = self.response_handlers.remove(&response.id) {
            handler(response)
        } else {
            tracing::error!("Received a response with ID {}, which was not expected", response.id);
            Task::nothing()
        }
    }
}