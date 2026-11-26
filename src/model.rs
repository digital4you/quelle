use std::time::Duration;

use reqwest::{Client, header::HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::{Failure, read_json};

const CONTEXT_LIMIT: usize = 64_000;
const TEMPLATE_RESERVE: usize = 256;

pub struct Model {
    pub client: Client,
    pub completions: Url,
    pub tokenize: Url,
    pub name: String,
    pub authorization: HeaderValue,
    pub context_limit: usize,
}

#[derive(Clone, Serialize)]
pub struct Message {
    role: &'static str,
    content: String,
}

impl Message {
    pub fn exchange(system: String, data: Value) -> [Self; 2] {
        [
            Self {
                role: "system",
                content: system,
            },
            Self {
                role: "user",
                content: data.to_string(),
            },
        ]
    }

    /// The same exchange with a correction appended to the system turn.
    pub fn corrected(messages: &[Self; 2], note: &str) -> [Self; 2] {
        let mut messages = messages.clone();
        messages[0].content = format!("{}\n\n{note}", messages[0].content);
        messages
    }
}

#[derive(Deserialize)]
struct TokenCount {
    count: usize,
    max_model_len: usize,
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: CompletionMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct CompletionMessage {
    content: Option<String>,
}

/// Prompt size against the space left after the reserved output.
pub struct Fit {
    pub input: usize,
    pub available: usize,
}

impl Fit {
    pub fn fits(&self) -> bool {
        self.available > 0 && self.input <= self.available
    }
}

impl Model {
    pub async fn measure(&self, messages: &[Message], output: usize) -> Result<Fit, Failure> {
        let response = self
            .client
            .post(self.tokenize.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .timeout(Duration::from_secs(5))
            .json(
                &json!({"model": self.name, "messages": messages, "add_generation_prompt": true,
                "chat_template_kwargs": {"enable_thinking": false}}),
            )
            .send()
            .await
            .map_err(|_| Failure::TokenizerUnavailable)?;
        if !response.status().is_success() {
            return Err(Failure::TokenizerUnavailable);
        }
        let token_count: TokenCount = read_json(response, 64 * 1024)
            .await
            .map_err(|_| Failure::TokenizerUnavailable)?;
        if token_count.max_model_len == 0 {
            return Err(Failure::TokenizerUnavailable);
        }
        let limit = token_count
            .max_model_len
            .min(self.context_limit)
            .min(CONTEXT_LIMIT);
        tracing::info!(
            stage = "tokenize",
            input_tokens = token_count.count,
            context_limit = limit,
            output_tokens = output
        );
        Ok(Fit {
            input: token_count.count,
            available: limit.saturating_sub(output + TEMPLATE_RESERVE),
        })
    }

    pub async fn fits(&self, messages: &[Message], output: usize) -> Result<bool, Failure> {
        Ok(self.measure(messages, output).await?.fits())
    }

    pub async fn complete(
        &self,
        messages: &[Message],
        output: usize,
        schema: Option<(&str, Value)>,
    ) -> Result<String, Failure> {
        let mut payload = json!({"model": self.name, "messages": messages, "temperature": 0.1,
            "max_completion_tokens": output, "stream": false,
            "chat_template_kwargs": {"enable_thinking": false}});
        if let Some((name, schema)) = schema {
            payload["response_format"] = json!({"type":"json_schema", "json_schema": {"name":name, "strict":true, "schema":schema}});
        }
        let response = self
            .client
            .post(self.completions.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .timeout(Duration::from_secs(55))
            .json(&payload)
            .send()
            .await
            .map_err(|_| Failure::ModelUnavailable)?;
        if !response.status().is_success() {
            return Err(Failure::ModelUnavailable);
        }
        let body: Completion = read_json(response, 256 * 1024)
            .await
            .map_err(|_| Failure::ModelUnavailable)?;
        body.choices
            .into_iter()
            .next()
            .filter(|choice| choice.finish_reason == "stop")
            .and_then(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .ok_or(Failure::IncompleteModelResponse)
    }
}
