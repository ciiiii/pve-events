//! Which events get forwarded.
//!
//! Every list is optional and every list supports a trailing `*` wildcard, so
//! `root@pam!*` silences all API-token activity (a Terraform apply generating a
//! dozen tasks is the usual reason someone reaches for this) without having to
//! enumerate token names.

use serde::Deserialize;

use crate::catalogue::DEFAULT_GROUPS;
use crate::event::{Event, Severity};

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// Catalogue groups to enable: `vm`, `ct`, `disk`, `backup`, `system`.
    #[serde(default = "default_groups")]
    pub groups: Vec<String>,
    /// Task types to forward even though their group is disabled.
    #[serde(default)]
    pub include_types: Vec<String>,
    /// Task types to drop even though their group is enabled. Wins over
    /// `include_types` — a deny is always the safer resolution of a conflict.
    #[serde(default)]
    pub exclude_types: Vec<String>,
    /// `info` forwards everything; `error` forwards only failed tasks.
    #[serde(default = "default_severity")]
    pub min_severity: Severity,
    /// Guest/volume ids to forward. Empty means all.
    #[serde(default)]
    pub guests: Vec<String>,
    #[serde(default)]
    pub exclude_guests: Vec<String>,
    /// Cluster nodes to forward. Empty means all.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// PVE users to forward, e.g. `root@pam`, `terraform@pve!ci`. Empty means all.
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub exclude_users: Vec<String>,
}

fn default_groups() -> Vec<String> {
    DEFAULT_GROUPS.iter().map(|s| s.to_string()).collect()
}

fn default_severity() -> Severity {
    Severity::Info
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            groups: default_groups(),
            include_types: Vec::new(),
            exclude_types: Vec::new(),
            min_severity: default_severity(),
            guests: Vec::new(),
            exclude_guests: Vec::new(),
            nodes: Vec::new(),
            users: Vec::new(),
            exclude_users: Vec::new(),
        }
    }
}

/// Exact match, or prefix match when the pattern ends in `*`.
fn matches(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

fn any_matches(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|p| matches(p, value))
}

/// True when `patterns` is empty (unset = allow all) or something matches.
fn allowed_by(patterns: &[String], value: &str) -> bool {
    patterns.is_empty() || any_matches(patterns, value)
}

impl Filter {
    pub fn allows(&self, e: &Event) -> bool {
        if any_matches(&self.exclude_types, &e.task_type) {
            return false;
        }
        let type_enabled =
            self.groups.iter().any(|g| g == e.group) || any_matches(&self.include_types, &e.task_type);
        if !type_enabled {
            return false;
        }
        if e.severity < self.min_severity {
            return false;
        }
        if any_matches(&self.exclude_users, &e.user) || !allowed_by(&self.users, &e.user) {
            return false;
        }
        if any_matches(&self.exclude_guests, &e.guest) || !allowed_by(&self.guests, &e.guest) {
            return false;
        }
        allowed_by(&self.nodes, &e.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Task};

    fn ev(task_type: &str, id: &str, status: &str, user: &str) -> Event {
        Event::from_task(&Task {
            upid: "UPID:pve:0:0:0:x:y:z:".into(),
            task_type: task_type.into(),
            id: id.into(),
            node: "pve".into(),
            user: user.into(),
            tokenid: None,
            status: status.into(),
            starttime: 0,
            endtime: Some(1),
        })
        .unwrap()
    }

    #[test]
    fn defaults_pass_guest_and_disk_events_but_not_backups() {
        let f = Filter::default();
        assert!(f.allows(&ev("qmstart", "1440", "OK", "root@pam")));
        assert!(f.allows(&ev("vzstart", "102", "OK", "root@pam")));
        assert!(f.allows(&ev("imgdel", "local", "OK", "root@pam")));
        assert!(!f.allows(&ev("vzdump", "1440", "OK", "root@pam")));
        assert!(!f.allows(&ev("aptupdate", "", "OK", "root@pam")));
    }

    #[test]
    fn min_severity_error_keeps_only_failures() {
        let f = Filter { min_severity: Severity::Error, ..Default::default() };
        assert!(!f.allows(&ev("qmstart", "1440", "OK", "root@pam")));
        assert!(f.allows(&ev("qmstart", "1440", "start failed", "root@pam")));
    }

    #[test]
    fn wildcard_silences_all_api_token_activity() {
        // The motivating case: one Terraform apply emits a dozen tasks.
        let f = Filter { exclude_users: vec!["root@pam!*".into()], ..Default::default() };
        assert!(!f.allows(&ev("qmstart", "1440", "OK", "root@pam!terraform")));
        assert!(f.allows(&ev("qmstart", "1440", "OK", "root@pam")));
    }

    #[test]
    fn include_types_opts_a_single_type_back_in() {
        let f = Filter { include_types: vec!["vzdump".into()], ..Default::default() };
        assert!(f.allows(&ev("vzdump", "1440", "OK", "root@pam")));
        assert!(!f.allows(&ev("aptupdate", "", "OK", "root@pam")));
    }

    #[test]
    fn exclude_wins_over_include() {
        let f = Filter {
            include_types: vec!["vzdump".into()],
            exclude_types: vec!["vzdump".into()],
            ..Default::default()
        };
        assert!(!f.allows(&ev("vzdump", "1440", "OK", "root@pam")));
    }

    #[test]
    fn guest_allowlist_is_all_or_listed() {
        let f = Filter { guests: vec!["1440".into()], ..Default::default() };
        assert!(f.allows(&ev("qmstart", "1440", "OK", "root@pam")));
        assert!(!f.allows(&ev("qmstart", "1410", "OK", "root@pam")));
    }

    #[test]
    fn empty_list_means_allow_all_not_deny_all() {
        // Guards the classic inversion: an unset allowlist must not block
        // everything, which would make the tool silently do nothing.
        let f = Filter::default();
        assert!(f.guests.is_empty() && f.nodes.is_empty() && f.users.is_empty());
        assert!(f.allows(&ev("qmstart", "1440", "OK", "anyone@pve")));
    }
}
