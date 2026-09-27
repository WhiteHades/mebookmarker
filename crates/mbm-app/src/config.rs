//! configuration: one toml file, read at startup, written by `mbm config`.
//!
//! everything has a default, and a missing file is a working install. that is
//! the whole design goal: `mbm add https://…` should work on a machine that has
//! never heard of this program, and a config file should be something a person
//! writes once when they want a source and never touches again.
//!
//! secrets are named, never stored. the config says which environment variable
//! holds a token, or which keyring entry, and the value is read at use. a
//! config file with a real key in it is a config file that ends up in a dotfiles
//! repository, and that is the failure mode this shape exists to prevent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mbm_core::error::{Error, Result};
use mbm_core::medium::{SinkMedium, SourceMedium};
use serde::{Deserialize, Serialize};

/// where the store lives by default.
///
/// xdg, so a backup that syncs `Documents` does not drag a database along, and
/// the directory is one a person can find again.
#[must_use]
pub fn default_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("mebookmarker")
}

/// the whole configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// where the database lives.
    pub data_dir: PathBuf,
    /// how many items one fetch may take.
    pub page_size: usize,
    /// how long one http request may take.
    pub request_timeout_secs: u64,
    /// how many times a transient failure is retried.
    pub retries: u32,
    /// the user agent sent with every request.
    pub user_agent: String,
    /// the sources to read from.
    pub sources: Vec<Source>,
    /// the outputs to write to.
    pub sinks: Vec<Sink>,
    /// the category taxonomy.
    pub taxonomy: mbm_core::category::Taxonomy,
    /// the tags offered to the model.
    pub tag_vocabulary: Vec<String>,
    /// the enrichment stages and their switches.
    pub enrich: Enrich,
    /// the local agent.
    pub agent: Agent,
    /// the gateway key's environment variable.
    pub jev_env_var: String,
    /// the twitter credentials' source.
    pub twitter: Twitter,
    /// github credentials' source.
    pub github: GitHub,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            page_size: 100,
            request_timeout_secs: 30,
            retries: 3,
            user_agent: format!("mebookmarker/{}", env!("CARGO_PKG_VERSION")),
            sources: Vec::new(),
            sinks: Vec::new(),
            taxonomy: mbm_core::category::Taxonomy::default_taxonomy(),
            tag_vocabulary: default_vocabulary(),
            enrich: Enrich::default(),
            agent: Agent::default(),
            jev_env_var: "AI_GATEWAY_API_KEY".to_owned(),
            twitter: Twitter::default(),
            github: GitHub::default(),
        }
    }
}

/// the tags offered to the model when the config does not say.
///
/// a fixed list matters: the model picks from what it is given, so an empty list
/// means no topic tagging at all, and a list of a hundred means the model has to
/// choose between two things that are really one.
#[must_use]
pub fn default_vocabulary() -> Vec<String> {
    [
        "rust",
        "python",
        "javascript",
        "systems",
        "databases",
        "algorithms",
        "security",
        "machine-learning",
        "devops",
        "design",
        "ux",
        "productivity",
        "business",
        "science",
        "health",
        "climate",
        "history",
        "philosophy",
        "art",
        "music",
        "writing",
        "learning",
        "news",
        "tools",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// a source to read from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Source {
    /// which medium this is.
    pub medium: SourceMedium,
    /// whether it is read at all.
    pub enabled: bool,
    /// the medium's own settings, as a free-form table.
    ///
    /// free-form per medium rather than a struct per medium, because a new
    /// adapter should be usable from a config file without a new field on this
    /// one.
    pub options: toml::Table,
}

impl Default for Source {
    fn default() -> Self {
        Self { medium: SourceMedium::Rss, enabled: true, options: toml::Table::new() }
    }
}

impl Source {
    /// read one option as a string.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.options.get(key)?.as_str()
    }

    /// read one option as a list of strings.
    #[must_use]
    pub fn list(&self, key: &str) -> Vec<String> {
        self.options
            .get(key)
            .and_then(toml::Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    }

    /// read one option as a boolean.
    #[must_use]
    pub fn flag(&self, key: &str) -> bool {
        self.options.get(key).and_then(toml::Value::as_bool).unwrap_or(false)
    }
}

/// an output to write to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sink {
    /// which format this is.
    pub kind: SinkMedium,
    /// whether it is written at all.
    pub enabled: bool,
    /// where it goes, relative to the data directory when relative.
    pub path: PathBuf,
}

impl Default for Sink {
    fn default() -> Self {
        Self { kind: SinkMedium::Jsonl, enabled: true, path: PathBuf::from("bookmarks.jsonl") }
    }
}

/// the enrichment stages and their switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Enrich {
    /// the free stage: links, mentions, tags, fingerprints.
    pub entities: bool,
    /// media alt text and the description gate.
    pub vision: bool,
    /// topic tags from the vocabulary.
    pub tags: bool,
    /// categories from the taxonomy.
    pub categorize: bool,
    /// generated titles and summaries, through the local agent.
    pub describe: bool,
    /// how many rows one stage takes per page.
    pub page: usize,
    /// the confidence below which a model's answer is thrown away.
    pub confidence_floor: f32,
}

impl Default for Enrich {
    fn default() -> Self {
        Self {
            entities: true,
            vision: true,
            tags: true,
            categorize: true,
            // prose through a local agent is the expensive tier, so it is the
            // one that has to be asked for
            describe: false,
            page: 256,
            confidence_floor: mbm_enrich::CONFIDENCE_FLOOR,
        }
    }
}

/// the local agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agent {
    /// which agent to drive: `opencode`, `codex`, or `claude`.
    pub name: String,
    /// the model, or `None` for whatever the agent defaults to.
    pub model: Option<String>,
    /// how long one call may take.
    pub timeout_secs: u64,
}

impl Default for Agent {
    fn default() -> Self {
        Self { name: String::new(), model: None, timeout_secs: 180 }
    }
}

impl Agent {
    /// the agent to use: the configured one, or the first one installed.
    ///
    /// falling back to what is installed is what makes the describe stage work
    /// on a machine with one agent and no config.
    #[must_use]
    pub fn resolve(&self) -> Option<mbm_agent::Agent> {
        if self.name.is_empty() {
            return mbm_agent::default_agent();
        }
        self.name.parse().ok().or_else(mbm_agent::default_agent)
    }

    /// the driver this describes.
    #[must_use]
    pub fn driver(&self) -> Option<mbm_agent::Driver> {
        let agent = self.resolve()?;
        let mut driver =
            mbm_agent::Driver::new(agent).with_timeout(Duration::from_secs(self.timeout_secs));
        if let Some(model) = &self.model {
            driver = driver.with_model(model.clone());
        }
        Some(driver)
    }
}

/// where a secret comes from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Twitter {
    /// the environment variable holding the cookie jar.
    pub cookie_env_var: String,
    /// a path to a cookie jar, relative to the data directory.
    pub cookie_file: PathBuf,
    /// a path to a `bird` binary, for the cli route.
    pub bird_path: PathBuf,
    /// whether to shell out to `bird` instead of calling graphql.
    pub use_bird: bool,
}

/// where a secret comes from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitHub {
    /// the environment variable holding the token.
    pub token_env_var: String,
}

impl GitHub {
    /// the token, read from the environment.
    #[must_use]
    pub fn token(&self) -> Option<String> {
        std::env::var(&self.token_env_var).ok().filter(|t| !t.trim().is_empty())
    }
}

impl Twitter {
    /// the cookies, read from the environment or from a file.
    #[must_use]
    pub fn cookies(&self, data_dir: &Path) -> mbm_ingest::Cookies {
        if let Ok(jar) = std::env::var(&self.cookie_env_var)
            && !jar.trim().is_empty()
        {
            return mbm_ingest::Cookies::from_cookie_jar(&jar);
        }
        let path = self.cookie_file.clone();
        let path = if path.is_absolute() { path } else { data_dir.join(path) };
        std::fs::read_to_string(path)
            .map(|body| mbm_ingest::Cookies::from_cookie_jar(&body))
            .unwrap_or_default()
    }
}

impl Config {
    /// read a config from a path, falling back to the defaults for anything
    /// the file does not say.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(body) => {
                toml::from_str(&body).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// the config path for a given directory, or the default one.
    #[must_use]
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("mebookmarker.toml")
    }

    /// where the store lives.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.data_dir.join("mebookmarker.db")
    }

    /// resolve a sink path against the data directory.
    #[must_use]
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { path.to_path_buf() } else { self.data_dir.join(path) }
    }

    /// the gateway key, read from the environment.
    #[must_use]
    pub fn jev_key(&self) -> Option<String> {
        std::env::var(&self.jev_env_var).ok().filter(|k| !k.trim().is_empty())
    }

    /// the sources that are switched on.
    #[must_use]
    pub fn enabled_sources(&self) -> Vec<&Source> {
        self.sources.iter().filter(|s| s.enabled).collect()
    }

    /// the sinks that are switched on.
    #[must_use]
    pub fn enabled_sinks(&self) -> Vec<&Sink> {
        self.sinks.iter().filter(|s| s.enabled).collect()
    }

    /// the sources of one medium.
    #[must_use]
    pub fn sources_of(&self, medium: SourceMedium) -> Vec<&Source> {
        self.sources.iter().filter(|s| s.medium == medium && s.enabled).collect()
    }

    /// check everything that can be checked without touching the network.
    pub fn validate(&self) -> Result<()> {
        if self.page_size == 0 {
            return Err(Error::Config("page_size must be at least 1".to_owned()));
        }
        if self.retries > 10 {
            return Err(Error::Config("retries above 10 is a typo, not a setting".to_owned()));
        }
        if self.enrich.confidence_floor < 0.0 || self.enrich.confidence_floor > 1.0 {
            return Err(Error::Config(
                "enrich.confidence_floor is a probability, so 0 to 1".to_owned(),
            ));
        }
        if !self.agent.name.is_empty() {
            self.agent
                .name
                .parse::<mbm_agent::Agent>()
                .map_err(|e| Error::Config(format!("agent: {e}")))?;
        }

        // two sources of the same medium writing to the same place is fine, and
        // two sinks of the same kind writing to one file is not
        let mut paths: Vec<&Path> = Vec::new();
        for sink in self.enabled_sinks() {
            if paths.contains(&sink.path.as_path()) {
                return Err(Error::Config(format!(
                    "two enabled sinks write to `{}`",
                    sink.path.display()
                )));
            }
            paths.push(&sink.path);
        }

        for source in &self.sources {
            if source.enabled
                && source.medium == SourceMedium::X
                && self.twitter.cookie_env_var.is_empty()
            {
                return Err(Error::Config(
                    "the twitter source is enabled with no cookie_env_var to read its cookies from"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// the configuration as toml, with a header explaining each section.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).unwrap_or_default();
        format!("{HEADER}{body}")
    }

    /// the example's contents, which differ from a plain dump in two ways that
    /// make it portable: the data directory is relative, and it names itself.
    #[must_use]
    pub fn example_toml() -> String {
        let config = Config { data_dir: PathBuf::from("."), ..Config::default() };
        config.to_toml().replace("mebookmarker.toml", "mebookmarker.toml.example")
    }

    /// write the example, for `mbm config --init` and for a test that keeps it
    /// current.
    pub fn write_example(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        std::fs::write(path, Self::example_toml()).map_err(|e| Error::io(path, e))
    }

    /// write the configuration out, creating the directory.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        std::fs::write(path, self.to_toml()).map_err(|e| Error::io(path, e))
    }
}

/// the comment block every generated config starts with.
///
/// kept in its own file because it is prose about the format rather than the
/// format itself, and mixing the two in one string literal makes both harder to
/// read.
const HEADER: &str = include_str!("config.header.toml");

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_missing_file_is_a_working_install() {
        let dir = tmp();
        let config = Config::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn a_partial_file_takes_the_rest_from_the_defaults() {
        let dir = tmp();
        let path = dir.path().join("mebookmarker.toml");
        std::fs::write(&path, "page_size = 25\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.page_size, 25);
        assert_eq!(config.retries, Config::default().retries);
    }

    #[test]
    fn a_full_round_trip_keeps_everything() {
        let dir = tmp();
        let mut config = Config::default();
        config.page_size = 7;
        config.enrich.describe = true;
        config.agent.name = "codex".to_owned();
        config.agent.model = Some("gpt-5".to_owned());
        config.sources.push(Source {
            medium: SourceMedium::Rss,
            enabled: true,
            options: toml::Table::new(),
        });
        config.sinks.push(Sink {
            kind: SinkMedium::Html,
            enabled: true,
            path: PathBuf::from("site"),
        });

        let path = dir.path().join("mebookmarker.toml");
        config.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_ignored() {
        let dir = tmp();
        let path = dir.path().join("mebookmarker.toml");
        std::fs::write(&path, "page_sizes = 25\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("page_sizes"), "{err}");
    }

    #[test]
    fn a_malformed_file_names_the_path() {
        let dir = tmp();
        let path = dir.path().join("mebookmarker.toml");
        std::fs::write(&path, "this is not toml\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("mebookmarker.toml"), "{err}");
    }

    /// the checked-in example, regenerated so it cannot drift from the defaults.
    ///
    /// a config example that a release changed and nobody noticed is worse than
    /// no example, because a person trusts it. this test fails the moment the
    /// defaults and the file disagree.
    #[test]
    fn the_example_config_matches_the_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mebookmarker.toml.example");
        let Ok(existing) = std::fs::read_to_string(&path) else {
            panic!("{} does not exist. create it with:\n    mbm config init", path.display());
        };
        // the checked-in file carries a hand-written examples section after the
        // generated part, so the check is that the generated part is a prefix
        // and not a copy
        let generated = Config::example_toml();
        assert!(
            existing.starts_with(&generated),
            "the generated part of {} does not match the defaults.\nrun `mbm config --example` \
             and keep the examples section.\n\n{}",
            path.display(),
            generated
        );
    }

    #[test]
    fn the_saved_file_explains_itself() {
        let body = Config::default().to_toml();
        assert!(body.starts_with("# mebookmarker configuration"), "{body}");
        assert!(body.contains("mbm config init"), "{body}");
    }

    #[test]
    fn a_zero_page_size_is_rejected() {
        let mut config = Config::default();
        config.page_size = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_absurd_retry_count_is_rejected() {
        let mut config = Config::default();
        config.retries = 100;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("retries"), "{err}");
    }

    #[test]
    fn a_confidence_outside_zero_to_one_is_rejected() {
        let mut config = Config::default();
        config.enrich.confidence_floor = 1.5;
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_unknown_agent_name_is_rejected_with_the_real_ones() {
        let mut config = Config::default();
        config.agent.name = "gpt".to_owned();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("opencode"), "{err}");
    }

    #[test]
    fn two_sinks_writing_to_one_path_are_rejected() {
        let mut config = Config::default();
        config.sinks = vec![
            Sink { kind: SinkMedium::Json, enabled: true, path: PathBuf::from("out") },
            Sink { kind: SinkMedium::Jsonl, enabled: true, path: PathBuf::from("out") },
        ];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("out"), "{err}");
    }

    #[test]
    fn a_disabled_sink_writing_to_one_path_is_fine() {
        let mut config = Config::default();
        config.sinks = vec![
            Sink { kind: SinkMedium::Json, enabled: true, path: PathBuf::from("out") },
            Sink { kind: SinkMedium::Jsonl, enabled: false, path: PathBuf::from("out") },
        ];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn an_enabled_twitter_source_needs_somewhere_to_read_cookies_from() {
        let mut config = Config::default();
        config.sources.push(Source { medium: SourceMedium::X, ..Source::default() });
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("cookie_env_var"), "{err}");
    }

    #[test]
    fn source_options_are_read_by_type() {
        let source: Source = toml::from_str(
            r#"
            medium = "rss"
            enabled = true

            [options]
            urls = ["https://a.example/feed", "https://b.example/feed"]
            limit = 50
            paginate = true
            "#,
        )
        .unwrap();
        assert_eq!(source.get("nope"), None);
        assert_eq!(source.list("urls").len(), 2);
        assert!(source.flag("paginate"));
        assert!(!source.flag("missing"));
    }

    #[test]
    fn the_database_lives_under_the_data_directory() {
        let mut config = Config::default();
        config.data_dir = PathBuf::from("/tmp/mbm");
        assert_eq!(config.database(), PathBuf::from("/tmp/mbm/mebookmarker.db"));
    }

    #[test]
    fn a_relative_sink_path_resolves_against_the_data_directory() {
        let mut config = Config::default();
        config.data_dir = PathBuf::from("/data");
        assert_eq!(config.resolve(Path::new("out.jsonl")), PathBuf::from("/data/out.jsonl"));
        assert_eq!(config.resolve(Path::new("/abs/out.jsonl")), PathBuf::from("/abs/out.jsonl"));
    }

    #[test]
    fn an_absent_agent_name_falls_back_to_what_is_installed() {
        let config = Config::default();
        // the answer depends on the machine, and both answers are correct
        assert_eq!(config.agent.resolve(), mbm_agent::default_agent());
    }

    #[test]
    fn a_named_agent_wins_over_the_installed_one() {
        let mut config = Config::default();
        config.agent.name = "claude".to_owned();
        assert_eq!(config.agent.resolve(), Some(mbm_agent::Agent::Claude));
    }

    #[test]
    fn a_driver_carries_the_model_and_the_timeout() {
        let mut config = Config::default();
        config.agent.name = "codex".to_owned();
        config.agent.model = Some("gpt-5".to_owned());
        config.agent.timeout_secs = 30;
        let driver = config.agent.driver().unwrap();
        let args = driver.command_line("p");
        let at = args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(args[at + 1], "gpt-5");
    }

    #[test]
    fn the_default_vocabulary_is_usable() {
        let vocabulary = default_vocabulary();
        assert!(vocabulary.len() > 10);
        assert!(vocabulary.contains(&"rust".to_owned()));
        let mut sorted = vocabulary.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), vocabulary.len(), "no duplicates");
    }

    #[test]
    fn describe_is_the_stage_that_has_to_be_asked_for() {
        let enrich = Enrich::default();
        assert!(enrich.entities);
        assert!(!enrich.describe, "prose through an agent is the expensive tier");
    }

    #[test]
    fn the_default_taxonomy_has_categories() {
        let config = Config::default();
        assert!(!config.taxonomy.categories.is_empty());
        assert!(config.taxonomy.get(&config.taxonomy.fallback).is_some());
    }

    #[test]
    fn enabled_sources_are_the_ones_the_user_wants() {
        let mut config = Config::default();
        config.sources = vec![
            Source { medium: SourceMedium::Rss, enabled: true, options: toml::Table::new() },
            Source { medium: SourceMedium::Reddit, enabled: false, options: toml::Table::new() },
        ];
        assert_eq!(config.enabled_sources().len(), 1);
        assert_eq!(config.sources_of(SourceMedium::Rss).len(), 1);
        assert_eq!(config.sources_of(SourceMedium::Reddit).len(), 0);
    }

    #[test]
    fn the_config_path_lands_in_the_given_directory() {
        assert_eq!(
            Config::path_in(Path::new("/etc/mbm")),
            PathBuf::from("/etc/mbm/mebookmarker.toml")
        );
    }
}
