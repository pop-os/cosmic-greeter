// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use super::{Message, SocketState};
use cosmic::iced::Subscription;
use futures_util::SinkExt;
use greetd_ipc::codec::TokioCodec;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub fn subscription() -> Subscription<Message> {
    struct GreetdSubscription;
    cosmic::iced::subscription::channel(
        std::any::TypeId::of::<GreetdSubscription>(),
        1,
        |mut sender| async move {
            let (tx, mut rx) = mpsc::channel::<greetd_ipc::Request>(1);
            _ = sender.send(Message::GreetdChannel(tx)).await;

            let socket_path =
                std::env::var_os("GREETD_SOCK").expect("GREETD_SOCK environment not set");

            loop {
                _ = sender.send(Message::Reconnect).await;

                let mut stream = match UnixStream::connect(&socket_path).await {
                    Ok(stream) => stream,
                    Err(why) => {
                        _ = sender.send(Message::Socket(SocketState::Error(Arc::new(why))));

                        break;
                    }
                };

                _ = sender.send(Message::Socket(SocketState::Open)).await;

                while let Some(request) = rx.recv().await {
                    if let Err(why) = request.write_to(&mut stream).await {
                        log::error!("error writing to GREETD_SOCK stream: {why}");
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
                                            .send(Message::Prompt(
                                                auth_message,
                                                true,
                                                Some(String::new()),
                                            ))
                                            .await;
                                    }
                                    greetd_ipc::AuthMessageType::Visible => {
                                        _ = sender
                                            .send(Message::Prompt(
                                                auth_message,
                                                false,
                                                Some(String::new()),
                                            ))
                                            .await;
                                    }
                                    //TODO: treat error type differently?
                                    greetd_ipc::AuthMessageType::Info
                                    | greetd_ipc::AuthMessageType::Error => {
                                        _ = sender
                                            .send(Message::Prompt(auth_message, false, None))
                                            .await;
                                    }
                                },
                                greetd_ipc::Response::Error {
                                    error_type: _,
                                    description,
                                } => {
                                    //TODO: use error_type?
                                    match request {
                                        greetd_ipc::Request::CancelSession => {
                                            // Do not send errors for cancel session to gui
                                            log::warn!(
                                                "error while cancelling session: {}",
                                                description
                                            );
                                            // Reconnect to socket
                                            _ = break
                                        }
                                        _ => {
                                            _ = sender.send(Message::Error(description)).await;
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
                                        // Reconnect to socket
                                        break;
                                    }
                                },
                            }
                        }
                        Err(err) => {
                            log::error!("failed to read socket: {:?}", err);
                            break;
                        }
                    }
                }
            }

            futures_util::future::pending().await
        },
    )
}
