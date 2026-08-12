use everruns::{ModelSpec, Provider};

/// Host-selected model and provider used to reason about one Svit process.
///
/// Keeping these values together prevents a Svit builder from representing a
/// model without the provider that serves it.
///
/// ```
/// use svit::{LLMSIM_MODEL_ID, LlmSimConfig, Reasoner, llm_sim_provider};
///
/// let reasoner = Reasoner::new(
///     LLMSIM_MODEL_ID,
///     llm_sim_provider(LlmSimConfig::fixed("done")),
/// );
/// assert_eq!(reasoner.model_id(), LLMSIM_MODEL_ID);
/// ```
#[derive(Clone)]
pub struct Reasoner {
    model: String,
    provider: Provider,
}

impl Reasoner {
    /// Creates a reasoner from one provider-visible model ID and provider.
    pub fn new(model: impl Into<String>, provider: impl Into<Provider>) -> Self {
        Self {
            model: model.into(),
            provider: provider.into(),
        }
    }

    /// Returns the provider-visible model ID.
    pub fn model_id(&self) -> &str {
        &self.model
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    pub(crate) fn model_spec(&self) -> ModelSpec {
        ModelSpec::on(self.provider.id().clone(), self.model.clone())
    }
}
