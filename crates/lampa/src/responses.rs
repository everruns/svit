use agentyk::{
    ChatDriver, ChatRequest, ChatResponse, ContentPart, DriverId, LlmErrorKind, Message, Role,
    ToolCall, Usage,
};
use async_trait::async_trait;
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OUTPUT_METADATA_KEY: &str = "openai.responses.output";

/// OpenAI Responses adapter for Lampa's reasoning and function-tool workflow.
pub(crate) struct OpenAiResponsesDriver {
    client: reqwest::Client,
}

impl OpenAiResponsesDriver {
    pub(crate) fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn build_body(request: &ChatRequest) -> Value {
        let mut input = Vec::new();
        for message in &request.messages {
            if message.role == Role::Assistant
                && let Some(output) = message.metadata[OUTPUT_METADATA_KEY].as_array()
            {
                input.extend(output.iter().cloned());
                continue;
            }
            match message.role {
                Role::Tool => input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.text(),
                })),
                Role::User | Role::Assistant | Role::System => {
                    let role = match message.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                        Role::Tool => unreachable!(),
                    };
                    if !message.content.is_empty() {
                        input.push(json!({
                            "role": role,
                            "content": response_content(&message.content),
                        }));
                    }
                    input.extend(message.tool_calls.iter().map(|call| {
                        json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        })
                    }));
                }
            }
        }

        let mut body = json!({
            "model": request.model.model,
            "input": input,
            "store": false,
        });
        if let Some(instructions) = &request.system_prompt {
            body["instructions"] = json!(instructions);
        }
        if let Some(effort) = request
            .model
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort.as_deref())
        {
            body["reasoning"] = json!({"effort": effort});
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        })
                    })
                    .collect(),
            );
        }
        body
    }

    fn parse_response(body: Value) -> agentyk::Result<ChatResponse> {
        let output = body
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| driver_error(LlmErrorKind::Unknown, "response has no output array"))?;
        let mut text = String::new();
        let mut calls = Vec::new();
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(part_text) = part.get("text").and_then(Value::as_str)
                        {
                            text.push_str(part_text);
                        }
                    }
                }
                Some("function_call") => calls.push(ToolCall {
                    id: string_field(item, "call_id")?,
                    name: string_field(item, "name")?,
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_else(|| json!({})),
                }),
                _ => {}
            }
        }

        let usage = body.get("usage").cloned().unwrap_or_else(|| json!({}));
        let input_tokens = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached_tokens = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning_tokens = usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let message = Message::assistant_with_calls(text, calls)
            .with_metadata(json!({(OUTPUT_METADATA_KEY): output}));
        Ok(ChatResponse::new(
            message,
            Usage::new(input_tokens, output_tokens)
                .with_cache(cached_tokens, 0)
                .with_reasoning(reasoning_tokens),
        ))
    }
}

fn response_content(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({"type": "input_text", "text": text.text}),
            ContentPart::Image(image) => {
                let url = image.url.clone().unwrap_or_else(|| {
                    format!(
                        "data:{};base64,{}",
                        image.media_type.as_deref().unwrap_or("image/png"),
                        image.base64.as_deref().unwrap_or_default()
                    )
                });
                json!({"type": "input_image", "image_url": url})
            }
        })
        .collect()
}

fn string_field(value: &Value, field: &str) -> agentyk::Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            driver_error(
                LlmErrorKind::Unknown,
                format!("function call has no {field}"),
            )
        })
}

fn driver_error(kind: LlmErrorKind, message: impl Into<String>) -> agentyk::Error {
    agentyk::Error::driver(kind, message)
}

fn classify_status(status: reqwest::StatusCode) -> LlmErrorKind {
    match status.as_u16() {
        401 | 403 => LlmErrorKind::Authentication,
        400 | 404 | 409 | 422 => LlmErrorKind::InvalidRequest,
        429 => LlmErrorKind::RateLimited,
        503 => LlmErrorKind::Overloaded,
        500..=599 => LlmErrorKind::ServerError,
        _ => LlmErrorKind::Unknown,
    }
}

#[async_trait]
impl ChatDriver for OpenAiResponsesDriver {
    fn id(&self) -> DriverId {
        DriverId::openai()
    }

    async fn complete(&self, request: ChatRequest) -> agentyk::Result<ChatResponse> {
        let key = request.model.api_key.as_deref().ok_or_else(|| {
            driver_error(
                LlmErrorKind::Authentication,
                "OpenAI Responses API key is required",
            )
        })?;
        let base_url = request
            .model
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let mut builder = self
            .client
            .post(format!("{base_url}/responses"))
            .bearer_auth(key)
            .json(&Self::build_body(&request));
        if let Some(organization) = request.model.metadata["organization"].as_str() {
            builder = builder.header("OpenAI-Organization", organization);
        }
        if let Some(project) = request.model.metadata["project"].as_str() {
            builder = builder.header("OpenAI-Project", project);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| driver_error(LlmErrorKind::Network, error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| driver_error(LlmErrorKind::Network, error.to_string()))?;
        if !status.is_success() {
            return Err(driver_error(
                classify_status(status),
                format!("OpenAI Responses HTTP {status}: {body}"),
            ));
        }
        let body = serde_json::from_str(&body)
            .map_err(|error| driver_error(LlmErrorKind::Unknown, error.to_string()))?;
        Self::parse_response(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::{ModelSpec, ToolDefinition};

    #[test]
    fn request_uses_responses_reasoning_and_flat_function_tools() {
        let request = ChatRequest::new(
            ModelSpec::openai("gpt-test").reasoning_effort("medium"),
            vec![Message::user("remember blue")],
        )
        .system_prompt("own the process")
        .tools(vec![ToolDefinition::new(
            "read",
            "read memory",
            json!({"type": "object"}),
        )]);

        let body = OpenAiResponsesDriver::build_body(&request);

        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("function").is_none());
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn response_maps_text_function_calls_and_usage() {
        let response = OpenAiResponsesDriver::parse_response(json!({
            "output": [
                {"type": "message", "content": [
                    {"type": "output_text", "text": "working"}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "read",
                 "arguments": "{\"path\":\"/memory\"}"}
            ],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 8,
                "input_tokens_details": {"cached_tokens": 3},
                "output_tokens_details": {"reasoning_tokens": 4}
            }
        }))
        .unwrap();

        assert_eq!(response.message.text(), "working");
        assert_eq!(response.message.tool_calls[0].name, "read");
        assert_eq!(response.message.tool_calls[0].arguments["path"], "/memory");
        assert_eq!(response.usage.reasoning_tokens, 4);
    }

    #[test]
    fn response_output_items_round_trip_for_tool_continuation() {
        let response = OpenAiResponsesDriver::parse_response(json!({
            "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"},
                {"type": "function_call", "call_id": "call_1", "name": "read",
                 "arguments": "{\"path\":\"/memory\"}"}
            ]
        }))
        .unwrap();
        let request = ChatRequest::new(
            ModelSpec::openai("gpt-test"),
            vec![response.message, Message::tool_result("call_1", "{}")],
        );

        let body = OpenAiResponsesDriver::build_body(&request);

        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], "opaque");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
    }
}
