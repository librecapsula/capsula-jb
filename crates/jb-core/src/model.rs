//! The speaker model registry — the single definition both platform
//! crates (rockchip/amlogic) and both UIs (CLI/TUI) share. Adding a
//! model = a new variant in [`Model`] + an arm in `ALL`; everything
//! derived from the registry (the TUI menu, the backup layout scan,
//! the CLI `--model` vocabulary, the config hints) follows by itself.

use serde::{Deserialize, Serialize};

/// The supported speaker models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    /// The big Capsula (RK3308, rockchip).
    Capsula,
    /// The late-revision Capsula Mini (A113X, amlogic).
    CapsulaMiniAmlogic,
}

impl Model {
    /// The registry and its order: menu rows top to bottom, backup
    /// catalog scans, error vocabularies.
    pub const ALL: &'static [Model] = &[Model::Capsula, Model::CapsulaMiniAmlogic];

    /// The human label (the TUI menu, logs).
    pub fn label(&self) -> &'static str {
        match self {
            Model::Capsula => "Капсула (Большая)",
            Model::CapsulaMiniAmlogic => "Капсула Мини (Amlogic)",
        }
    }

    /// The canonical name — one string, three roles by design: the
    /// CLI `--model` value, the backup layout segment
    /// (`backup/<name>/<key>/` — identity keys of different platforms
    /// may collide, so the layout is per-model) and the serde name in
    /// `backup.json` (kept in sync by the test below).
    pub fn name(&self) -> &'static str {
        match self {
            Model::Capsula => "capsula",
            Model::CapsulaMiniAmlogic => "capsula-mini-amlogic",
        }
    }

    /// Parse the canonical model name — the same vocabulary as
    /// [`Model::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        Model::ALL.iter().copied().find(|m| m.name() == name)
    }

    /// The vocabulary for messages: "capsula | capsula-mini-amlogic".
    pub fn names() -> String {
        Model::ALL
            .iter()
            .map(|m| m.name())
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_roundtrips_every_model() {
        for m in Model::ALL {
            assert_eq!(Model::from_name(m.name()), Some(*m));
        }
        assert_eq!(Model::from_name("Capsula"), None);
        assert_eq!(Model::from_name(""), None);
    }

    #[test]
    fn serde_name_matches_canonical_name() {
        for m in Model::ALL {
            let json = serde_json::to_string(m).unwrap();
            assert_eq!(json, format!("\"{}\"", m.name()));
            let back: Model = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *m);
        }
    }

    #[test]
    fn names_vocabulary() {
        assert_eq!(Model::names(), "capsula | capsula-mini-amlogic");
    }
}
