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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_round_trip() {
        for agent in Agent::ALL {
            assert_eq!(agent.name().parse::<Agent>().unwrap(), *agent);
            assert_eq!(agent.name().to_owned().parse::<Agent>().unwrap(), *agent);
        }
    }

    #[test]
    fn an_unknown_agent_lists_the_real_ones() {
        let err = "gpt".parse::<Agent>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gpt"), "{msg}");
        assert!(msg.contains("opencode"), "{msg}");
    }

    #[test]
    fn the_agent_names_are_also_the_binary_names() {
        for agent in Agent::ALL {
            assert_eq!(agent.binary(), agent.name());
        }
    }

    #[test]
    fn opencode_is_called_with_a_json_stream() {
        let driver = Driver::new(Agent::Opencode);
        let args = driver.command_line("do the thing");
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--format".to_owned()));
        assert!(args.contains(&"json".to_owned()));
        assert_eq!(args.last().unwrap(), "do the thing");
    }

    #[test]
    fn codex_is_told_it_may_run_anywhere_and_write_nothing() {
        // without the repo check codex refuses to run from a directory it does
        // not trust, and without the sandbox it would run with write access for
        // what is a read-only question
        let args = Driver::new(Agent::Codex).command_line("p");
        assert!(args.contains(&"--skip-git-repo-check".to_owned()), "{args:?}");
        let at = args.iter().position(|a| a == "--sandbox").unwrap();
        assert_eq!(args[at + 1], "read-only");
    }

    #[test]
    fn a_model_is_passed_in_each_agents_own_spelling() {
        let opencode = Driver::new(Agent::Opencode).with_model("anthropic/claude");
        let args = opencode.command_line("p");
        let at = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[at + 1], "anthropic/claude");

        let codex = Driver::new(Agent::Codex).with_model("gpt-5");
        let args = codex.command_line("p");
        let at = args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(args[at + 1], "gpt-5");
    }

    #[test]
    fn claude_takes_the_prompt_as_its_own_argument() {
        let args = Driver::new(Agent::Claude).command_line("the prompt");
        let at = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[at + 1], "the prompt");
    }

    #[test]
    fn an_extra_argument_reaches_the_command_line() {
        let driver = Driver::new(Agent::Opencode).with_arg("--continue");
        assert!(driver.command_line("p").contains(&"--continue".to_owned()));
    }

    #[test]
    fn an_opencode_stream_yields_its_last_text_part() {
        let body = concat!(
            r#"{"type":"step-start","part":{"type":"step-start"}}"#,
            "\n",
            r#"{"part":{"type":"reasoning","text":"thinking"}}"#,
            "\n",
            r#"{"part":{"type":"text","text":"first draft"}}"#,
            "\n",
            r#"{"part":{"type":"text","text":"the final answer"}}"#,
            "\n",
        );
        let reply = parse_reply(Agent::Opencode, body.as_bytes());
        assert_eq!(reply, "the final answer");
    }

    #[test]
    fn an_opencode_run_that_printed_prose_still_parses() {
        let reply = parse_reply(Agent::Opencode, b"just some prose\n");
        assert_eq!(reply, "just some prose");
    }

    #[test]
    fn an_empty_opencode_stream_is_empty_rather_than_an_error() {
        let reply = parse_reply(Agent::Opencode, b"");
        assert_eq!(reply, "");
    }

    #[test]
    fn a_claude_result_is_read_from_its_envelope() {
        let body =
            r#"{"type":"result","subtype":"success","result":"the answer","total_cost_usd":0.01}"#;
        assert_eq!(parse_reply(Agent::Claude, body.as_bytes()), "the answer");
    }

    #[test]
    fn a_claude_stream_takes_the_last_result() {
        let body = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"result","result":"first"  }"#,
            "\n",
            r#"{"type":"result","result":"second"}"#,
            "\n",
        );
        assert_eq!(parse_reply(Agent::Claude, body.as_bytes()), "second");
    }

    #[test]
    fn a_claude_run_with_no_envelope_keeps_its_text() {
        assert_eq!(parse_reply(Agent::Claude, b"plain\n"), "plain");
    }

    #[test]
    fn codex_output_is_taken_as_written() {
        assert_eq!(parse_reply(Agent::Codex, b"  the answer \n"), "the answer");
    }

    #[test]
    fn a_provider_error_reads_as_a_sentence() {
        let raw = r#"{"type":"error","sessionID":"ses_x","error":{"type":"provider.quota","message":"Insufficient credits","status":402}}"#;
        assert_eq!(explain(raw), "provider.quota: Insufficient credits");
    }

    #[test]
    fn a_plain_error_reads_as_written() {
        assert_eq!(
            explain("could not find the model\ntry again"),
            "could not find the model | try again"
        );
    }

    #[test]
    fn an_empty_error_says_so_rather_than_being_blank() {
        assert_eq!(explain(""), "no output on either stream");
        assert_eq!(explain("   \n  "), "no output on either stream");
    }

    #[test]
    fn a_fence_around_json_is_unwrapped() {
        let raw = "```json\n{\"title\":\"a\"}\n```";
        assert_eq!(unwrap_fence(raw), "{\"title\":\"a\"}");
    }

    #[test]
    fn a_fence_with_no_language_tag_is_unwrapped() {
        let raw = "```\n{\"title\":\"a\"}\n```";
        assert_eq!(unwrap_fence(raw), "{\"title\":\"a\"}");
    }

    #[test]
    fn a_fence_with_an_uppercase_tag_is_unwrapped() {
        assert_eq!(unwrap_fence("```JSON\n{}\n```"), "{}");
    }

    #[test]
    fn prose_around_no_fence_is_left_alone() {
        assert_eq!(unwrap_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(unwrap_fence("  {\"a\":1}  "), "{\"a\":1}");
    }

    #[test]
    fn an_unterminated_fence_keeps_its_body() {
        assert_eq!(unwrap_fence("```json\n{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn the_describe_prompt_names_the_shape_it_wants() {
        let mut b = mbm_core::bookmark::Bookmark::new(
            mbm_core::bookmark::SourceRef::new(mbm_core::medium::SourceMedium::X, "1", None),
            "a post about sqlite internals",
            0,
        );
        b.url = Some(url::Url::parse("https://example.com/a").unwrap());
        let prompt = describe_prompt(&b);
        assert!(prompt.contains("\"title\""), "{prompt}");
        assert!(prompt.contains("\"summary\""), "{prompt}");
        assert!(prompt.contains("https://example.com/a"), "{prompt}");
        assert!(prompt.contains("sqlite internals"), "{prompt}");
    }

    #[test]
    fn a_long_post_is_truncated_before_it_is_sent() {
        let text = "x".repeat(10_000);
        let b = mbm_core::bookmark::Bookmark::new(
            mbm_core::bookmark::SourceRef::new(mbm_core::medium::SourceMedium::X, "1", None),
            &text,
            0,
        );
        let prompt = describe_prompt(&b);
        assert!(prompt.len() < 5000, "the prompt was {} bytes", prompt.len());
    }

    #[test]
    fn the_media_prompt_lists_the_images() {
        let mut b = mbm_core::bookmark::Bookmark::new(
            mbm_core::bookmark::SourceRef::new(mbm_core::medium::SourceMedium::X, "1", None),
            "a post",
            0,
        );
        b.media.push(mbm_core::bookmark::Media {
            kind: mbm_core::bookmark::MediaKind::Photo,
            url: url::Url::parse("https://pbs.twimg.com/media/a.jpg").unwrap(),
            preview_url: None,
            width: None,
            height: None,
            duration_ms: None,
            alt_text: None,
        });
        let prompt = describe_media_prompt(&b);
        assert!(prompt.contains("pbs.twimg.com"), "{prompt}");
        assert!(prompt.contains("captions"), "{prompt}");
    }

    #[test]
    fn the_state_directory_is_under_the_home_directory() {
        let home = dirs::home_dir().unwrap();
        for agent in Agent::ALL {
            assert!(state_dir(*agent).starts_with(&home), "{agent}");
        }
    }

    #[test]
    fn a_driver_for_a_missing_binary_fails_with_the_name_in_it() {
        // the name is checked rather than the path, so this is the same on any
        // machine: a driver for an agent that is not installed must say so
        let err = Driver::new(Agent::Codex).preflight();
        if Agent::Codex.is_installed() {
            assert!(err.is_ok());
        } else {
            let msg = err.unwrap_err().to_string();
            assert!(msg.contains("codex"), "{msg}");
            assert!(msg.contains("not on the path"), "{msg}");
        }
    }
}
