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
    host::run(app::Model::new(sessions::claude_home()))
}
