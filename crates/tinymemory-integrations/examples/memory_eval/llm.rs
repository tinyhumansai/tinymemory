//! An optional model that answers each probe from its pack (`--llm`).
//!
//! The scripted agent's extractive answer only finds lines that share words
//! with the question, so it cannot answer a paraphrase even when the pack
//! holds the fact. A model reading the same pack shows whether the pack is
//! usable, which is what a host cares about.
//!
//! Any OpenAI-compatible chat endpoint works:
//!
//! - `EVAL_LLM_URL`: default `https://openrouter.ai/api/v1`.
//! - `EVAL_LLM_KEY`: default `OPENROUTER_API_KEY`.
//! - `EVAL_LLM_MODEL`: default `openai/gpt-4.1-mini`.
//!
//! It answers at temperature 0 and is scored like the extractive answer.
//! Each answer carries the tokens it took and, where the endpoint reports it
//! (OpenRouter does), its cost, so a run's cost covers the answers too.

use serde_json::{Value, json};

/// Changes when the answer instruction changes, so reports remain comparable.
pub(crate) const PROMPT_VERSION: &str = "source-reconciliation-v2";

/// What the model is told.
const SYSTEM: &str = "Answer the question from all memory sections. When two sources have \
     incompatible values and one has no date, do not assume which is current: name BOTH values \
     and their sources. Example: an undated policy says shipping takes four days; a dated message \
     says operations now says six days. Answer: \"The policy says four days, while operations says \
     six days.\" When a newer dated message explicitly replaces an older dated value, answer \
     with ONLY the new value and omit the old value even if the question asks what changed. \
     Example: May says Alice owns a job; June says the job moved to Bob. Answer: \"Bob owns the \
     job.\" Keep answers short. If no evidence, say unknown.";

/// One answer and what it cost.
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) tokens: u64,
    /// `None` when the endpoint does not price its calls.
    pub(crate) cost_usd: Option<f64>,
}

/// A chat model.
pub(crate) struct Llm {
    client: reqwest::Client,
    url: String,
    key: String,
    /// The model's id.
    pub(crate) model: String,
}

impl Llm {
    /// The model the environment names.
    ///
    /// # Errors
    ///
    /// When neither `EVAL_LLM_KEY` nor `OPENROUTER_API_KEY` is set.
    pub(crate) fn from_env() -> Result<Self, String> {
        let key = std::env::var("EVAL_LLM_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .map_err(|_| "--llm needs EVAL_LLM_KEY or OPENROUTER_API_KEY".to_string())?;
        Ok(Self {
            client: reqwest::Client::new(),
            url: std::env::var("EVAL_LLM_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string())
                .trim_end_matches('/')
                .to_string(),
            key,
            model: std::env::var("EVAL_LLM_MODEL")
                .unwrap_or_else(|_| "openai/gpt-4.1-mini".to_string()),
        })
    }

    /// The model's answer to `question` given `pack`.
    ///
    /// # Errors
    ///
    /// A transport failure, or an answer without text.
    pub(crate) async fn answer(&self, pack: &str, question: &str) -> Result<Answer, String> {
        let body = json!({
            "model": self.model,
            "temperature": 0,
            // Room for a reasoning model's thinking (some, such as GLM 5.3
            // Flash, cannot turn it off) as well as the one-sentence answer.
            "max_tokens": 2000,
            "reasoning": { "effort": "low" },
            // OpenRouter's usage accounting: the call's cost in `usage.cost`.
            "usage": { "include": true },
            "messages": [
                { "role": "system", "content": SYSTEM },
                { "role": "user", "content": format!("{pack}\n\nQuestion: {question}") },
            ],
        });
        let answer: Value = self
            .client
            .post(format!("{}/chat/completions", self.url))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())?;
        let text = answer
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_string())
            .ok_or_else(|| format!("no answer text in {answer}"))?;
        Ok(Answer {
            text,
            tokens: answer
                .pointer("/usage/total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cost_usd: answer.pointer("/usage/cost").and_then(Value::as_f64),
        })
    }
}
