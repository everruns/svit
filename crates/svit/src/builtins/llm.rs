use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};

use super::{Builtin, BuiltinContext, BuiltinManual, BuiltinResult, MAX_TOOL_INPUT_BYTES};
use crate::Reasoner;

impl Reasoner {
    async fn run_once(&self, system_prompt: Option<String>, prompt: String) -> Result<String, ()> {
        let instructions = system_prompt
            .filter(|prompt| !prompt.trim().is_empty())
            .map(|prompt| format!("{prompt}\n\nComplete the supplied task."))
            .unwrap_or_else(|| "Complete the supplied task.".to_string());
        let agent = everruns::Agent::builder()
            .name("svit-builtin-llm")
            .instructions(instructions)
            .model(self.model_id())
            .provider(self.provider().clone())
            .build()
            .map_err(|_| ())?;
        let turn = agent
            .session()
            .send_and_wait(prompt)
            .await
            .map_err(|_| ())?;
        turn.success.then_some(turn.response).ok_or(())
    }
}

pub(super) struct LlmBuiltin {
    reasoner: Reasoner,
    system_prompt: Option<String>,
}

impl LlmBuiltin {
    pub(super) fn new(reasoner: Reasoner) -> Self {
        Self {
            reasoner,
            system_prompt: None,
        }
    }
}

#[async_trait]
impl Builtin for LlmBuiltin {
    fn manual(&self) -> BuiltinManual {
        BuiltinManual::new(
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

    async fn execute(&self, _context: BuiltinContext, arguments: JsonValue) -> BuiltinResult {
        let prompt = arguments["prompt"].as_str().unwrap_or_default();
        if prompt.is_empty() || prompt.len() > MAX_TOOL_INPUT_BYTES {
            return BuiltinResult::error("llm prompt is empty or exceeds its limit");
        }
        // THREAT[TM-EFF-005]: Model calls require an explicit host-selected
        // provider and remain outside Svit transactions.
        match self
            .reasoner
            .run_once(self.system_prompt.clone(), prompt.to_owned())
            .await
        {
            Ok(response) => BuiltinResult::text(response),
            Err(_) => BuiltinResult::error("llm request failed"),
        }
    }
}
