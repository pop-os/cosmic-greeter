fn main() {
    use pam_client::conv_cli::Conversation;
    use pam_client::{Context, Flag}; // CLI implementation

    let passwd = pwd::Passwd::current_user().expect("Failed to get current user");

    let mut context = Context::new(
        "cosmic-greeter",    // Service name, decides which policy is used (see `/etc/pam.d`)
        Some(&passwd.name),  // Optional preset user name
        Conversation::new(), // Handler for user interaction
    )
    .expect("Failed to initialize PAM context");

    // Authenticate the user (ask for password, 2nd-factor token, fingerprint, etc.)
    context
        .authenticate(Flag::NONE)
        .expect("Authentication failed");

    // Validate the account (is not locked, expired, etc.)
    context
        .acct_mgmt(Flag::NONE)
        .expect("Account validation failed");
}
