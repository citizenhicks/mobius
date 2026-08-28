//! Stable model route selection and route diagnostics.

use std::sync::Arc;

use super::CompactOutput;
use super::CompactRequest;
use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelPricing;
use super::ModelRequest;
use super::PromptCacheCapability;
use crate::Error;
use crate::Result;
use crate::protocol::ModelChoice;
use crate::protocol::ModelStepDiagnostics;
use crate::protocol::PromptCacheDiagnostics;
use crate::protocol::TokenUsage;
use crate::protocol::ToolDiscoveryMode;

/// Selects a model Adapter by a stable provider ID.
pub struct ModelRouter {
    default: String,
    routes: Vec<ModelRoute>,
}

struct ModelRoute {
    choice: ModelChoice,
    provider: Arc<dyn Model>,
}

impl ModelRouter {
    /// Creates a router with its first provider.
    pub fn new(id: impl Into<String>, provider: Arc<dyn Model>) -> Self {
        let id = id.into();
        let choice = inferred_choice(&id, provider.as_ref());
        Self {
            default: id,
            routes: vec![ModelRoute { choice, provider }],
        }
    }

    /// Registers another provider.
    pub fn register(&mut self, id: impl Into<String>, provider: Arc<dyn Model>) -> Result<()> {
        let id = id.into();
        if self.routes.iter().any(|route| route.choice.route == id) {
            return Err(Error::Duplicate(format!("model provider `{id}`")));
        }
        self.routes.push(ModelRoute {
            choice: inferred_choice(&id, provider.as_ref()),
            provider,
        });
        Ok(())
    }

    /// Returns the selectable routes in frontend display order.
    #[must_use]
    pub fn choices(
        &self,
    ) -> impl DoubleEndedIterator<Item = &ModelChoice> + ExactSizeIterator + Clone {
        self.routes.iter().map(|route| &route.choice)
    }

    /// Resolves one route and optional reasoning effort through the model catalog.
    pub fn resolve_choice(
        &self,
        route: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<&ModelChoice> {
        let choice = self
            .choices()
            .find(|choice| choice.route == route)
            .ok_or_else(|| Error::Unknown(format!("model route `{route}`")))?;
        let Some(reasoning_effort) = reasoning_effort else {
            return Ok(choice);
        };
        self.choices()
            .find(|candidate| {
                candidate.group == choice.group
                    && candidate.reasoning_effort.as_deref() == Some(reasoning_effort)
            })
            .ok_or_else(|| {
                Error::Unknown(format!(
                    "reasoning effort `{reasoning_effort}` for model route `{route}`"
                ))
            })
    }

    /// Replaces display metadata for one registered route.
    pub fn configure_choice(&mut self, mut choice: ModelChoice) -> Result<()> {
        if choice.group.trim().is_empty() || choice.model.trim().is_empty() {
            return Err(Error::Config(
                "model choice group and model cannot be empty".into(),
            ));
        }
        if choice.context_window.is_some_and(|window| window <= 0) {
            return Err(Error::Config(
                "model choice context window must be positive".into(),
            ));
        }
        let current = self
            .routes
            .iter_mut()
            .find(|current| current.choice.route == choice.route)
            .ok_or_else(|| Error::Unknown(format!("model route `{}`", choice.route)))?;
        choice.supports_image_input = current.provider.supports_image_input();
        choice.tool_discovery = current.provider.tool_discovery();
        current.choice = choice;
        Ok(())
    }

    /// Returns the default provider ID.
    #[must_use]
    pub fn default_provider(&self) -> &str {
        &self.default
    }

    /// Streams one response through the selected provider.
    pub async fn respond(
        &self,
        provider: &str,
        request: ModelRequest<'_>,
        events: ModelEventSink,
    ) -> Result<ModelOutput> {
        self.provider(provider)?.respond(request, events).await
    }

    /// Reports whether one route has a native compaction endpoint.
    pub fn compaction_endpoint(&self, provider: &str) -> Result<bool> {
        Ok(self.provider(provider)?.compaction_endpoint())
    }

    /// Reports whether one route accepts native image input.
    pub fn supports_image_input(&self, provider: &str) -> Result<bool> {
        Ok(self.provider(provider)?.supports_image_input())
    }

    /// Reports deferred-tool cache behavior for one route.
    pub fn tool_discovery(&self, provider: &str) -> Result<ToolDiscoveryMode> {
        Ok(self.provider(provider)?.tool_discovery())
    }

    /// Reports prompt-cache support for one route.
    pub fn prompt_cache_capability(&self, provider: &str) -> Result<PromptCacheCapability> {
        Ok(self.provider(provider)?.prompt_cache_capability())
    }

    /// Returns provider-owned pricing for one route when it is known.
    pub fn pricing(&self, provider: &str) -> Result<Option<ModelPricing>> {
        Ok(self.provider(provider)?.pricing())
    }

    /// Estimates one completed request from provider-owned rates.
    pub fn estimated_cost_microusd(
        &self,
        provider: &str,
        usage: &TokenUsage,
    ) -> Result<Option<u64>> {
        Ok(self
            .provider(provider)?
            .pricing()
            .and_then(|pricing| pricing.estimate_microusd(usage)))
    }

    pub(crate) fn model_step_diagnostics(
        &self,
        provider: &str,
        context_epoch: u64,
        rewrite_reasons: Vec<String>,
        usage: &TokenUsage,
    ) -> Result<ModelStepDiagnostics> {
        let model = self.provider(provider)?;
        let capability = model.prompt_cache_capability();
        Ok(ModelStepDiagnostics {
            provider: provider.into(),
            prompt_cache: PromptCacheDiagnostics {
                capability: capability.mode(),
                context_epoch,
                outcome: capability.outcome(usage, !rewrite_reasons.is_empty()),
                rewrite_reasons,
            },
            estimated_cost_microusd: model
                .pricing()
                .and_then(|pricing| pricing.estimate_microusd(usage)),
        })
    }

    /// Compacts context through the selected provider.
    pub async fn compact(
        &self,
        provider: &str,
        request: CompactRequest<'_>,
    ) -> Result<CompactOutput> {
        self.provider(provider)?.compact(request).await
    }

    fn provider(&self, id: &str) -> Result<&dyn Model> {
        self.routes
            .iter()
            .find(|route| route.choice.route == id)
            .map(|route| route.provider.as_ref())
            .ok_or_else(|| Error::Unknown(format!("model provider `{id}`")))
    }
}

fn inferred_choice(route: &str, provider: &dyn Model) -> ModelChoice {
    let mut info = provider.info();
    if info.model.is_empty() {
        info.model = route.to_string();
    }
    ModelChoice {
        route: route.to_string(),
        group: route.to_string(),
        model: info.model,
        reasoning_effort: info.reasoning_effort,
        context_window: None,
        supports_image_input: provider.supports_image_input(),
        tool_discovery: provider.tool_discovery(),
    }
}
