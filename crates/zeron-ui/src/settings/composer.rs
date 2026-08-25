use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use zeron_proto::{HarnessId, ReasoningLevel};

const FILE_NAME: &str = "composer-defaults.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedModel {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteModel {
    pub harness: HarnessId,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ComposerDefaults {
    pub harness: Option<HarnessId>,
    pub model_by_harness: HashMap<HarnessId, RememberedModel>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_labels: HashMap<String, String>,
    pub device: Option<String>,
    pub project: Option<String>,
    pub no_project: bool,
    pub favorites: Vec<FavoriteModel>,
}

impl ComposerDefaults {
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<ComposerDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "composer-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    pub fn model_for(&self, harness: HarnessId) -> Option<&RememberedModel> {
        self.model_by_harness.get(&harness)
    }

    pub fn remember_model(&mut self, harness: HarnessId, id: String, label: String) {
        self.harness = Some(harness);
        self.model_by_harness
            .insert(harness, RememberedModel { id, label });
    }

    pub fn label_for(&self, id: &str) -> Option<&str> {
        self.model_labels.get(id).map(String::as_str)
    }

    pub fn is_favorite(&self, harness: HarnessId, model: &str) -> bool {
        self.favorites
            .iter()
            .any(|f| f.harness == harness && f.model == model)
    }

    pub fn toggle_favorite(&mut self, harness: HarnessId, model: &str) -> bool {
        if let Some(at) = self
            .favorites
            .iter()
            .position(|f| f.harness == harness && f.model == model)
        {
            self.favorites.remove(at);
            false
        } else {
            self.favorites.push(FavoriteModel {
                harness,
                model: model.to_string(),
            });
            true
        }
    }

    pub fn remember_labels<'a>(
        &mut self,
        models: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> bool {
        let mut changed = false;
        for (id, label) in models {
            if self.model_labels.get(id).map(String::as_str) != Some(label) {
                self.model_labels.insert(id.to_string(), label.to_string());
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = ComposerDefaults {
            harness: Some(HarnessId::ClaudeCode),
            reasoning: Some(ReasoningLevel::XHigh),
            ..Default::default()
        };
        defaults.remember_model(
            HarnessId::ClaudeCode,
            "claude-fable-5".into(),
            "Fable 5".into(),
        );
        defaults.remember_model(HarnessId::Codex, "gpt-5.2-codex".into(), "GPT-5.2".into());
        defaults.save(dir.path()).unwrap();
        let loaded = ComposerDefaults::load(dir.path());
        assert_eq!(loaded, defaults);
        assert_eq!(
            loaded.model_for(HarnessId::ClaudeCode).map(|m| &*m.label),
            Some("Fable 5")
        );
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
        std::fs::write(ComposerDefaults::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
    }

    #[test]
    fn favorites_toggle_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = ComposerDefaults::default();
        assert!(defaults.toggle_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(defaults.toggle_favorite(HarnessId::Codex, "gpt-5.2-codex"));
        assert!(defaults.is_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(!defaults.is_favorite(HarnessId::Codex, "claude-opus-5"));
        defaults.save(dir.path()).unwrap();
        assert_eq!(ComposerDefaults::load(dir.path()), defaults);
        assert!(!defaults.toggle_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(!defaults.is_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(defaults.is_favorite(HarnessId::Codex, "gpt-5.2-codex"));
    }

    #[test]
    fn remember_model_updates_harness_and_row() {
        let mut defaults = ComposerDefaults::default();
        defaults.remember_model(HarnessId::Codex, "m1".into(), "One".into());
        defaults.remember_model(HarnessId::Codex, "m2".into(), "Two".into());
        assert_eq!(defaults.harness, Some(HarnessId::Codex));
        assert_eq!(
            defaults.model_for(HarnessId::Codex).map(|m| &*m.id),
            Some("m2")
        );
        assert!(defaults.model_for(HarnessId::ClaudeCode).is_none());
    }
}
