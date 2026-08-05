pub mod chess;
pub mod tictactoe;

use crate::app_base::GameApp;

/// Construct every game shipped with LRGP.
///
/// Integrations should use this registry instead of duplicating a list of
/// concrete game types. Adding another built-in game then requires one entry
/// here, while routers, manifest discovery, persistence hydration, and clients
/// continue to use the same generic paths.
pub fn builtin_games() -> Vec<Box<dyn GameApp>> {
    vec![
        Box::new(tictactoe::TicTacToeApp::new()),
        Box::new(chess::ChessApp::new()),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::constants::{CMD_ACCEPT, CMD_CHALLENGE, CMD_DECLINE};

    use super::*;

    #[test]
    fn builtin_registry_has_unique_coherent_manifests() {
        let games = builtin_games();
        assert!(games.len() >= 2);

        let mut ids = HashSet::new();
        for game in games {
            let manifest = game.manifest();
            assert_eq!(manifest.app_id, game.app_id());
            assert_eq!(manifest.version, game.version());
            assert!(ids.insert(manifest.app_id.clone()));
            assert!(!manifest.display_name.trim().is_empty());
            assert!(!manifest.icon.trim().is_empty());
            assert!(manifest.max_players >= 2);
            for lifecycle_action in [CMD_CHALLENGE, CMD_ACCEPT, CMD_DECLINE] {
                assert!(
                    manifest
                        .actions
                        .iter()
                        .any(|action| action == lifecycle_action),
                    "{} is missing {lifecycle_action}",
                    manifest.app_id
                );
            }
            for (action, method) in &manifest.preferred_delivery {
                assert!(manifest.actions.contains(action));
                assert!(matches!(
                    method.as_str(),
                    "opportunistic" | "direct" | "propagated"
                ));
            }
        }
    }
}
