//! Typed world-model entity taxonomy without forcing a vault schema migration.
//!
//! The legacy vault has a deliberately fixed `NodeType` set. Connected reasoning needs
//! additional concepts such as activities, applications and files, but widening the
//! persistent enum would make older frontmatter/index readers fail. This additive type
//! lets session/world-model code use richer entities while mapping stable kinds onto the
//! existing vault where possible.

use serde::{Deserialize, Serialize};

use crate::model::NodeType;

/// Entity kinds understood by the connected world model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorldEntityKind {
    User,
    Project,
    Activity,
    Goal,
    Task,
    Application,
    File,
    Person,
    Concept,
    Decision,
    Event,
    Outcome,
    Preference,
    Fact,
    Error,
    Session,
    Note,
}

impl WorldEntityKind {
    /// Map a richer world-model kind to the current persistent vault taxonomy.
    ///
    /// `None` means the kind should remain a projected/session entity until a future,
    /// explicitly migrated vault schema supports it. Callers must not silently coerce
    /// applications/files/events into generic notes merely to persist them.
    pub fn legacy_node_type(self) -> Option<NodeType> {
        match self {
            Self::Project => Some(NodeType::Project),
            Self::Goal => Some(NodeType::Goal),
            Self::Task => Some(NodeType::Task),
            Self::Person => Some(NodeType::Person),
            Self::Decision => Some(NodeType::Decision),
            Self::Preference => Some(NodeType::Preference),
            Self::Fact => Some(NodeType::Fact),
            Self::Error => Some(NodeType::Error),
            Self::Session => Some(NodeType::Session),
            Self::Note => Some(NodeType::Note),
            Self::User
            | Self::Activity
            | Self::Application
            | Self::File
            | Self::Concept
            | Self::Event
            | Self::Outcome => None,
        }
    }

    /// Inverse mapping for existing vault nodes.
    pub fn from_legacy(node_type: NodeType) -> Self {
        match node_type {
            NodeType::Project => Self::Project,
            NodeType::Goal => Self::Goal,
            NodeType::Task => Self::Task,
            NodeType::Decision => Self::Decision,
            NodeType::Person => Self::Person,
            NodeType::Preference => Self::Preference,
            NodeType::Fact => Self::Fact,
            NodeType::Error => Self::Error,
            NodeType::Session => Self::Session,
            NodeType::Note => Self::Note,
        }
    }
}

/// A lightweight projected entity suitable for typed graph/UI contracts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldEntity {
    pub id: String,
    pub kind: WorldEntityKind,
    pub label: String,
    #[serde(default)]
    pub project: Option<String>,
    pub confidence: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_vault_types_round_trip() {
        for legacy in NodeType::ALL {
            let world = WorldEntityKind::from_legacy(legacy);
            assert_eq!(world.legacy_node_type(), Some(legacy));
        }
    }

    #[test]
    fn richer_projected_types_do_not_silently_coerce_to_notes() {
        for kind in [
            WorldEntityKind::User,
            WorldEntityKind::Activity,
            WorldEntityKind::Application,
            WorldEntityKind::File,
            WorldEntityKind::Concept,
            WorldEntityKind::Event,
            WorldEntityKind::Outcome,
        ] {
            assert_eq!(kind.legacy_node_type(), None);
        }
    }
}
