//! ccsm — browse, preview, trash and restore Claude Code sessions.
#![cfg_attr(feature = "standalone", windows_subsystem = "windows")]

mod app;
mod sessions;
mod view;
// Render host: the real terminal, or (feature `standalone`) a self-contained egui window.
#[cfg_attr(feature = "standalone", path = "standalone.rs")]
#[cfg_attr(not(feature = "standalone"), path = "terminal.rs")]
mod host;

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        // CCSM_BUILD is the CI run number, set only for numbered pre-release builds.
        let build = option_env!("CCSM_BUILD").filter(|n| !n.is_empty()).map(|n| format!("+{n}")).unwrap_or_default();
        println!("ccsm {}{build}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    host::run(app::Model::new(sessions::claude_home()))
}
