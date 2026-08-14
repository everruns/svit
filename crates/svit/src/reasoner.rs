use everruns::{ModelSpec, Provider};

/// Host-selected model and provider used to reason about one Svit process.
///
/// Keeping these values together prevents a Svit builder from representing a
/// model without the provider that serves it.
///
/// ```no_run
/// use svit::{OpenAI, Reasoner};
///
/// let reasoner = Reasoner::new(
///     "gpt-5.6-terra",
///     OpenAI::from_env()?,
/// );
/// assert_eq!(reasoner.model_id(), "gpt-5.6-terra");
/// # Ok::<(), Box<dyn std::error::Error>>(())
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
