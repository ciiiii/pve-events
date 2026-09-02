//! A PVE task row, and its rendering into a notifiable event.

use serde::Deserialize;

use crate::catalogue::{self, Spec};

/// One row from `/api2/json/cluster/tasks`.
///
/// Only `upid`, `type` and `node` are guaranteed present; PVE omits `id` for
/// node-scoped tasks and omits `endtime`/`status` while a task is still running.
#[derive(Deserialize, Clone, Debug)]
pub struct Task {
    pub upid: String,
    #[serde(rename = "type")]
    pub task_type: String,
    #[serde(default)]
    pub id: String,
    pub node: String,
    /// The plain user, WITHOUT the API token. PVE reports `root@pam` here even
    /// for a task run as `root@pam!terraform` — see `tokenid`.
    #[serde(default)]
    pub user: String,
    /// Set when an API token ran the task. Kept separate by PVE, which is why
    /// [`Task::identity`] recomposes the two: filtering on `root@pam!*` would
    /// otherwise match nothing at all.
    #[serde(default)]
    pub tokenid: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub starttime: i64,
    /// `None` while the task is still running — such rows are skipped, because
    /// the outcome is not known yet and the next poll will see it finished.
    pub endtime: Option<i64>,
}

impl Task {
    /// The full `user@realm!tokenid` identity, as it appears in the UPID and in
    /// the PVE UI — the form a user will reach for when writing a filter.
    pub fn identity(&self) -> String {
        let user = if self.user.is_empty() { "unknown" } else { &self.user };
        match self.tokenid.as_deref().filter(|t| !t.is_empty()) {
            Some(token) => format!("{user}!{token}"),
            None => user.to_string(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Error => "error",
        }
    }

    /// Decimal RGB, for sinks that colour-code (Discord embeds).
    pub fn color(self) -> u32 {
        match self {
            Severity::Info => 0x3B_A5_5D,
            Severity::Error => 0xD8_3C_3C,
        }
    }
}

/// A task, resolved against the catalogue and rendered.
#[derive(Clone, Debug)]
pub struct Event {
    pub upid: String,
    pub task_type: String,
    pub group: &'static str,
    pub subject: String,
    /// The guest or volume id, verbatim. Empty for node-scoped tasks.
    pub guest: String,
    pub node: String,
    /// Full `user@realm!tokenid` identity — what filters match against.
    pub user: String,
    /// Just the token id, empty when a plain user ran the task.
    pub tokenid: String,
    pub status: String,
    pub severity: Severity,
    pub emoji: &'static str,
    pub verb: &'static str,
    pub title: String,
    pub body: String,
    pub duration_secs: i64,
    pub endtime: i64,
}

impl Event {
    /// `None` when the task type is not in the catalogue.
    pub fn from_task(task: &Task) -> Option<Event> {
        let spec: Spec = catalogue::lookup(&task.task_type)?;
        let endtime = task.endtime.unwrap_or(task.starttime);

        // PVE writes "OK" on success and puts the error text in `status`
        // otherwise, so the message itself is the failure reason.
        let severity = if task.status == "OK" { Severity::Info } else { Severity::Error };

        let subject = catalogue::subject(&spec, &task.id, &task.node);
        let title = match severity {
            Severity::Info => format!("{} {} {}", spec.emoji, subject, spec.verb),
            Severity::Error => format!("⚠️ {} {} — FAILED", subject, spec.verb),
        };

        let identity = task.identity();
        let duration_secs = (endtime - task.starttime).max(0);
        let mut lines = vec![
            format!("*Task*: `{}`", task.task_type),
            format!("*Node*: {}", task.node),
            format!("*By*: {identity}"),
            format!("*Status*: {}", if task.status.is_empty() { "unknown" } else { &task.status }),
        ];
        // Sub-second tasks are the common case; printing "0s" for every start
        // is noise, so the line only appears when it says something.
        if duration_secs > 1 {
            lines.push(format!("*Took*: {duration_secs}s"));
        }

        Some(Event {
            upid: task.upid.clone(),
            task_type: task.task_type.clone(),
            group: spec.group,
            subject,
            guest: task.id.clone(),
            node: task.node.clone(),
            user: identity,
            tokenid: task.tokenid.clone().unwrap_or_default(),
            status: task.status.clone(),
            severity,
            emoji: spec.emoji,
            verb: spec.verb,
            title,
            body: lines.join("\n"),
            duration_secs,
            endtime,
        })
    }

    /// Placeholder lookup for sink body templates. Unknown names return `None`
    /// so the renderer can fail loudly on a typo instead of silently emitting
    /// an empty string into a webhook payload.
    pub fn field(&self, name: &str) -> Option<String> {
        Some(match name {
            "title" => self.title.clone(),
            "body" | "message" => self.body.clone(),
            "severity" => self.severity.as_str().to_string(),
            "color" => self.severity.color().to_string(),
            "emoji" => self.emoji.to_string(),
            "verb" => self.verb.to_string(),
            "subject" => self.subject.clone(),
            "guest" => self.guest.clone(),
            "node" => self.node.clone(),
            "user" => self.user.clone(),
            "tokenid" => self.tokenid.clone(),
            "status" => self.status.clone(),
            "type" => self.task_type.clone(),
            "group" => self.group.to_string(),
            "upid" => self.upid.clone(),
            "duration" => self.duration_secs.to_string(),
            "endtime" => self.endtime.to_string(),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(task_type: &str, id: &str, status: &str) -> Task {
        Task {
            upid: format!("UPID:pve:0:0:0:{task_type}:{id}:root@pam:"),
            task_type: task_type.into(),
            id: id.into(),
            node: "pve".into(),
            user: "root@pam".into(),
            tokenid: None,
            status: status.into(),
            starttime: 1000,
            endtime: Some(1010),
        }
    }

    #[test]
    fn success_and_failure_render_differently() {
        let ok = Event::from_task(&task("qmstart", "1440", "OK")).unwrap();
        assert_eq!(ok.title, "▶️ VM 1440 started");
        assert_eq!(ok.severity, Severity::Info);

        let bad = Event::from_task(&task("qmshutdown", "1400", "VM quit/powerdown failed")).unwrap();
        assert_eq!(bad.title, "⚠️ VM 1400 shut down — FAILED");
        assert_eq!(bad.severity, Severity::Error);
        // The failure reason must survive into the body, not just the severity.
        assert!(bad.body.contains("VM quit/powerdown failed"));
    }

    #[test]
    fn the_api_token_is_recomposed_into_the_identity() {
        // PVE reports user="root@pam" and tokenid="terraform" as SEPARATE fields
        // even though the UPID reads root@pam!terraform. Without recomposing,
        // a filter on `root@pam!*` matches nothing and silently does nothing.
        let mut t = task("qmstart", "1440", "OK");
        t.tokenid = Some("terraform".into());
        assert_eq!(t.identity(), "root@pam!terraform");

        let e = Event::from_task(&t).unwrap();
        assert_eq!(e.user, "root@pam!terraform");
        assert_eq!(e.tokenid, "terraform");
        assert!(e.body.contains("*By*: root@pam!terraform"));
    }

    #[test]
    fn a_plain_user_gets_no_bang_suffix() {
        let mut t = task("qmstart", "1440", "OK");
        assert_eq!(t.identity(), "root@pam");
        // An empty tokenid must behave like an absent one.
        t.tokenid = Some(String::new());
        assert_eq!(t.identity(), "root@pam");
        t.user = String::new();
        t.tokenid = None;
        assert_eq!(t.identity(), "unknown");
    }

    #[test]
    fn unknown_task_types_are_dropped() {
        assert!(Event::from_task(&task("somethingnew", "1", "OK")).is_none());
    }

    #[test]
    fn duration_line_is_omitted_when_it_says_nothing() {
        let mut t = task("qmstart", "1440", "OK");
        t.endtime = Some(t.starttime);
        assert!(!Event::from_task(&t).unwrap().body.contains("Took"));
        assert!(Event::from_task(&task("qmstart", "1440", "OK")).unwrap().body.contains("Took*: 10s"));
    }

    #[test]
    fn unknown_placeholders_are_an_error_not_an_empty_string() {
        let e = Event::from_task(&task("qmstart", "1440", "OK")).unwrap();
        assert!(e.field("title").is_some());
        assert!(e.field("nope").is_none());
    }
}
