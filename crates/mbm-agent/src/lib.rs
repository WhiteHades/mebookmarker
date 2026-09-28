//! drivers for the local coding agents.
//!
//! tier three of the pipeline: the stages that write prose. a title, a summary,
//! a description of an image — these are the only parts of a bookmark that a
//! typed question cannot produce, and they are a small fraction of the work.
//!
//! three agents, all of which are already installed on a developer machine and
//! all of which are driven the same way: hand them a prompt, take back one
//! message.
//!
//! | agent      | binary     | how it is called                                |
//! |------------|------------|-------------------------------------------------|
//! | `opencode` | `opencode` | `run --format json --model <model>`              |
//! | `codex`    | `codex`    | `exec --output-last-message <file>`              |
//! | `claude`   | `claude`   | `-p <prompt> --output-format json`               |
//!
//! each driver is a thin wrapper over a command line. the parsing half is a
//! pure function over bytes, so it is tested without an agent installed, and
//! the request half is the thing that needs a real binary.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use mbm_core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// which agent to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    /// opencode, the default.
    Opencode,
    /// codex.
    Codex,
    /// claude, off by default because it is the one that costs money on a
    /// subscription rather than on a token count we can see.
    Claude,
}

impl Agent {
    /// every agent, cheapest local first.
    pub const ALL: &'static [Self] = &[Self::Opencode, Self::Codex, Self::Claude];

    /// the binary that has to be on the path.
    #[must_use]
    pub const fn binary(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// the stored name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// parse a stored name.
    ///
    /// the error lists the real names, because this string comes from a config
    /// file and a typo there should say what to type instead.
    pub fn parse(raw: &str) -> Result<Self> {
        let wanted = raw.trim().to_ascii_lowercase();
        Self::ALL.iter().copied().find(|agent| agent.name() == wanted).ok_or_else(|| {
            Error::Config(format!(
                "unknown agent `{raw}`: expected one of {}",
                Self::ALL.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
            ))
        })
    }

    /// whether the binary is on the path.
    #[must_use]
    pub fn is_installed(self) -> bool {
        mbm_extract::api::which(self.binary())
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for Agent {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// a configured agent.
#[derive(Debug, Clone)]
pub struct Driver {
    agent: Agent,
    model: Option<String>,
    timeout: Duration,
    extra_args: Vec<String>,
}

impl Driver {
    /// build a driver.
    #[must_use]
    pub fn new(agent: Agent) -> Self {
        Self { agent, model: None, timeout: Duration::from_secs(180), extra_args: Vec::new() }
    }

    /// pick the model.
    ///
    /// each agent names its models differently, and the point of a default is
    /// that it tracks whatever the agent ships as its current one.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// change how long one call may take.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// add a raw argument, for a flag the driver does not model.
    #[must_use]
    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    /// which agent this is.
    #[must_use]
    pub fn agent(&self) -> Agent {
        self.agent
    }

    /// the command line this would run, for a log line or a dry run.
    #[must_use]
    pub fn command_line(&self, prompt: &str) -> Vec<String> {
        let mut args: Vec<String> = match self.agent {
            Agent::Opencode => vec!["run".to_owned(), "--format".to_owned(), "json".to_owned()],
            // codex refuses to run outside a directory it trusts, and this
            // program runs wherever the user's shell happens to be. it is a
            // read-only call over a prompt this program wrote, so there is
            // nothing for the check to protect.
            Agent::Codex => vec![
                "exec".to_owned(),
                "--skip-git-repo-check".to_owned(),
                "--sandbox".to_owned(),
                "read-only".to_owned(),
            ],
            Agent::Claude => {
                vec![
                    "-p".to_owned(),
                    prompt.to_owned(),
                    "--output-format".to_owned(),
                    "json".to_owned(),
                ]
            }
        };
        // codex spells it `-m`; the other two spell it `--model`
        if let Some(model) = &self.model {
            args.push(
                match self.agent {
                    Agent::Codex => "-m",
                    Agent::Opencode | Agent::Claude => "--model",
                }
                .to_owned(),
            );
            args.push(model.clone());
        }
        args.extend(self.extra_args.iter().cloned());
        if self.agent != Agent::Claude {
            args.push(prompt.to_owned());
        }
        args
    }

    /// check that the agent can be run at all.
    pub fn preflight(&self) -> Result<()> {
        if !self.agent.is_installed() {
            return Err(Error::Agent(format!(
                "`{}` is not on the path. install it, or name a different agent.",
                self.agent.binary()
            )));
        }
        Ok(())
    }

    /// run one prompt and return the agent's reply.
    pub async fn ask(&self, prompt: &str) -> Result<String> {
        self.preflight()?;
        let args = self.command_line(prompt);
        let mut command = Command::new(self.agent.binary());
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // the agent inherits the environment so its own login and provider
        // setup still applies
        let child = command
            .spawn()
            .map_err(|e| Error::Agent(format!("cannot run {}: {e}", self.agent.binary())))?;

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| Error::Agent(format!("{} did not finish in time", self.agent.binary())))?
            .map_err(|e| Error::Agent(format!("{} failed: {e}", self.agent.binary())))?;

        if !output.status.success() {
            // an agent reports a provider error on stdout, not stderr, and an
            // empty message is the one thing that makes a failure impossible to
            // act on. both streams are worth looking at, and the useful part of
            // a json error is its message, not the envelope.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let raw = if stderr.trim().is_empty() { &stdout } else { &stderr };
            let detail = explain(raw);
            return Err(Error::Agent(format!(
                "{} exited {}: {detail}",
                self.agent.binary(),
                output.status
            )));
        }

        Ok(parse_reply(self.agent, &output.stdout))
    }

    /// run a prompt whose answer is json, and return the parsed value.
    ///
    /// a model that wraps its answer in a code fence is the normal case rather
    /// than the exception, so the fence is unwrapped before parsing.
    pub async fn ask_json<T: serde::de::DeserializeOwned>(&self, prompt: &str) -> Result<T> {
        let raw = self.ask(prompt).await?;
        let cleaned = unwrap_fence(&raw);
        serde_json::from_str(&cleaned).map_err(|e| {
            Error::Agent(format!(
                "the agent did not return the json that was asked for: {e}\n{cleaned}"
            ))
        })
    }
}

/// the one message an agent produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reply {
    /// the text.
    pub text: String,
    /// what the agent said it did, when it says.
    pub session: Option<String>,
}

/// the useful part of an agent's failure output.
///
/// an agent that fails on a provider error writes a json envelope whose
/// `error.message` is the only line a person needs, and the rest is a session
/// id and a timestamp. anything that is not json is passed through as written.
#[must_use]
pub fn explain(raw: &str) -> String {
    for line in raw.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(message) = value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(|m| m.as_str())
        {
            let kind = value
                .pointer("/error/type")
                .or_else(|| value.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("error");
            return format!("{kind}: {message}");
        }
    }

    // the last few non-blank lines, oldest first: an agent that failed loudly
    // tends to print a banner, a blank line, and then the reason
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(4)..].to_vec();
    let joined = tail.join(" | ");
    if joined.trim().is_empty() { "no output on either stream".to_owned() } else { joined }
}

/// read an agent's output.
///
/// each of the three has its own envelope, and each of them has changed shape
/// at some point, so the reader takes the newest field it recognises and falls
/// back to the next.
pub fn parse_reply(agent: Agent, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    match agent {
        Agent::Opencode => parse_opencode(&text),
        Agent::Codex => text.trim().to_owned(),
        Agent::Claude => parse_claude(&text),
    }
}

/// read opencode's json stream.
///
/// `opencode run --format json` writes one json object per line, one per
/// internal event. the answer is the text of the last `message` part, and the
/// session id rides on the first line.
fn parse_opencode(text: &str) -> String {
    let mut out = String::new();
    let mut saw_json = false;

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        saw_json = true;

        // reasoning and step markers ride on the same stream and carry no text
        if let Some(part) = value.get("part")
            && part.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(body) = part.get("text").and_then(|t| t.as_str())
        {
            out.clear();
            out.push_str(body);
        }
        // the older shape put the answer on `text` directly
        if out.is_empty()
            && let Some(body) = value.get("text").and_then(|t| t.as_str())
        {
            out.push_str(body);
        }
    }

    if saw_json {
        return out.trim().to_owned();
    }
    // a run that printed prose instead of the stream it was asked for is still
    // usable, and using it is better than failing
    text.trim().to_owned()
}

/// read claude's json output.
///
/// `claude -p` with `--output-format json` returns one object whose `result`
/// holds the answer, wrapped in a type tag.
fn parse_claude(text: &str) -> String {
    for line in text.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(result) = value.get("result").and_then(|r| r.as_str()) {
            return result.trim().to_owned();
        }
    }
    text.trim().to_owned()
}

/// strip a markdown code fence from around a json answer.
///
/// the fence is the single most common reason a json-parsing step fails against
/// a model, and unwrapping it is cheaper than asking the model to stop.
#[must_use]
pub fn unwrap_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_owned();
    }
    let without_open = match trimmed.find('\n') {
        Some(at) => &trimmed[at + 1..],
        None => return trimmed.to_owned(),
    };
    let body = match without_open.rfind("```") {
        Some(at) => &without_open[..at],
        None => without_open,
    };
    // ```json and ```JSON both appear; the language tag itself is not json
    let body = body.strip_prefix("json").or_else(|| body.strip_prefix("JSON")).unwrap_or(body);
    body.trim().to_owned()
}

/// the prompt the describe stage sends.
///
/// kept in one place because it is the only prose this crate produces, and
/// because a change to it should be a visible diff rather than an edit buried
/// in a loop.
#[must_use]
pub fn describe_prompt(bookmark: &mbm_core::bookmark::Bookmark) -> String {
    use std::fmt::Write as _;

    let text = bookmark.text.chars().take(4000).collect::<String>();
    let mut prompt = String::with_capacity(text.len() + 600);
    prompt.push_str(
        "You are labelling one bookmark in a personal archive. Reply with a json object and \
         nothing else: {\"title\": string, \"summary\": string}.\n\n",
    );
    prompt.push_str(
        "- title: at most 70 characters, naming what the item is, no quotes or \
                     trailing punctuation\n",
    );
    prompt.push_str(
        "- summary: at most 200 characters, one sentence, saying what the item is \
                     for\n\n",
    );
    if let Some(url) = &bookmark.url {
        let _ = writeln!(prompt, "url: {}", url.as_str());
    }
    if let Some(handle) = &bookmark.author {
        let _ = writeln!(prompt, "author: {}", handle.handle);
    }
    for link in bookmark.links.iter().take(5) {
        let _ = writeln!(prompt, "link: {}", link.resolved.as_str());
    }
    prompt.push_str("\ntext:\n");
    prompt.push_str(&text);
    prompt
}

/// the prompt the vision stage sends for a described image.
#[must_use]
pub fn describe_media_prompt(bookmark: &mbm_core::bookmark::Bookmark) -> String {
    let media = bookmark.media.iter().map(|m| m.url.as_str()).collect::<Vec<_>>().join("\n");
    format!(
        "Describe each image in this bookmark in one sentence each, plainly, saying what is \
         actually in the picture. Reply with a json object and nothing else: \
         {{\"captions\": [string]}}.\n\nimages:\n{media}\n\ntext:\n{}\n",
        bookmark.text.chars().take(2000).collect::<String>()
    )
}

/// where an agent's state and credentials live.
///
/// a driver never reads these; they are here so the app can report a path in an
/// error rather than leaving a user to hunt for it.
#[must_use]
pub fn state_dir(agent: Agent) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    match agent {
        Agent::Opencode => home.join(".local/share/opencode"),
        Agent::Codex => home.join(".codex"),
        Agent::Claude => home.join(".claude"),
    }
}

/// the agents that are installed, for a status line.
#[must_use]
pub fn installed() -> Vec<Agent> {
    Agent::ALL.iter().copied().filter(|a| a.is_installed()).collect()
}

/// the first installed agent, or `None`.
///
/// this is the default the app uses, so a fresh install with one agent on the
/// path needs no configuration at all.
#[must_use]
pub fn default_agent() -> Option<Agent> {
    Agent::ALL.iter().copied().find(|a| a.is_installed())
}

/// write a prompt to a temporary file, for an agent that reads a file rather
/// than an argument.
///
/// some agents treat a long argument as an instruction to follow rather than a
/// question to answer, and a file is unambiguous.
pub async fn write_prompt(prompt: &str) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push(format!("mebookmarker-prompt-{}.md", std::process::id()));
    let mut file = tokio::fs::File::create(&path).await.map_err(|e| Error::io(&path, e))?;
    file.write_all(prompt.as_bytes()).await.map_err(|e| Error::io(&path, e))?;
    Ok(path)
}
