use greetd_ipc::{AuthMessageType, ErrorType, Request, Response, codec::TokioCodec};
use std::{env, fs, io, thread};
use tokio::net::UnixListener;

fn main() {
    let greetd_sock = env::current_dir().unwrap().join("socket");
    if greetd_sock.exists() {
        fs::remove_file(&greetd_sock).unwrap();
    }

    // Set up the socket environment variable before spawning greeter
    unsafe { env::set_var("GREETD_SOCK", &greetd_sock) };

    // Spawn the mock greetd server in a background thread
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = UnixListener::bind(&greetd_sock).unwrap();
            println!("listening at {:?}", greetd_sock);

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
                        Request::read_from(&mut cursor).await.unwrap()
                    };
                    println!("{:?}", request);

                    let response = match request {
                        Request::CreateSession { .. } => Response::AuthMessage {
                            auth_message_type: AuthMessageType::Secret,
                            auth_message: "MOCKING:".to_string(),
                        },
                        Request::PostAuthMessageResponse { response } => {
                            match response.as_deref() {
                                Some("password") => Response::Success,
                                _ => {
                                    // Add 1 second delay to simulate real PAM authentication failure behavior
                                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                    Response::Error {
                                        error_type: ErrorType::AuthError,
                                        description: "AUTH_ERR".to_string(),
                                    }
                                }
                            }
                        }
                        Request::StartSession { .. } => Response::Success,
                        Request::CancelSession => Response::Success,
                    };

                    let mut bytes = Vec::with_capacity(4096);
                    response.write_to(&mut bytes).await.unwrap();
                    socket.try_write(&bytes).unwrap();
                }
            }
        });
    });

    // Run greeter on main thread (required for winit event loop)
    cosmic_greeter::greeter::main().unwrap();
}
