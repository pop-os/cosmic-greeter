use std::os::unix::fs::PermissionsExt as _;

pub fn init() -> anyhow::Result<()> {
    let path = cosmic_settings_daemon_config::greeter::GreeterAccessibilityState::path();
    std::fs::create_dir_all(&path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
