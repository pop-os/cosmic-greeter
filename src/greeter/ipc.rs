// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use super::{Message, SocketState};
use cosmic::iced::Subscription;
use futures_util::SinkExt;
use greetd_ipc::codec::TokioCodec;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::{common, fl};

/// Convert greetd error descriptions to user-friendly localized messages
fn greetd_error_to_message(error_type: greetd_ipc::ErrorType, description: &str) -> String {
    use greetd_ipc::ErrorType;

    match error_type {
        ErrorType::AuthError => {
            // For authentication errors, check description for specific error types
            if description.contains("PERM_DENIED") {
                fl!("auth-error-denied")
            } else if description.contains("MAXTRIES") {
                fl!("auth-error-maxtries")
            } else if description.contains("ACCT_EXPIRED") || description.contains("USER_UNKNOWN") {
                fl!("auth-error-account")
            } else {
                fl!("auth-error-credentials")
            }
        }
        ErrorType::Error => {
            // For generic errors, show a generic message
            fl!("auth-error-default")
        }
    }
}

pub fn subscription() -> Subscription<Message> {
    struct GreetdSubscription;
    Subscription::run_with_id(
        std::any::TypeId::of::<GreetdSubscription>(),
        cosmic::iced_futures::stream::channel(1, |mut sender| async move {
            let (tx, mut rx) = mpsc::channel::<greetd_ipc::Request>(1);
            _ = sender.send(Message::GreetdChannel(tx)).await;

            let socket_path =
                std::env::var_os("GREETD_SOCK").expect("GREETD_SOCK environment not set");

            let mut interval = tokio::time::interval(Duration::from_secs(1));

            loop {
                _ = sender.send(Message::Reconnect).await;

                let mut stream = match UnixStream::connect(&socket_path).await {
                    Ok(stream) => stream,
                    Err(why) => {
                        tracing::error!("greetd IPC socket connection failed: {why:?}");
                        _ = sender.send(Message::Socket(SocketState::Error(Arc::new(why))));

                        break;
                    }
                };

                _ = sender.send(Message::Socket(SocketState::Open)).await;

                while let Some(request) = rx.recv().await {
                    if let Err(why) = request.write_to(&mut stream).await {
                        tracing::error!("error writing to GREETD_SOCK stream: {why:?}");
                        break;
                    }

                    match greetd_ipc::Response::read_from(&mut stream).await {
                        Ok(response) => {
                            match response {
                                greetd_ipc::Response::AuthMessage {
                                    auth_message_type,
                                    auth_message,
                                } => match auth_message_type {
                                    greetd_ipc::AuthMessageType::Secret => {
                                        _ = sender
                                            .send(
                                                common::Message::Prompt(
                                                    auth_message,
                                                    true,
                                                    Some(String::new()),
                                                )
                                                .into(),
                                            )
                                            .await;
                                    }
                                    greetd_ipc::AuthMessageType::Visible => {
                                        _ = sender
                                            .send(
                                                common::Message::Prompt(
                                                    auth_message,
                                                    false,
                                                    Some(String::new()),
                                                )
                                                .into(),
                                            )
                                            .await;
                                    }
                                    greetd_ipc::AuthMessageType::Info => {
                                        _ = sender
                                            .send(
                                                common::Message::Prompt(auth_message, false, None)
                                                    .into(),
                                            )
                                            .await;
                                    }
                                    greetd_ipc::AuthMessageType::Error => {
                                        _ = sender.send(Message::Error(auth_message)).await;
                                    }
                                },
                                greetd_ipc::Response::Error {
                                    error_type,
                                    description,
                                } => {
                                    match request {
                                        greetd_ipc::Request::CancelSession => {
                                            // Do not send errors for cancel session to gui
                                            tracing::warn!(
                                                "error while cancelling session: {}",
                                                description
                                            );

                                            // Reconnect to socket
                                            break;
                                        }
                                        _ => {
                                            _ = sender
                                                .send(Message::Error(greetd_error_to_message(
                                                    error_type,
                                                    &description,
                                                )))
                                                .await;
                                        }
                                    }
                                }
                                greetd_ipc::Response::Success => match request {
                                    greetd_ipc::Request::CreateSession { .. } => {
                                        // User has no auth required, proceed to login
                                        _ = sender.send(Message::Login).await;
                                    }
                                    greetd_ipc::Request::PostAuthMessageResponse { .. } => {
                                        // All auth is completed, proceed to login
                                        _ = sender.send(Message::Login).await;
                                    }
                                    greetd_ipc::Request::StartSession { .. } => {
                                        // Session has been started, exit greeter
                                        _ = sender.send(Message::Exit).await;
                                    }
                                    greetd_ipc::Request::CancelSession => {
                                        tracing::info!("greetd IPC session canceled");
                                        // Reconnect to socket
                                        break;
                                    }
                                },
                            }
                        }
                        Err(err) => {
                            tracing::error!("failed to read socket: {:?}", err);
                            break;
                        }
                    }
                }

                tracing::info!("reconnecting to greetd IPC socket");
                interval.tick().await;
            }

            futures_util::future::pending().await
        }),
    )
}
