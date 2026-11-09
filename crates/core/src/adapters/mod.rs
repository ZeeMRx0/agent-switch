//! Surface adapters.

pub mod antigravity_cli;
pub mod claude_code;
pub mod claude_desktop;
pub mod chatgpt_desktop;
pub mod codex;
pub mod codex_cli;

use crate::config::SurfaceKind;
use crate::surface::Surface;

/// The production adapter for `kind`. Desktop adapters built here never quit an app; callers that have the
/// user's consent build them with `with_restart` / `with_quit` instead.
pub fn surface_for(kind: SurfaceKind) -> Box<dyn Surface> {
    match kind {
        SurfaceKind::ClaudeCli => Box::new(claude_code::ClaudeCode::production()),
        SurfaceKind::CodexCli => Box::new(codex_cli::CodexCli),
        SurfaceKind::ChatgptDesktop => Box::new(chatgpt_desktop::ChatgptDesktop::production()),
        SurfaceKind::ClaudeDesktop => Box::new(claude_desktop::ClaudeDesktop::production()),
        SurfaceKind::AntigravityCli => Box::new(antigravity_cli::AntigravityCli::production()),
    }
}
