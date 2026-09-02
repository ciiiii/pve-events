//! Where events go, and in what shape.
//!
//! A preset is nothing but a default body template plus a default `Content-Type`
//! — everything a preset sets can be overridden in config. `kind = "webhook"`
//! with your own `body` template covers anything not listed.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::event::{Event, Severity};

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SinkKind {
    /// Generic: you supply `body`.
    Webhook,
    Discord,
    Slack,
    Ntfy,
    Gotify,
    Telegram,
    /// `{title, message, status, target}` — the shape hub-api/notify accepts.
    Portal,
    /// Render to stdout and send nothing. For `--dry-run` and for testing a filter.
    Stdout,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Sink {
    pub kind: SinkKind,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Body template. Overrides the preset's.
    #[serde(default)]
    pub body: Option<String>,
    /// Body template used when the task failed. Falls back to `body`.
    #[serde(default)]
    pub body_error: Option<String>,
}

fn default_method() -> String {
    "POST".into()
}

impl Sink {
    /// `(body_template, content_type)` for the preset.
    fn preset(&self, severity: Severity) -> (&'static str, &'static str) {
        const JSON: &str = "application/json";
        match self.kind {
            // `status` is what keeps routine start/stop churn out of the portal's
            // unread badge while leaving failures sitting there unread.
            SinkKind::Portal => match severity {
                Severity::Info => (
                    r#"{"title":"{{title}}","message":"{{body}}","status":"none","target":"telegram"}"#,
                    JSON,
                ),
                Severity::Error => (
                    r#"{"title":"{{title}}","message":"{{body}}","status":"unread","target":"telegram"}"#,
                    JSON,
                ),
            },
            SinkKind::Discord => (
                r#"{"username":"pve-events","embeds":[{"title":"{{title}}","description":"{{body}}","color":{{raw:color}}}]}"#,
                JSON,
            ),
            SinkKind::Slack => (r#"{"text":"*{{title}}*\n{{body}}"}"#, JSON),
            SinkKind::Gotify => (r#"{"title":"{{title}}","message":"{{body}}","priority":5}"#, JSON),
            // ntfy takes the topic from the URL path and the title from a header,
            // so the body is the message verbatim.
            SinkKind::Ntfy => ("{{raw:body}}", "text/plain; charset=utf-8"),
            SinkKind::Telegram => (r#"{"text":"*{{title}}*\n\n{{body}}","parse_mode":"Markdown"}"#, JSON),
            SinkKind::Webhook | SinkKind::Stdout => ("", JSON),
        }
    }

    fn template(&self, severity: Severity) -> Result<String> {
        if severity == Severity::Error
            && let Some(t) = &self.body_error
        {
            return Ok(t.clone());
        }
        if let Some(t) = &self.body {
            return Ok(t.clone());
        }
        let (preset, _) = self.preset(severity);
        if preset.is_empty() {
            // stdout never sends a body, so it needs no template -- only webhook
            // genuinely cannot proceed without one.
            if self.kind == SinkKind::Stdout {
                return Ok(String::new());
            }
            bail!("sink.kind = \"webhook\" has no built-in body; set sink.body to a template");
        }
        Ok(preset.to_string())
    }

    /// Validate at startup rather than on the first event — a bad template
    /// should stop the process, not silently drop notifications at 3am.
    pub fn validate(&self) -> Result<()> {
        if self.kind != SinkKind::Stdout && self.url.is_empty() {
            bail!("sink.url is required for kind = \"{:?}\"", self.kind);
        }
        for sev in [Severity::Info, Severity::Error] {
            let t = self.template(sev)?;
            render(&t, &probe_event()).with_context(|| format!("sink body template ({})", sev.as_str()))?;
        }
        Ok(())
    }

    pub fn deliver(&self, agent: &ureq::Agent, e: &Event) -> Result<()> {
        let body = render(&self.template(e.severity)?, e)?;

        if self.kind == SinkKind::Stdout {
            println!("{}\n{}\n", e.title, e.body);
            return Ok(());
        }

        let (_, content_type) = self.preset(e.severity);
        // ureq's typed helpers are per-verb (get/post/...), so an arbitrary
        // configured method goes through the http::Request path instead.
        let mut req = ureq::http::Request::builder()
            .method(self.method.as_str())
            .uri(&self.url)
            .header("Content-Type", content_type);
        if self.kind == SinkKind::Ntfy {
            // HTTP header values must be ASCII; ntfy titles carry emoji. Losing
            // the emoji from the header is cosmetic, a rejected request is not.
            req = req.header("X-Title", ascii_only(&e.title));
            if e.severity == Severity::Error {
                req = req.header("X-Priority", "4");
            }
        }
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let req = req.body(body.as_bytes()).context("building sink request")?;
        let resp = agent.run(req).context("sending to sink")?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("sink returned HTTP {status}");
        }
        Ok(())
    }
}

/// The sink's own agent, with the default web PKI roots.
///
/// Deliberately NOT the PVE client's agent: configuring a private `ca_bundle`
/// there *replaces* the root store, which would leave a public https webhook
/// with nothing to verify against.
pub fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
        // Status is inspected in `deliver`, so a 5xx reports as "sink returned
        // HTTP 502" rather than as an opaque transport error.
        .http_status_as_error(false)
        .build()
        .into()
}

fn ascii_only(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).collect::<String>().trim().to_string()
}

/// Substitute `{{field}}` (JSON-string-escaped) and `{{raw:field}}` (verbatim).
///
/// Escaping by default is the safe direction: the overwhelming majority of
/// bodies are JSON, and a task status containing a quote — PVE error strings
/// routinely do — would otherwise produce invalid JSON that the receiving end
/// rejects with a 400 nobody sees.
pub fn render(template: &str, e: &Event) -> Result<String> {
    let mut out = String::with_capacity(template.len() + 128);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| anyhow!("unclosed {{{{ in template"))?;
        let key = after[..end].trim();
        let (name, raw) = match key.strip_prefix("raw:") {
            Some(n) => (n.trim(), true),
            None => (key, false),
        };
        let value = e.field(name).ok_or_else(|| anyhow!("unknown template placeholder {{{{{name}}}}}"))?;
        if raw {
            out.push_str(&value);
        } else {
            // serde_json gives us a quoted string; the template supplies its own
            // quotes, so strip them.
            let quoted = serde_json::to_string(&value)?;
            out.push_str(&quoted[1..quoted.len() - 1]);
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A representative event used only to typecheck templates at startup.
fn probe_event() -> Event {
    Event {
        upid: "UPID:pve:0:0:0:qmstart:100:root@pam:".into(),
        task_type: "qmstart".into(),
        group: "vm",
        subject: "VM 100".into(),
        guest: "100".into(),
        node: "pve".into(),
        user: "root@pam".into(),
        tokenid: String::new(),
        status: "OK".into(),
        severity: Severity::Info,
        emoji: "▶️",
        verb: "started",
        title: "▶️ VM 100 started".into(),
        body: "*Task*: `qmstart`".into(),
        duration_secs: 1,
        endtime: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(kind: SinkKind) -> Sink {
        Sink {
            kind,
            url: "https://example.invalid/hook".into(),
            method: default_method(),
            headers: BTreeMap::new(),
            body: None,
            body_error: None,
        }
    }

    fn event(status: &str) -> Event {
        let mut e = probe_event();
        if status != "OK" {
            e.severity = Severity::Error;
            e.status = status.into();
            e.title = "⚠️ VM 100 started — FAILED".into();
        }
        e
    }

    #[test]
    fn every_preset_renders_valid_json_for_both_severities() {
        for kind in
            [SinkKind::Portal, SinkKind::Discord, SinkKind::Slack, SinkKind::Gotify, SinkKind::Telegram]
        {
            for status in ["OK", "failed"] {
                let s = sink(kind);
                let e = event(status);
                let out = render(&s.template(e.severity).unwrap(), &e).unwrap();
                serde_json::from_str::<serde_json::Value>(&out)
                    .unwrap_or_else(|err| panic!("{kind:?}/{status} produced invalid JSON: {err}\n{out}"));
            }
        }
    }

    #[test]
    fn quotes_in_a_pve_error_string_do_not_break_the_json_body() {
        // Real PVE status text, verbatim -- this is the case that made escaping
        // the default rather than an opt-in.
        let mut e = event("failed");
        e.body = "*Status*: Configuration file 'nodes/pve/qemu-server/106.conf' does not exist".into();
        e.title = "⚠️ \"quoted\" \\ backslash".into();
        let out = render(&sink(SinkKind::Portal).template(e.severity).unwrap(), &e).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["title"], "⚠️ \"quoted\" \\ backslash");
    }

    #[test]
    fn portal_preset_keeps_successes_out_of_the_unread_badge() {
        let s = sink(SinkKind::Portal);
        let ok = render(&s.template(Severity::Info).unwrap(), &event("OK")).unwrap();
        let bad = render(&s.template(Severity::Error).unwrap(), &event("failed")).unwrap();
        assert!(ok.contains(r#""status":"none""#));
        assert!(bad.contains(r#""status":"unread""#));
    }

    #[test]
    fn raw_prefix_emits_an_unquoted_number_for_discord_color() {
        let out = render("{{raw:color}}", &event("OK")).unwrap();
        assert_eq!(out, Severity::Info.color().to_string());
        // Without raw: it would be quoted, and Discord rejects a string colour.
        assert_eq!(render("{{color}}", &event("OK")).unwrap(), out);
    }

    #[test]
    fn unknown_placeholder_is_rejected_rather_than_silently_empty() {
        let err = render("{{nope}}", &event("OK")).unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
        assert!(render("{{title", &event("OK")).is_err(), "unclosed braces must error");
    }

    #[test]
    fn webhook_kind_demands_a_template_instead_of_sending_an_empty_body() {
        let err = sink(SinkKind::Webhook).validate().unwrap_err().to_string();
        assert!(err.contains("sink.body"), "{err}");
    }

    #[test]
    fn validate_catches_a_bad_custom_template_at_startup() {
        let mut s = sink(SinkKind::Webhook);
        s.body = Some("{{typo}}".into());
        assert!(s.validate().is_err());
        s.body = Some(r#"{"t":"{{title}}"}"#.into());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn a_url_is_required_unless_writing_to_stdout() {
        let mut s = sink(SinkKind::Portal);
        s.url = String::new();
        assert!(s.validate().is_err());

        let mut s = sink(SinkKind::Stdout);
        s.url = String::new();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn ntfy_header_value_is_stripped_to_ascii() {
        assert_eq!(ascii_only("▶️ VM 100 started"), "VM 100 started");
    }
}
