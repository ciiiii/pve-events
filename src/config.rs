//! Configuration: a TOML file, environment variables, or both.
//!
//! Every field can come from either source; the environment always wins, so a
//! committed config file can carry the boring settings while the token stays in
//! the environment. A config file is optional — env alone is enough to run.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::catalogue;
use crate::filter::Filter;
use crate::sink::Sink;

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Pve {
    /// Base URL of the Proxmox API, e.g. `https://10.0.0.100:8006`.
    #[serde(default)]
    pub url: String,
    /// `user@realm`, e.g. `monitor@pve`.
    #[serde(default)]
    pub user: String,
    /// The token id — the part after `!` in `user@realm!tokenid`.
    #[serde(default)]
    pub token_name: String,
    /// Secret. Prefer `PVE_TOKEN_VALUE` in the environment over a file on disk.
    #[serde(default)]
    pub token_value: String,
    /// Poll one node instead of the whole cluster.
    #[serde(default)]
    pub node: Option<String>,
    /// PEM root to verify the API cert against, for the usual private-CA setup.
    #[serde(default)]
    pub ca_bundle: Option<PathBuf>,
    #[serde(default = "yes")]
    pub verify_tls: bool,
    #[serde(default = "default_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn yes() -> bool {
    true
}

fn default_interval() -> u64 {
    20
}

fn default_timeout() -> u64 {
    15
}

fn default_state_path() -> PathBuf {
    PathBuf::from("/var/lib/pve-events/state.json")
}

impl Default for Pve {
    fn default() -> Self {
        Pve {
            url: String::new(),
            user: String::new(),
            token_name: String::new(),
            token_value: String::new(),
            node: None,
            ca_bundle: None,
            verify_tls: true,
            poll_interval_secs: default_interval(),
            timeout_secs: default_timeout(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub pve: Pve,
    #[serde(default)]
    pub filter: Filter,
    #[serde(default = "default_sink")]
    pub sink: Sink,
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,
    /// Unix timestamp to use as the initial watermark instead of *now*, applied
    /// only when there is no state file yet.
    ///
    /// The default of "start from now" exists so a fresh deploy does not fire a
    /// burst for the backlog `/cluster/tasks` always returns. Set `0` to replay
    /// everything still in that window — useful for a first smoke test.
    #[serde(default)]
    pub backfill_from: Option<i64>,
}

fn default_sink() -> Sink {
    Sink {
        kind: crate::sink::SinkKind::Stdout,
        url: String::new(),
        method: "POST".into(),
        headers: Default::default(),
        body: None,
        body_error: None,
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pve: Pve::default(),
            filter: Filter::default(),
            sink: default_sink(),
            state_path: default_state_path(),
            backfill_from: None,
        }
    }
}

fn env(key: &str) -> Option<String> {
    match std::env::var(key) {
        // An empty variable means "unset" here. Compose and systemd both make it
        // easy to define a variable with no value, and treating that as an
        // explicit empty override would blank a working config-file setting.
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn env_list(key: &str) -> Option<Vec<String>> {
    env(key).map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
}

fn env_bool(key: &str) -> Option<bool> {
    env(key).map(|v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
}

impl Config {
    pub fn load(path: Option<&PathBuf>) -> Result<Config> {
        let mut cfg = match path {
            Some(p) => {
                let raw =
                    std::fs::read_to_string(p).with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str(&raw).with_context(|| format!("parsing config {}", p.display()))?
            }
            None => Config::default(),
        };
        cfg.apply_env()?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn apply_env(&mut self) -> Result<()> {
        let p = &mut self.pve;
        if let Some(v) = env("PVE_URL") {
            p.url = v;
        }
        if let Some(v) = env("PVE_USER") {
            p.user = v;
        }
        if let Some(v) = env("PVE_TOKEN_NAME") {
            p.token_name = v;
        }
        if let Some(v) = env("PVE_TOKEN_VALUE") {
            p.token_value = v;
        }
        if let Some(v) = env("PVE_NODE") {
            p.node = Some(v);
        }
        if let Some(v) = env("PVE_CA_BUNDLE") {
            p.ca_bundle = Some(PathBuf::from(v));
        }
        if let Some(v) = env_bool("PVE_VERIFY_TLS") {
            p.verify_tls = v;
        }
        if let Some(v) = env("PVE_EVENTS_POLL_INTERVAL") {
            p.poll_interval_secs = v.parse().context("PVE_EVENTS_POLL_INTERVAL")?;
        }

        let f = &mut self.filter;
        if let Some(v) = env_list("PVE_EVENTS_GROUPS") {
            f.groups = v;
        }
        if let Some(v) = env_list("PVE_EVENTS_INCLUDE_TYPES") {
            f.include_types = v;
        }
        if let Some(v) = env_list("PVE_EVENTS_EXCLUDE_TYPES") {
            f.exclude_types = v;
        }
        if let Some(v) = env("PVE_EVENTS_MIN_SEVERITY") {
            f.min_severity = match v.to_ascii_lowercase().as_str() {
                "info" => crate::event::Severity::Info,
                "error" => crate::event::Severity::Error,
                other => bail!("PVE_EVENTS_MIN_SEVERITY must be info or error, got {other:?}"),
            };
        }
        if let Some(v) = env_list("PVE_EVENTS_GUESTS") {
            f.guests = v;
        }
        if let Some(v) = env_list("PVE_EVENTS_EXCLUDE_GUESTS") {
            f.exclude_guests = v;
        }
        if let Some(v) = env_list("PVE_EVENTS_NODES") {
            f.nodes = v;
        }
        if let Some(v) = env_list("PVE_EVENTS_USERS") {
            f.users = v;
        }
        if let Some(v) = env_list("PVE_EVENTS_EXCLUDE_USERS") {
            f.exclude_users = v;
        }

        let s = &mut self.sink;
        if let Some(v) = env("PVE_EVENTS_SINK") {
            s.kind = serde_json::from_value(serde_json::Value::String(v.to_ascii_lowercase()))
                .context("PVE_EVENTS_SINK")?;
        }
        if let Some(v) = env("PVE_EVENTS_URL") {
            s.url = v;
        }
        if let Some(v) = env("PVE_EVENTS_METHOD") {
            s.method = v;
        }
        if let Some(v) = env("PVE_EVENTS_BODY") {
            s.body = Some(v);
        }
        if let Some(v) = env("PVE_EVENTS_BODY_ERROR") {
            s.body_error = Some(v);
        }
        // `Key: value, Other: thing`. Values may not contain a comma; anything
        // that involved belongs in the config file.
        if let Some(items) = env_list("PVE_EVENTS_HEADERS") {
            for item in items {
                let (k, v) = item
                    .split_once(':')
                    .with_context(|| format!("PVE_EVENTS_HEADERS entry {item:?} is not `Key: value`"))?;
                s.headers.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        if let Some(v) = env("PVE_EVENTS_STATE") {
            self.state_path = PathBuf::from(v);
        }
        if let Some(v) = env("PVE_EVENTS_BACKFILL_FROM") {
            self.backfill_from = Some(v.parse().context("PVE_EVENTS_BACKFILL_FROM")?);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("pve.url / PVE_URL", &self.pve.url),
            ("pve.user / PVE_USER", &self.pve.user),
            ("pve.token_name / PVE_TOKEN_NAME", &self.pve.token_name),
            ("pve.token_value / PVE_TOKEN_VALUE", &self.pve.token_value),
        ] {
            if value.is_empty() {
                bail!("{name} is required");
            }
        }
        if self.pve.poll_interval_secs == 0 {
            bail!("poll_interval_secs must be greater than 0");
        }

        // A typo in a group name would otherwise match nothing and the tool
        // would sit there silently forwarding zero events.
        let known = catalogue::all_groups();
        for g in &self.filter.groups {
            if !known.contains(&g.as_str()) {
                bail!("unknown filter group {g:?}; known groups: {}", known.join(", "));
            }
        }
        for t in self.filter.include_types.iter().chain(&self.filter.exclude_types) {
            if !t.ends_with('*') && catalogue::lookup(t).is_none() {
                bail!("unknown task type {t:?} in filter — it is not in the catalogue");
            }
        }
        self.sink.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env is process-global, so these run under one lock and clean up after
    /// themselves rather than racing each other.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const VARS: &[&str] = &[
        "PVE_URL",
        "PVE_USER",
        "PVE_TOKEN_NAME",
        "PVE_TOKEN_VALUE",
        "PVE_NODE",
        "PVE_CA_BUNDLE",
        "PVE_VERIFY_TLS",
        "PVE_EVENTS_POLL_INTERVAL",
        "PVE_EVENTS_GROUPS",
        "PVE_EVENTS_INCLUDE_TYPES",
        "PVE_EVENTS_EXCLUDE_TYPES",
        "PVE_EVENTS_MIN_SEVERITY",
        "PVE_EVENTS_GUESTS",
        "PVE_EVENTS_EXCLUDE_GUESTS",
        "PVE_EVENTS_NODES",
        "PVE_EVENTS_USERS",
        "PVE_EVENTS_EXCLUDE_USERS",
        "PVE_EVENTS_SINK",
        "PVE_EVENTS_URL",
        "PVE_EVENTS_METHOD",
        "PVE_EVENTS_BODY",
        "PVE_EVENTS_BODY_ERROR",
        "PVE_EVENTS_HEADERS",
        "PVE_EVENTS_STATE",
        "PVE_EVENTS_BACKFILL_FROM",
    ];

    fn with_env<T>(pairs: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for v in VARS {
            unsafe { std::env::remove_var(v) };
        }
        for (k, v) in pairs {
            unsafe { std::env::set_var(k, v) };
        }
        let out = f();
        for v in VARS {
            unsafe { std::env::remove_var(v) };
        }
        out
    }

    const CREDS: &[(&str, &str)] = &[
        ("PVE_URL", "https://pve:8006"),
        ("PVE_USER", "monitor@pve"),
        ("PVE_TOKEN_NAME", "events"),
        ("PVE_TOKEN_VALUE", "s3cret"),
    ];

    fn load_with(extra: &[(&str, &str)]) -> Result<Config> {
        let merged: Vec<(&str, &str)> = CREDS.iter().chain(extra.iter()).copied().collect();
        with_env(&merged, || Config::load(None))
    }

    #[test]
    fn env_alone_is_enough_to_run() {
        let cfg = load_with(&[]).unwrap();
        assert_eq!(cfg.pve.user, "monitor@pve");
        assert_eq!(cfg.pve.poll_interval_secs, 20);
        assert_eq!(cfg.filter.groups, vec!["vm", "ct", "disk"]);
    }

    #[test]
    fn missing_credentials_name_the_field_that_is_missing() {
        let err =
            with_env(&[("PVE_URL", "https://pve:8006")], || Config::load(None)).unwrap_err().to_string();
        assert!(err.contains("PVE_USER"), "{err}");
    }

    #[test]
    fn env_overrides_the_config_file() {
        let dir = std::env::temp_dir().join(format!("pve-events-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[pve]
url = "https://from-file:8006"
user = "file@pve"
token_name = "file"
token_value = "file"
poll_interval_secs = 300

[filter]
groups = ["vm"]

[sink]
kind = "stdout"
"#,
        )
        .unwrap();

        let cfg = with_env(&[("PVE_URL", "https://from-env:8006")], || Config::load(Some(&path))).unwrap();
        assert_eq!(cfg.pve.url, "https://from-env:8006");
        // Untouched file settings survive the merge.
        assert_eq!(cfg.pve.user, "file@pve");
        assert_eq!(cfg.pve.poll_interval_secs, 300);
        assert_eq!(cfg.filter.groups, vec!["vm"]);
    }

    #[test]
    fn an_empty_env_var_does_not_blank_a_configured_value() {
        // compose's `PVE_NODE:` and systemd's `Environment=X=` both produce this.
        let cfg = load_with(&[("PVE_URL", "https://kept:8006"), ("PVE_NODE", "  ")]).unwrap();
        assert_eq!(cfg.pve.url, "https://kept:8006");
        assert!(cfg.pve.node.is_none());
    }

    #[test]
    fn a_typo_in_a_group_name_is_rejected_rather_than_matching_nothing() {
        let err = load_with(&[("PVE_EVENTS_GROUPS", "vm,dsk")]).unwrap_err().to_string();
        assert!(err.contains("dsk"), "{err}");
    }

    #[test]
    fn a_typo_in_a_task_type_is_rejected_too() {
        let err = load_with(&[("PVE_EVENTS_EXCLUDE_TYPES", "qmstartt")]).unwrap_err().to_string();
        assert!(err.contains("qmstartt"), "{err}");
        // ...but a wildcard is not a typo.
        assert!(load_with(&[("PVE_EVENTS_EXCLUDE_TYPES", "qm*")]).is_ok());
    }

    #[test]
    fn lists_and_headers_parse_from_comma_separated_env() {
        let cfg = load_with(&[
            ("PVE_EVENTS_EXCLUDE_USERS", "root@pam!terraform, ci@pve"),
            ("PVE_EVENTS_SINK", "webhook"),
            ("PVE_EVENTS_URL", "https://example.invalid/h"),
            ("PVE_EVENTS_BODY", r#"{"t":"{{title}}"}"#),
            ("PVE_EVENTS_HEADERS", "Authorization: Bearer x, X-Src: pve"),
        ])
        .unwrap();
        assert_eq!(cfg.filter.exclude_users, vec!["root@pam!terraform", "ci@pve"]);
        assert_eq!(cfg.sink.headers["Authorization"], "Bearer x");
        assert_eq!(cfg.sink.headers["X-Src"], "pve");
    }

    #[test]
    fn min_severity_rejects_a_value_that_is_neither_info_nor_error() {
        assert!(load_with(&[("PVE_EVENTS_MIN_SEVERITY", "warning")]).is_err());
        assert!(load_with(&[("PVE_EVENTS_MIN_SEVERITY", "ERROR")]).is_ok());
    }

    #[test]
    fn a_sink_without_a_url_is_caught_before_the_loop_starts() {
        let err = load_with(&[("PVE_EVENTS_SINK", "discord")]).unwrap_err().to_string();
        assert!(err.contains("sink.url"), "{err}");
    }
}
