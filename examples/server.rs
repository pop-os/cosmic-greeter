use greetd_ipc::{codec::SyncCodec, AuthMessageType, ErrorType, Request, Response};
use std::io;
use tokio::net::UnixListener;

#[tokio::main]
async fn main() {
    let listener = UnixListener::bind("socket").unwrap();
    println!("listening");

    loop {
        let (socket, _addr) = listener.accept().await.unwrap();
        println!("new connection");

        loop {
            let request = {
                socket.readable().await.unwrap();

                let mut bytes = Vec::with_capacity(4096);
                match socket.try_read_buf(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        println!("read {} bytes", count);
                    }
                    Err(err) => match err.kind() {
                        io::ErrorKind::WouldBlock => continue,
                        _ => {
                            println!("failed to read socket: {:?}", err);
                            break;
                        }
                    },
                }

                let mut cursor = io::Cursor::new(bytes);
                Request::read_from(&mut cursor).unwrap()
            };
            println!("{:?}", request);

            let response = match request {
                Request::CreateSession { .. } => Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_string(),
                },
                Request::PostAuthMessageResponse { response } => {
                    match response.as_ref().map(|x| x.as_str()) {
                        Some("password") => Response::Success,
                        _ => Response::Error {
                            error_type: ErrorType::AuthError,
                            description: "pam_authenticate: AUTH_ERR".to_string(),
                        },
                    }
                }
                Request::StartSession { .. } => Response::Success,
                _ => {
                    println!("unhandled request");
                    break;
                }
            };

            let mut bytes = Vec::with_capacity(4096);
            response.write_to(&mut bytes).unwrap();
            socket.try_write(&bytes).unwrap();
        }
    }
}
