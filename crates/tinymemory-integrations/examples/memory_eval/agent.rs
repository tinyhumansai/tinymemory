//! A deliberately tiny scripted agent.
//!
//! It has no model. Each user turn runs the real lifecycle calls, so
//! everything the memory layer does is the same as for a real agent:
//!
//! 1. `pre_turn` logs the user's text and recalls a context pack, leaving
//!    out the turns still in its prompt window.
//! 2. The agent "runs" the scripted tool calls and copies their results into
//!    its reply. Memory keeps only a call's name and id, so a result the
//!    reply leaves out is lost.
//! 3. It answers a question with the pack line that shares the most words
//!    with it ([`answer`]), and otherwise acknowledges.
//! 4. `post_turn` logs the reply with its tool calls.
//!
//! The extractive answer is a stand-in for a model reading the pack. It
//! scores what a model would see, not how well some model reasons.

use std::collections::HashSet;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use tinymemory_api::{Error, ToolCallRef};
use tinymemory_tools::lifecycle::turn::{LoggedToolCall, logged_reply};
use tinymemory_tools::{AgentMemory, BackgroundJob, PostTurn, PreTurn, TurnContext};

/// OpenHuman's default deadline for the pack before a model turn.
pub(crate) const PRE_TURN_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// The host selects one lifecycle hook from its recall configuration and
/// whether the thread is resuming after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostHook {
    Plain,
    Resumed,
    Dated,
    DatedResumed,
}

impl HostHook {
    pub(crate) fn for_turn(resumed: bool, date_hint: bool) -> Self {
        match (resumed, date_hint) {
            (false, false) => Self::Plain,
            (true, false) => Self::Resumed,
            (false, true) => Self::Dated,
            (true, true) => Self::DatedResumed,
        }
    }

    pub(crate) async fn run(self, memory: AgentMemory, pre: PreTurn) -> Result<TurnContext, Error> {
        match self {
            Self::Plain => memory.pre_turn(pre).await,
            Self::Resumed => memory.pre_turn_resumed(pre).await,
            Self::Dated => memory.pre_turn_dated(pre, false, async { None }).await,
            Self::DatedResumed => memory.pre_turn_dated(pre, true, async { None }).await,
        }
    }
}
/// OpenHuman's maximum length of one logged tool result.
/// A scripted tool call and the result the "tool" returns.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolStep {
    /// The tool's name.
    pub(crate) name: &'static str,
    /// What it returned.
    pub(crate) result: &'static str,
}

/// What one scripted turn did and how long each step took.
#[derive(Debug, Clone)]
pub(crate) struct TurnRecord {
    /// `pre_turn` latency, in milliseconds.
    pub(crate) pre_ms: f64,
    /// `post_turn` latency, in milliseconds.
    pub(crate) post_ms: f64,
    /// Whether the user turn was logged.
    pub(crate) logged: bool,
    /// Whether OpenHuman would have continued with an empty pack.
    pub(crate) timed_out: bool,
    /// Tokens in the pack injected into the simulated model prompt.
    pub(crate) pack_tokens: usize,
    /// Repeated bullet lines in the injected pack.
    pub(crate) duplicate_lines: usize,
    /// Bullet lines in the injected pack.
    pub(crate) pack_lines: usize,
    /// The jobs `post_turn` handed back.
    pub(crate) jobs: Vec<BackgroundJob>,
    /// How many tool calls the reply made.
    pub(crate) tool_calls: usize,
}

type PendingPreTurn = (
    Instant,
    tokio::task::JoinHandle<(Result<TurnContext, Error>, Instant)>,
);

/// One conversation thread driven by the script.
pub(crate) struct ScriptedAgent {
    memory: AgentMemory,
    thread: String,
    next: u32,
    /// How many of the thread's turns stay in the prompt.
    window: u32,
    clock: Option<DateTime<Utc>>,
    openhuman: bool,
    date_hint: bool,
    pending: Vec<PendingPreTurn>,
}

/// User turns that completed after the simulated host stopped waiting.
pub(crate) struct FlushReport {
    pub(crate) logged: usize,
    pub(crate) completion_ms: Vec<f64>,
}

impl ScriptedAgent {
    /// A new thread for `memory`, keeping the last `window` turns in the
    /// prompt.
    pub(crate) fn new(memory: AgentMemory, thread: impl Into<String>, window: u32) -> Self {
        Self {
            memory,
            thread: thread.into(),
            next: 0,
            window,
            clock: None,
            openhuman: false,
            date_hint: false,
            pending: Vec::new(),
        }
    }

    /// Mirror OpenHuman's default hook, deadline, and reply logging.
    pub(crate) fn openhuman(mut self) -> Self {
        self.openhuman = true;
        self
    }

    /// Exercise the host's optional dated-recall path.
    pub(crate) fn date_hint(mut self) -> Self {
        self.date_hint = true;
        self
    }

    /// Wait for pre-turn tasks whose host deadline expired. OpenHuman leaves
    /// those tasks running, so their accepted user turns may still land.
    pub(crate) async fn flush(&mut self) -> Result<FlushReport, Error> {
        let mut logged = 0;
        let mut completion_ms = Vec::new();
        for (started, task) in self.pending.drain(..) {
            let (context, completed) = task
                .await
                .map_err(|error| Error::Unavailable(format!("pre-turn task failed: {error}")))?;
            let context = context?;
            logged += usize::from(context.logged.is_some());
            completion_ms.push(completed.duration_since(started).as_secs_f64() * 1_000.0);
        }
        Ok(FlushReport {
            logged,
            completion_ms,
        })
    }

    /// Timestamps the thread's turns from `at`, a minute apart.
    pub(crate) fn at(mut self, at: DateTime<Utc>) -> Self {
        self.clock = Some(at);
        self
    }

    /// The first turn index still in the prompt.
    fn in_prompt_from(&self) -> u32 {
        self.next.saturating_sub(self.window)
    }

    /// The timestamp of the next turn, if the thread is timed.
    fn tick(&mut self) -> Option<DateTime<Utc>> {
        let at = self.clock?;
        self.clock = Some(at + Duration::minutes(1));
        Some(at)
    }

    /// One exchange: the user says `text`, the agent calls `tools` and
    /// replies.
    pub(crate) async fn user(
        &mut self,
        text: &str,
        tools: &[ToolStep],
    ) -> Result<TurnRecord, tinymemory_api::Error> {
        let mut pre = PreTurn::new(&self.thread, self.next, text);
        pre.in_prompt_from = self.in_prompt_from();
        pre.at = self.tick();
        let started = Instant::now();
        let context = if self.openhuman {
            let memory = self.memory.clone();
            let hook = HostHook::for_turn(false, self.date_hint);
            let mut task = tokio::spawn(async move {
                let result = hook.run(memory, pre).await;
                (result, Instant::now())
            });
            match tokio::time::timeout(PRE_TURN_TIMEOUT, &mut task).await {
                Ok(result) => Some(
                    result
                        .map_err(|error| {
                            Error::Unavailable(format!("pre-turn task failed: {error}"))
                        })?
                        .0?,
                ),
                Err(_) => {
                    self.pending.push((started, task));
                    None
                }
            }
        } else {
            Some(self.memory.pre_turn(pre).await?)
        };
        let pre_ms = ms(started);

        let markdown = context
            .as_ref()
            .map_or("", |value| value.pack.markdown.as_str());
        let mut seen = HashSet::new();
        let lines: Vec<&str> = markdown
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        let duplicate_lines = lines.iter().filter(|line| !seen.insert(**line)).count();
        let prompt = if self.openhuman && !markdown.is_empty() {
            format!("<memory-context>\n{markdown}\n</memory-context>\n\n{text}")
        } else {
            markdown.to_string()
        };
        let answer = reply(&prompt, text, if self.openhuman { &[] } else { tools });
        let tool_calls: Vec<ToolCallRef> = tools
            .iter()
            .enumerate()
            .map(|(index, step)| ToolCallRef {
                name: step.name.to_string(),
                id: Some(format!("{}-{}-{index}", self.thread, self.next)),
            })
            .collect();
        let logged_calls: Vec<LoggedToolCall> = tools
            .iter()
            .zip(&tool_calls)
            .map(|(step, call)| LoggedToolCall {
                call: call.clone(),
                result: Some(step.result.to_string()),
            })
            .collect();
        let reply = if self.openhuman {
            logged_reply(&answer, &logged_calls)
        } else {
            answer
        };
        let mut post = PostTurn::new(&self.thread, self.next + 1, reply);
        post.tool_calls = tool_calls;
        post.at = self.tick();
        let started = Instant::now();
        let report = self.memory.post_turn(post).await?;
        let post_ms = ms(started);
        self.next += 2;
        Ok(TurnRecord {
            pre_ms,
            post_ms,
            logged: context.as_ref().is_some_and(|value| value.logged.is_some()),
            timed_out: context.is_none(),
            pack_tokens: context.as_ref().map_or(0, |value| value.pack.tokens),
            duplicate_lines,
            pack_lines: lines.len(),
            jobs: report.jobs,
            tool_calls: tools.len(),
        })
    }
}

/// The agent's reply: tool results first, then an answer or an
/// acknowledgement.
fn reply(markdown: &str, text: &str, tools: &[ToolStep]) -> String {
    let mut lines: Vec<String> = tools
        .iter()
        .map(|step| format!("{} returned: {}", step.name, step.result))
        .collect();
    if text.trim_end().ends_with('?') {
        lines.push(match answer(markdown, text) {
            Some(line) => format!("Going by memory: {line}"),
            None => NOT_IN_MEMORY.to_string(),
        });
    } else if lines.is_empty() {
        lines.push("Noted.".to_string());
    }
    lines.join("\n")
}

/// The agent's reply when nothing in the pack answers.
const NOT_IN_MEMORY: &str = "I don't have that in memory.";

/// Words too common to tell two lines apart.
const STOPWORDS: [&str; 42] = [
    "a", "an", "the", "is", "are", "was", "were", "do", "does", "did", "of", "to", "in", "on",
    "at", "for", "and", "or", "our", "we", "i", "my", "me", "you", "your", "what", "which", "who",
    "when", "where", "how", "it", "that", "this", "with", "be", "should", "can", "get", "have",
    "has", "from",
];

/// The lowercase content words of `text`.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-')
        .map(str::to_lowercase)
        .filter(|word| word.len() > 1 && !STOPWORDS.contains(&word.as_str()))
        .collect()
}

/// The pack line the agent would answer `question` with: the bullet sharing
/// the most content words with it (earlier sections win ties), skipping
/// lines that only repeat a question and ignoring the agent's own
/// non-answers.
pub(crate) fn answer(markdown: &str, question: &str) -> Option<String> {
    let wanted = words(question);
    let mut best: Option<(usize, String)> = None;
    for line in markdown.lines().filter_map(|line| line.strip_prefix("- ")) {
        let body = line.trim();
        if body.ends_with('?') {
            continue;
        }
        let body = body.replace(NOT_IN_MEMORY, "");
        let body = body.trim();
        let held = words(body);
        let overlap = wanted.iter().filter(|word| held.contains(word)).count();
        if overlap > 0 && best.as_ref().is_none_or(|(score, _)| overlap > *score) {
            best = Some((overlap, body.to_string()));
        }
    }
    best.map(|(_, line)| line)
}

/// Milliseconds since `started`.
pub(crate) fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1e3
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
