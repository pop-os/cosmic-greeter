// Copyright 2025 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

//! Out-of-process PAM conversation worker.
//!
//! Each PAM stack (password, fingerprint) runs in its own short-lived child
//! process, re-execed from this same binary with `--pam-conversation <service>
//! <user>`. The parent (the locker) talks to it over the child's stdin/stdout
//! using a line-delimited RON protocol, and relays prompts/messages to the UI.

use std::ffi::{CStr, CString};
use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::fl;

/// Messages from the worker (child) to the host (parent/UI).
#[derive(Debug, Serialize, Deserialize)]
pub enum WorkerMsg {
    /// PAM asked for hidden input (a password). Expects an [`HostMsg::Input`].
    PromptEchoOff(String),
    /// PAM asked for visible input. Expects an [`HostMsg::Input`].
    PromptEchoOn(String),
    /// Informational text (e.g. "Place your finger on the reader").
    Info(String),
    /// Error text from a PAM module.
    Error(String),
    /// Authentication and account management succeeded.
    Success,
    /// Authentication failed; carries an already-localized message.
    Failure(String),
}

/// Messages from the host (parent) to the worker (child).
#[derive(Debug, Serialize, Deserialize)]
pub enum HostMsg {
    /// The value the user typed in response to a prompt.
    Input(String),
}

/// Convert PAM errors to user-friendly localized messages.
pub fn pam_error_to_message(error: &pam_client::Error) -> String {
    use pam_client::ErrorCode;

    // Use the structured error code instead of string matching for reliability
    match error.code() {
        ErrorCode::AUTH_ERR | ErrorCode::CRED_INSUFFICIENT => fl!("auth-error-credentials"),
        ErrorCode::PERM_DENIED => fl!("auth-error-denied"),
        ErrorCode::MAXTRIES => fl!("auth-error-maxtries"),
        ErrorCode::ACCT_EXPIRED | ErrorCode::USER_UNKNOWN => fl!("auth-error-account"),
        // For any other error, show a generic message
        _ => fl!("auth-error-default"),
    }
}

/// Write a single message to stdout as one RON line
fn send(msg: &WorkerMsg) {
    let Ok(line) = ron::to_string(msg) else {
        return;
    };
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

fn recv_input() -> Option<String> {
    let mut line = String::new();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    match stdin.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => match ron::from_str::<HostMsg>(line.trim_end()) {
            Ok(HostMsg::Input(value)) => Some(value),
            Err(_) => None,
        },
    }
}

/// PAM conversation handler that proxies every callback over stdio instead of
/// touching the UI directly.
struct WorkerConversation;

impl WorkerConversation {
    fn prompt(&mut self, prompt_c: &CStr, secret: bool) -> Result<CString, pam_client::ErrorCode> {
        let prompt = prompt_c
            .to_str()
            .map_err(|_| pam_client::ErrorCode::CONV_ERR)?
            .to_string();

        send(&if secret {
            WorkerMsg::PromptEchoOff(prompt)
        } else {
            WorkerMsg::PromptEchoOn(prompt)
        });

        let value = recv_input().ok_or(pam_client::ErrorCode::CONV_ERR)?;
        CString::new(value).map_err(|_| pam_client::ErrorCode::CONV_ERR)
    }
}

impl pam_client::ConversationHandler for WorkerConversation {
    fn prompt_echo_on(&mut self, prompt_c: &CStr) -> Result<CString, pam_client::ErrorCode> {
        self.prompt(prompt_c, false)
    }
    fn prompt_echo_off(&mut self, prompt_c: &CStr) -> Result<CString, pam_client::ErrorCode> {
        self.prompt(prompt_c, true)
    }
    fn text_info(&mut self, prompt_c: &CStr) {
        if let Ok(prompt) = prompt_c.to_str() {
            send(&WorkerMsg::Info(prompt.to_string()));
        }
    }
    fn error_msg(&mut self, prompt_c: &CStr) {
        if let Ok(prompt) = prompt_c.to_str() {
            send(&WorkerMsg::Error(prompt.to_string()));
        }
    }
}

/// Child entry point: run one PAM stack to completion and exit.
///
/// Reports the result over stdout and terminates the process (never returns).
pub fn run(service: &str, username: &str) -> ! {
    // Localization is needed for `pam_error_to_message`. Logging goes to stderr
    crate::localize::localize();
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let result = (|| -> Result<(), pam_client::Error> {
        let mut context = pam_client::Context::new(service, Some(username), WorkerConversation)?;
        tracing::info!("authenticate ({service})");
        context.authenticate(pam_client::Flag::NONE)?;
        tracing::info!("acct_mgmt ({service})");
        context.acct_mgmt(pam_client::Flag::NONE)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            send(&WorkerMsg::Success);
            std::process::exit(0);
        }
        Err(err) => {
            tracing::warn!("authentication error ({service}): {err}");
            send(&WorkerMsg::Failure(pam_error_to_message(&err)));
            std::process::exit(1);
        }
    }
}
