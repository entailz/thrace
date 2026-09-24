/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

#![recursion_limit = "512"]

pub mod app;
pub mod audio;
pub mod config;
pub mod embed;
pub mod emoji;
pub mod emoji_font;
pub mod highlight;
pub mod history_queue;
pub mod markdown;
pub mod matrix;
pub mod media_cache;
pub mod recent_emoji;
pub mod session_store;
pub mod theme;
pub mod ui;
pub mod verify;
pub mod video;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    let config = config::ConfigFile::open(config::config_path()?)?;
    let cli_theme = std::env::args().skip_while(|a| a != "--theme").nth(1);
    app::slint_bridge::run(config, cli_theme).map_err(|e| anyhow::anyhow!(e))
}
