//! Minimal structured-output client for the simulated user and the judge.
//!
//! Both run on OpenAI's Responses API with a strict JSON schema, separate
//! from the model under test, so the grader never shares a provider quirk or
//! a context window with the subject.

use std::time::Duration;

use serde_json::{Value as Json, json};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

#[derive(Clone)]
pub struct Llm {
    http: reqwest::Client,
    api_key: String,
    pub model: String,
    effort: String,
}

impl Llm {
    /// `None` when `OPENAI_API_KEY` is unset; callers report N/A or infra.
    pub fn from_env(model_env: &str, default_model: &str, effort: &str) -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        let model = std::env::var(model_env)
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| default_model.to_owned());
        Some(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(240))
                .build()
                .ok()?,
            api_key,
            model,
            effort: effort.to_owned(),
        })
    }

    /// One schema-constrained completion, retried on transient failures.
    pub async fn json(
        &self,
        instructions: &str,
        input: &str,
        name: &str,
        schema: Json,
    ) -> Result<Json, String> {
        let body = json!({
            "model": self.model,
            "instructions": instructions,
            "input": input,
            "reasoning": {"effort": self.effort},
            "store": false,
            "text": {"format": {"type": "json_schema", "name": name, "strict": true, "schema": schema}},
        });
        let mut last_error = String::new();
        for attempt in 0..5u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
            let response = match self
                .http
                .post(RESPONSES_URL)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_error = format!("request failed: {error}");
                    continue;
                }
            };
            let status = response.status();
            let payload: Json = match response.json().await {
                Ok(payload) => payload,
                Err(error) => {
                    last_error = format!("bad response body: {error}");
                    continue;
                }
            };
            if status.as_u16() == 429 || status.is_server_error() {
                last_error = format!("{status}: {}", payload["error"]["message"]);
                continue;
            }
            if !status.is_success() {
                return Err(format!("{status}: {}", payload["error"]["message"]));
            }
            let text = payload["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "message")
                .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
                .filter_map(|part| part["text"].as_str().map(str::to_owned))
                .collect::<String>();
            match serde_json::from_str(&text) {
                Ok(value) => return Ok(value),
                Err(error) => last_error = format!("unparseable structured output: {error}"),
            }
        }
        Err(last_error)
    }
}
