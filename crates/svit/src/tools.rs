use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use everruns_core::{Tool, ToolExecutionResult};
use serde_json::Value;

type Handler =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = ToolExecutionResult> + Send>> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct FunctionTool {
    name: String,
    description: String,
    parameters: Value,
    handler: Handler,
}

impl FunctionTool {
    pub(crate) fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolExecutionResult> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: Arc::new(move |arguments| Box::pin(handler(arguments))),
        }
    }
}

impl fmt::Debug for FunctionTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FunctionTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for FunctionTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        (self.handler)(arguments).await
    }
}

pub(crate) fn tool_json(value: Value) -> ToolExecutionResult {
    ToolExecutionResult::success(value)
}

pub(crate) fn tool_error(message: impl Into<String>) -> ToolExecutionResult {
    ToolExecutionResult::tool_error(message)
}
