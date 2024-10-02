use std::fmt;

use lsp_server::{ErrorCode, ExtractError, Notification, Request, RequestId};
use lsp_types::notification::{
    Cancel, DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles,
    DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as NotificationTrait,
};
use lsp_types::request::{
    CodeActionRequest, Completion, ExecuteCommand, Formatting, GotoDefinition, HoverRequest,
    Request as RequestTrait, SemanticTokensFullRequest,
};
use tracing::{error, warn};

use crate::lsp::ext::{ExpandMacro, ProvideVirtualFile, ViewAnalyzedCrates};
use crate::server::schedule::Task;
use crate::Backend;

pub mod traits;

use super::client::Responder;
use super::schedule::BackgroundSchedule;
use crate::state::State;

pub(crate) fn request<'a>(request: Request) -> Task<'a> {
    let id = request.id.clone();

    match request.method.as_str() {
        CodeActionRequest::METHOD => background_request_task::<CodeActionRequest>(
            request,
            BackgroundSchedule::LatencySensitive,
        ),
        Completion::METHOD => {
            background_request_task::<Completion>(request, BackgroundSchedule::LatencySensitive)
        }
        ExecuteCommand::METHOD => local_request_task::<ExecuteCommand>(request),
        ExpandMacro::METHOD => {
            background_request_task::<ExpandMacro>(request, BackgroundSchedule::Worker)
        }
        Formatting::METHOD => {
            background_request_task::<Formatting>(request, BackgroundSchedule::Fmt)
        }
        GotoDefinition::METHOD => {
            background_request_task::<GotoDefinition>(request, BackgroundSchedule::LatencySensitive)
        }
        HoverRequest::METHOD => {
            background_request_task::<HoverRequest>(request, BackgroundSchedule::LatencySensitive)
        }
        ProvideVirtualFile::METHOD => background_request_task::<ProvideVirtualFile>(
            request,
            BackgroundSchedule::LatencySensitive,
        ),
        SemanticTokensFullRequest::METHOD => background_request_task::<SemanticTokensFullRequest>(
            request,
            BackgroundSchedule::Worker,
        ),
        ViewAnalyzedCrates::METHOD => {
            background_request_task::<ViewAnalyzedCrates>(request, BackgroundSchedule::Worker)
        }

        method => {
            warn!("received request {method} which does not have a handler");
            return Task::nothing();
        }
    }
    .unwrap_or_else(|error| {
        error!("encountered error when routing request with ID {id}: {error}");
        let result: Result<(), LSPError> = Err(error);
        Task::immediate(id, result)
    })
}

pub(crate) fn notification<'a>(notification: Notification) -> Task<'a> {
    match notification.method.as_str() {
        Cancel::METHOD => local_notification_task::<Cancel>(notification),
        DidChangeTextDocument::METHOD => {
            local_notification_task::<DidChangeTextDocument>(notification)
        }
        DidChangeConfiguration::METHOD => {
            local_notification_task::<DidChangeConfiguration>(notification)
        }
        DidChangeWatchedFiles::METHOD => {
            local_notification_task::<DidChangeWatchedFiles>(notification)
        }
        DidCloseTextDocument::METHOD => {
            local_notification_task::<DidCloseTextDocument>(notification)
        }
        DidOpenTextDocument::METHOD => local_notification_task::<DidOpenTextDocument>(notification),
        DidSaveTextDocument::METHOD => local_notification_task::<DidSaveTextDocument>(notification),
        method => {
            warn!("received notification {method} which does not have a handler");

            return Task::nothing();
        }
    }
    .unwrap_or_else(|error| {
        error!("encountered error when routing notification: {error}");

        Task::nothing()
    })
}

fn local_request_task<'a, R: traits::SyncRequestHandler>(
    request: Request,
) -> Result<Task<'a>, LSPError> {
    let (id, params) = cast_request::<R>(request)?;
    Ok(Task::local(move |state, notifier, requester, responder| {
        let result = R::run(state, notifier, requester, params);
        respond::<R>(id, result, &responder);
    }))
}

fn background_request_task<'a, R: traits::BackgroundDocumentRequestHandler>(
    request: Request,
    schedule: BackgroundSchedule,
) -> Result<Task<'a>, LSPError> {
    let (id, params) = cast_request::<R>(request)?;
    Ok(Task::background(schedule, move |state: &State| {
        let state_snapshot = state.snapshot();
        Box::new(move |notifier, responder| {
            let result =
                Backend::catch_panics(|| R::run_with_snapshot(state_snapshot, notifier, params))
                    .and_then(|res| res);
            respond::<R>(id, result, &responder);
        })
    }))
}

fn local_notification_task<'a, N: traits::SyncNotificationHandler>(
    notification: Notification,
) -> Result<Task<'a>, LSPError> {
    let (id, params) = cast_notification::<N>(notification)?;
    Ok(Task::local(move |session, notifier, requester, _| {
        if let Err(err) = N::run(session, notifier, requester, params) {
            error!("an error occurred while running {id}: {err}");
            // show_err_msg!("Ruff encountered a problem. Check the logs for more details.");
        }
    }))
}

/// Tries to cast a serialized request from the server into
/// a parameter type for a specific request handler.
/// It is *highly* recommended to not override this function in your
/// implementation.
fn cast_request<R: RequestTrait>(request: Request) -> Result<(RequestId, R::Params), LSPError> {
    request
        .extract(R::METHOD)
        .map_err(|error| match error {
            json_error @ ExtractError::JsonError { .. } => {
                anyhow::anyhow!("JSON parsing failure:\n{json_error}")
            }
            ExtractError::MethodMismatch(_) => {
                unreachable!(
                    "a method mismatch should not be possible here unless you've used a different \
                     handler (`R`) than the one whose method name was matched against earlier"
                )
            }
        })
        .with_failure_code(ErrorCode::InternalError)
}

/// Sends back a response to the lsp_server using a [`Responder`].
fn respond<R: RequestTrait>(id: RequestId, result: LSPResult<R::Result>, responder: &Responder) {
    if let Err(err) = &result {
        error!("an error occurred with result ID {id}: {err}");
        // show_err_msg!("Ruff encountered a problem. Check the logs for more details.");
    }
    if let Err(err) = responder.respond(id, result) {
        error!("failed to send response: {err}");
    }
}

/// Tries to cast a serialized request from the lsp_server into
/// a parameter type for a specific request handler.
fn cast_notification<N: NotificationTrait>(
    notification: Notification,
) -> Result<(&'static str, N::Params), LSPError> {
    Ok((
        N::METHOD,
        notification
            .extract(N::METHOD)
            .map_err(|error| match error {
                json_error @ ExtractError::JsonError { .. } => {
                    anyhow::anyhow!("JSON parsing failure:\n{json_error}")
                }
                ExtractError::MethodMismatch(_) => {
                    unreachable!(
                        "a method mismatch should not be possible here unless you've used a \
                         different handler (`N`) than the one whose method name was matched \
                         against earlier"
                    )
                }
            })
            .with_failure_code(ErrorCode::InternalError)?,
    ))
}

pub(crate) struct LSPError {
    pub(crate) code: ErrorCode,
    pub(crate) error: anyhow::Error,
}

pub type LSPResult<T> = Result<T, LSPError>;

/// A trait to convert result types into the lsp_server result type, [`LSPResult`].
pub trait LSPResultEx<T> {
    fn with_failure_code(self, code: ErrorCode) -> Result<T, LSPError>;
}

impl<T, E: Into<anyhow::Error>> LSPResultEx<T> for Result<T, E> {
    fn with_failure_code(self, code: ErrorCode) -> Result<T, LSPError> {
        self.map_err(|error| LSPError::new(error.into(), code))
    }
}

impl LSPError {
    pub(crate) fn new(error: anyhow::Error, code: ErrorCode) -> Self {
        Self { code, error }
    }
}

// Right now, we treat the error code as invisible data that won't
// be printed.
impl fmt::Debug for LSPError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl fmt::Display for LSPError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}
