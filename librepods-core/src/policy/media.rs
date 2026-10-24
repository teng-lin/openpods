// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Confirmed per-player pause ownership; no playback effects run here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub id: String,
    pub owner: String,
    pub playback: PlaybackState,
    pub can_pause: bool,
    pub can_play: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerTarget {
    pub id: String,
    pub owner: String,
}
#[derive(Default)]
pub struct PauseOwnership {
    pending: BTreeMap<String, String>,
    owned: BTreeMap<String, String>,
}
impl PauseOwnership {
    pub fn request_pause(&mut self, players: &[PlayerSnapshot]) -> Vec<PlayerTarget> {
        let mut targets = Vec::new();
        for player in players.iter().take(64) {
            if player.playback == PlaybackState::Playing
                && player.can_pause
                && !self.owned.contains_key(&player.id)
                && !self.pending.contains_key(&player.id)
            {
                self.pending.insert(player.id.clone(), player.owner.clone());
                targets.push(PlayerTarget {
                    id: player.id.clone(),
                    owner: player.owner.clone(),
                });
            }
        }
        targets
    }
    pub fn observe(&mut self, players: &[PlayerSnapshot]) {
        let mut next_pending = BTreeMap::new();
        for (id, owner) in &self.pending {
            if let Some(player) = players.iter().find(|p| p.id == *id && p.owner == *owner) {
                if player.playback == PlaybackState::Paused {
                    self.owned.insert(id.clone(), owner.clone());
                } else if player.playback == PlaybackState::Playing {
                    next_pending.insert(id.clone(), owner.clone());
                }
            }
        }
        self.pending = next_pending;
        self.owned.retain(|id, owner| {
            players
                .iter()
                .any(|p| p.id == *id && p.owner == *owner && p.playback == PlaybackState::Paused)
        });
    }
    pub fn resume_targets(&mut self, players: &[PlayerSnapshot]) -> Vec<PlayerTarget> {
        self.observe(players);
        let targets = players
            .iter()
            .filter(|p| p.can_play && self.owned.get(&p.id) == Some(&p.owner))
            .map(|p| PlayerTarget {
                id: p.id.clone(),
                owner: p.owner.clone(),
            })
            .collect();
        self.clear();
        targets
    }
    pub fn clear(&mut self) {
        self.pending.clear();
        self.owned.clear();
    }
    pub fn has_owned(&self) -> bool {
        !self.owned.is_empty()
    }
    pub fn pause_failed(&mut self, target: &PlayerTarget) {
        if self.pending.get(&target.id) == Some(&target.owner) {
            self.pending.remove(&target.id);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn player(id: &str, owner: &str, playback: PlaybackState) -> PlayerSnapshot {
        PlayerSnapshot {
            id: id.into(),
            owner: owner.into(),
            playback,
            can_pause: true,
            can_play: true,
        }
    }
    #[test]
    fn only_confirmed_owned_pauses_can_resume() {
        let mut policy = PauseOwnership::default();
        let playing = player("a", ":1.1", PlaybackState::Playing);
        let already_paused = player("b", ":1.2", PlaybackState::Paused);
        assert_eq!(
            policy
                .request_pause(&[playing.clone(), already_paused.clone()])
                .len(),
            1
        );
        assert!(!policy.has_owned());
        let paused = player("a", ":1.1", PlaybackState::Paused);
        policy.observe(&[paused.clone(), already_paused]);
        assert!(policy.has_owned());
        assert_eq!(
            policy.resume_targets(&[paused]),
            vec![PlayerTarget {
                id: "a".into(),
                owner: ":1.1".into()
            }]
        );
        assert!(!policy.has_owned());
    }
    #[test]
    fn manual_playback_owner_replacement_and_disappearance_clear_ownership() {
        for snapshot in [
            vec![player("a", ":1.1", PlaybackState::Playing)],
            vec![player("a", ":1.2", PlaybackState::Paused)],
            vec![],
        ] {
            let mut policy = PauseOwnership::default();
            policy.request_pause(&[player("a", ":1.1", PlaybackState::Playing)]);
            policy.observe(&[player("a", ":1.1", PlaybackState::Paused)]);
            assert!(policy.resume_targets(&snapshot).is_empty());
        }
    }
    #[test]
    fn failed_and_unsupported_pause_never_record_ownership() {
        let mut policy = PauseOwnership::default();
        let mut unsupported = player("b", ":1.2", PlaybackState::Playing);
        unsupported.can_pause = false;
        let targets =
            policy.request_pause(&[player("a", ":1.1", PlaybackState::Playing), unsupported]);
        policy.pause_failed(&targets[0]);
        policy.observe(&[player("a", ":1.1", PlaybackState::Paused)]);
        assert!(!policy.has_owned());
        assert!(policy
            .request_pause(&[player("a", ":1.1", PlaybackState::Stopped)])
            .is_empty());
    }
}
