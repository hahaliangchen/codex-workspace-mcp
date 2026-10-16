//! Optional per-use overrides; resolving them never mutates the main state.
use crate::agent_service::AgentServiceState;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const OUTPUT_RESERVE: usize = 24_576;
/// Used when the DSH model entry does not declare `maxTokens`.
pub const DEFAULT_MAX_OUTPUT: usize = 32_768;

/// The model's declared output limit, bounded by what the window has left
/// after `input` tokens. Input budgets already leave OUTPUT_RESERVE free, so
/// a request within budget always receives at least that much.
pub fn output_tokens(
    state: &AgentServiceState,
    model: &str,
    input_budget: usize,
    input: usize,
) -> usize {
    let declared = state
        .provider_routes
        .get(&state.provider_name)
        .and_then(|route| route.max_output_tokens.get(model))
        .copied()
        .unwrap_or(DEFAULT_MAX_OUTPUT);
    declared
        .min((input_budget + OUTPUT_RESERVE).saturating_sub(input))
        .max(1)
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<usize>,
}
impl ModelSettings {
    pub fn normalize(&mut self) -> Result<()> {
        for value in [
            &mut self.provider,
            &mut self.model,
            &mut self.reasoning_effort,
        ] {
            *value = value
                .take()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
        }
        ensure!(
            self.reasoning_effort
                .as_deref()
                .is_none_or(|s| matches!(s, "none" | "low" | "medium" | "high" | "xhigh" | "max")),
            "unsupported reasoning effort"
        );
        ensure!(
            self.context_window
                .is_none_or(|n| (32_768..=1_050_000).contains(&n)),
            "context window must be between 32768 and 1050000 tokens"
        );
        Ok(())
    }
    pub fn input_budget(&self, inherited: usize) -> usize {
        self.context_window
            .map(|n| n.saturating_sub(OUTPUT_RESERVE))
            .unwrap_or(inherited)
    }
    pub fn resolve(&self, main: &AgentServiceState, main_model: &str) -> Result<AgentServiceState> {
        let mut state = main.clone();
        if let Some(provider) = &self.provider {
            let route = main
                .provider_routes
                .get(provider)
                .context("configured model provider is unavailable")?;
            state.provider_name = provider.clone();
            state.provider_url = route.url.clone();
            state.api_key = route.api_key.clone();
            state.model_map = route.models.clone();
        }
        let model = self.model.as_deref().unwrap_or(main_model);
        state.default_model = state
            .model_map
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned());
        if let Some(effort) = &self.reasoning_effort {
            state.reasoning_effort = Some(effort.clone());
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_overrides_preserve_main_and_empty_fields_inherit() {
        let root = std::env::temp_dir().join(crate::visual_artifacts::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let mut main =
            crate::agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
        main.reasoning_effort = Some("high".into());
        main.provider_routes.insert(
            "alternate".into(),
            crate::agent_service::AgentProviderRoute {
                url: "http://alternate.example/v1".into(),
                api_key: "fixture-key".into(),
                models: [("alias".into(), "mapped-model".into())]
                    .into_iter()
                    .collect(),
                max_output_tokens: [("mapped-model".into(), 64_000)].into_iter().collect(),
            },
        );
        let routed = ModelSettings {
            provider: Some("alternate".into()),
            model: Some("alias".into()),
            ..Default::default()
        }
        .resolve(&main, "turn-model")
        .unwrap();
        assert_eq!(routed.provider_url, "http://alternate.example/v1");
        assert_eq!(routed.default_model, "mapped-model");
        assert_eq!(routed.api_key, "fixture-key");
        assert_ne!(main.provider_name, routed.provider_name);
        assert_eq!(
            output_tokens(&routed, "mapped-model", 231_424, 1_000),
            64_000
        );
        assert_eq!(
            output_tokens(&routed, "mapped-model", 231_424, 231_424),
            OUTPUT_RESERVE
        );
        assert_eq!(
            output_tokens(&routed, "undeclared", 231_424, 1_000),
            DEFAULT_MAX_OUTPUT
        );

        let inherited = ModelSettings::default()
            .resolve(&main, "turn-model")
            .unwrap();
        assert_eq!(inherited.default_model, "turn-model");
        assert_eq!(inherited.reasoning_effort, main.reasoning_effort);
        assert_eq!(ModelSettings::default().input_budget(231424), 231424);
        let config = ModelSettings {
            model: Some("gpt-6.1-sol".into()),
            reasoning_effort: Some("low".into()),
            context_window: Some(500000),
            ..Default::default()
        };
        let child = config.resolve(&main, "turn-model").unwrap();
        assert_eq!(child.default_model, "gpt-6.1-sol");
        assert_eq!(child.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(config.input_budget(231424), 475424);
        assert_eq!(main.reasoning_effort.as_deref(), Some("high"));
        assert_ne!(main.default_model, child.default_model);
        let mut invalid = config.clone();
        invalid.context_window = Some(1000);
        assert!(invalid.normalize().is_err());
        let mut empty = ModelSettings {
            model: Some("  ".into()),
            ..Default::default()
        };
        empty.normalize().unwrap();
        assert_eq!(empty, ModelSettings::default());
        let _ = std::fs::remove_dir_all(root);
    }
}
