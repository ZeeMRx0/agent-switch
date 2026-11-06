//! Which `Surface` implementation serves each surface kind.

use agent_switch_core::config::SurfaceKind;
use agent_switch_core::surface::Surface;

pub fn surface_for(kind: SurfaceKind) -> Box<dyn Surface> {
    agent_switch_core::adapters::surface_for(kind)
}

/// The surface for `switch`, except Claude Desktop, which `commands::switch` builds itself to read its mirror
/// result. `--restart` on a Desktop app is consent to quit it.
pub fn surface_for_switch(kind: SurfaceKind, restart: bool) -> Box<dyn Surface> {
    match kind {
        SurfaceKind::ChatgptDesktop if restart => Box::new(crate::codex_commands::chatgpt_with_quit()),
        _ => surface_for(kind),
    }
}

/// A line to print after a successful switch on `kind`, when the surface has something to tell the user.
pub fn switch_note(kind: SurfaceKind) -> Option<&'static str> {
    match kind {
        SurfaceKind::ClaudeCli => Some(
            "note: a running Claude Code session follows within about 30 seconds (restart it to skip the wait). \
             The first message after a switch misses the prompt cache; that is expected.",
        ),
        SurfaceKind::CodexCli => Some(
            "note: a Codex session that is already running keeps its login; the new account applies to sessions started from now on.",
        ),
        _ => None,
    }
}
