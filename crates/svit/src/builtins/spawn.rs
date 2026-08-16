use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};

use super::{MAX_PORT_INPUT_BYTES, Port, PortContext, PortDescriptor, PortResult};
use crate::{Message, ProcessId, Reasoner};

#[derive(Clone)]
enum ChildEntry {
    Starting,
    Ready(Vec<u8>),
}

#[derive(Clone, Default)]
pub(super) struct ChildRegistry {
    children: Arc<Mutex<BTreeMap<ProcessId, ChildEntry>>>,
}

impl ChildRegistry {
    pub(super) fn ids(&self) -> Vec<ProcessId> {
        self.children
            .lock()
            .map(|children| {
                children
                    .iter()
                    .filter(|(_, child)| matches!(child, ChildEntry::Ready(_)))
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn snapshot(&self, id: &ProcessId) -> crate::Result<Option<Vec<u8>>> {
        Ok(self
            .children
            .lock()
            .ok()
            .and_then(|children| match children.get(id) {
                Some(ChildEntry::Ready(snapshot)) => Some(snapshot.clone()),
                _ => None,
            }))
    }
}

pub(super) struct SpawnPort {
    reasoner: Reasoner,
    registry: ChildRegistry,
}

impl SpawnPort {
    pub(super) fn new(reasoner: Reasoner, registry: ChildRegistry) -> Self {
        Self { reasoner, registry }
    }

    fn fail(&self, child_id: &ProcessId, message: &'static str) -> PortResult {
        if let Ok(mut children) = self.registry.children.lock()
            && matches!(children.get(child_id), Some(ChildEntry::Starting))
        {
            children.remove(child_id);
        }
        PortResult::error(message)
    }
}

#[async_trait]
impl Port for SpawnPort {
    fn descriptor(&self) -> PortDescriptor {
        PortDescriptor::new(
            "Fork the committed process, run one child Svit turn, and retain its snapshot.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "task": {"type": "string"}},
                "required": ["id", "task"]
            }),
        )
        .effect("child-process")
        .output("JSON object containing the child address and completed assistant response.")
        .limits([
            "One turn per unique local child address.",
            "256 KiB task.",
            "Child registry is local and non-durable.",
        ])
    }

    async fn execute(&self, context: PortContext, arguments: JsonValue) -> PortResult {
        let child_id = match arguments["id"]
            .as_str()
            .and_then(|id| ProcessId::new(id).ok())
        {
            Some(id) => id,
            None => return PortResult::error("invalid child address"),
        };
        let task = arguments["task"].as_str().unwrap_or_default();
        if task.is_empty() || task.len() > MAX_PORT_INPUT_BYTES {
            return PortResult::error("spawn task is empty or exceeds its limit");
        }
        {
            let Ok(mut children) = self.registry.children.lock() else {
                return PortResult::error("child registry unavailable");
            };
            if children.contains_key(&child_id) {
                return PortResult::error("child address already exists");
            }
            children.insert(child_id.clone(), ChildEntry::Starting);
        }
        let child_process = match context.process().lock() {
            Ok(parent) => match parent.fork(child_id.as_str()) {
                Ok(child) => child,
                Err(_) => return self.fail(&child_id, "child fork failed"),
            },
            Err(_) => return self.fail(&child_id, "parent unavailable"),
        };

        // THREAT[TM-FORK-002]: A child starts from Process::fork. Model
        // authority is supplied separately and never inferred from state.
        let builder = crate::Svit::resume(child_process).reasoner(self.reasoner.clone());
        let mut child = match builder.build().await {
            Ok(child) => child,
            Err(_) => return self.fail(&child_id, "child construction failed"),
        };
        let inbox = child.inbox();
        let mut outbox = child.outbox();
        if child.start().is_err() || inbox.send(Message::user(task.to_owned())).await.is_err() {
            return self.fail(&child_id, "child start failed");
        }
        drop(inbox);
        if child.block().await.is_err() {
            return self.fail(&child_id, "child turn failed");
        }
        let response = match outbox.try_recv() {
            Ok(response) => response,
            Err(_) => return self.fail(&child_id, "child produced no response"),
        };
        let snapshot = match child.snapshot() {
            Ok(snapshot) => snapshot,
            Err(_) => return self.fail(&child_id, "child snapshot failed"),
        };
        match self.registry.children.lock() {
            Ok(mut children) if matches!(children.get(&child_id), Some(ChildEntry::Starting)) => {
                children.insert(child_id.clone(), ChildEntry::Ready(snapshot));
            }
            _ => return PortResult::error("child address already exists"),
        }
        PortResult::text(
            json!({"id": child_id.as_str(), "response": response.text().unwrap_or_default()})
                .to_string(),
        )
    }
}
