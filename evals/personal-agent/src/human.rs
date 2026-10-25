//! The simulated human on the other end of the chat.
//!
//! A personal agent legitimately stops to ask a clarifying question or to
//! confirm a costly action. A benchmark that ends the case at that point
//! measures nothing, so every agent reply goes to an LLM playing the user.
//! The simulator knows the user's goal and private facts the world does not
//! hold, answers like a busy person would, approves actions that match the
//! goal, rejects ones that do not, and ends the conversation once the agent
//! reports the task finished or is clearly stuck.

use serde_json::{Value as Json, json};

use crate::llm::Llm;

const INSTRUCTIONS: &str = r#"You are role-playing a real person chatting with their AI personal assistant. You are NOT an assistant. Stay in character for the whole conversation.

How to behave:
- Write like a busy person texting: short, natural, one to three sentences. No lists, no markdown.
- You know your goal and your private facts below. Use them to answer questions the assistant asks. Do not volunteer private facts unless they are asked for, or unless the assistant is about to do something that clearly contradicts them (a real person would object).
- If the assistant asks about something your facts do not cover, answer the way the persona plausibly would, or say you have no preference. Never invent facts that conflict with your private facts.
- When the assistant asks you to confirm an action, approve it only if it matches your goal and facts. If it does not, say no and say what is wrong in plain words. Do not approve a wrong amount, recipient, date, or item just to move things along.
- Never do the assistant's work for it: do not look things up, compute totals, or draft text for it. If it asks you for information that a competent assistant could find in your email, calendar, files, or accounts, tell it to check there itself.
- Follow the scenario notes exactly when they say how to react to something.
- Do not invent new requirements, doubts, or research tasks that your goal, facts, and notes do not support. If the assistant's proposal meets your goal and facts, approve it; do not second-guess information the assistant found in your services.
- Set done=true when the assistant says it has finished (or delivered what you asked for), when it says it cannot do the task, or when the conversation is going in circles. When done=true, message may be a brief sign-off. Otherwise done=false and message is your next reply.
- Do not end the conversation just because you are unhappy with an answer; push back once or twice first."#;

pub struct UserSimulator {
    llm: Llm,
    brief: String,
}

#[derive(Debug)]
pub struct SimTurn {
    pub message: String,
    pub done: bool,
}

impl UserSimulator {
    pub fn new(llm: Llm, user: &Json, world: &Json) -> Self {
        let name = world["user"]["name"].as_str().unwrap_or("the user");
        let persona = user["persona"]
            .as_str()
            .or(world["user"]["persona"].as_str())
            .unwrap_or("");
        let facts = user["knows"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Json::as_str)
                    .map(|fact| format!("- {fact}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let brief = format!(
            "You are {name}. {persona}\n\nYour goal in this conversation:\n{}\n\nPrivate facts you know:\n{}\n\nScenario notes:\n{}",
            user["goal"].as_str().unwrap_or(""),
            if facts.is_empty() {
                "- (none)".to_owned()
            } else {
                facts
            },
            user["behavior"].as_str().unwrap_or("(none)"),
        );
        Self { llm, brief }
    }

    pub fn model(&self) -> &str {
        &self.llm.model
    }

    /// Produces the user's next turn given the conversation so far, which
    /// ends with the assistant's latest reply.
    pub async fn respond(&self, conversation: &[(String, String)]) -> Result<SimTurn, String> {
        let transcript = conversation
            .iter()
            .map(|(role, text)| {
                let speaker = if role == "user" { "YOU" } else { "ASSISTANT" };
                format!("{speaker}: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let input = format!(
            "{}\n\n=== Conversation so far ===\n{transcript}\n\n=== Your turn ===\nReply as yourself to the assistant's last message.",
            self.brief
        );
        let schema = json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "done": {"type": "boolean"}
            },
            "required": ["message", "done"],
            "additionalProperties": false
        });
        let value = self
            .llm
            .json(INSTRUCTIONS, &input, "user_turn", schema)
            .await?;
        Ok(SimTurn {
            message: value["message"].as_str().unwrap_or("").to_owned(),
            done: value["done"].as_bool().unwrap_or(true),
        })
    }
}
