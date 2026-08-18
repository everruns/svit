use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};

use super::{MAX_PORT_INPUT_BYTES, Port, PortContext, PortDescriptor, PortResult};
use crate::Reasoner;

impl Reasoner {
    async fn run_once(&self, system_prompt: Option<String>, prompt: String) -> Result<String, ()> {
        let instructions = system_prompt
            .filter(|prompt| !prompt.trim().is_empty())
            .map(|prompt| format!("{prompt}\n\nComplete the supplied task."))
            .unwrap_or_else(|| "Complete the supplied task.".to_string());
        let agent = everruns::Agent::builder()
            .name("svit-port-llm")
            .instructions(instructions)
            .model(self.model_id())
            .provider(self.provider().clone())
            .build()
            .map_err(|_| ())?;
        let turn = everruns::Engine::new()
            .create(agent)
            .send_and_wait(prompt)
            .await
            .map_err(|_| ())?;
        turn.success.then_some(turn.response).ok_or(())
    }
}

pub(super) struct LlmPort {
    reasoner: Reasoner,
    system_prompt: Option<String>,
}

impl LlmPort {
    pub(super) fn new(reasoner: Reasoner) -> Self {
        Self {
            reasoner,
            system_prompt: None,
        }
    }
}

#[async_trait]
impl Port for LlmPort {
    fn descriptor(&self) -> PortDescriptor {
        PortDescriptor::new(
            "Call the host-selected nested model with one prompt.",
            json!({
                "type": "object",
                "properties": {"prompt": {"type": "string"}},
                "required": ["prompt"]
            }),
        )
        .effect("external")
        .output("Text returned by the host-selected nested model.")
        .limits(["256 KiB prompt.", "Host model and provider limits apply."])
    }

    async fn execute(&self, _context: PortContext, arguments: JsonValue) -> PortResult {
        let prompt = arguments["prompt"].as_str().unwrap_or_default();
        if prompt.is_empty() || prompt.len() > MAX_PORT_INPUT_BYTES {
            return PortResult::error("llm prompt is empty or exceeds its limit");
        }
        // THREAT[TM-EFF-005]: Model calls require an explicit host-selected
        // provider and remain outside Svit transactions.
        match self
            .reasoner
            .run_once(self.system_prompt.clone(), prompt.to_owned())
            .await
        {
            Ok(response) => PortResult::text(response),
            Err(_) => PortResult::error("llm request failed"),
        }
    }
}
