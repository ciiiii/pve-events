//! The PVE side: an HTTP agent that trusts the right roots, and one GET.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use ureq::tls::{Certificate, RootCerts, TlsConfig};

use crate::config::Pve;
use crate::event::Task;

#[derive(Deserialize)]
struct TasksResponse {
    data: Vec<Task>,
}

pub struct Client {
    agent: ureq::Agent,
    url: String,
    auth: String,
}

impl Client {
    pub fn new(cfg: &Pve) -> Result<Client> {
        let mut tls = TlsConfig::builder();

        if !cfg.verify_tls {
            tls = tls.disable_verification(true);
        } else if let Some(path) = &cfg.ca_bundle {
            // A Proxmox UI cert is usually issued by a private CA, so the Mozilla
            // root set that ships with rustls cannot verify it. Trusting an
            // explicit root keeps verification on -- the alternative people reach
            // for is disabling it entirely.
            //
            // NOTE: `RootCerts::Specific` REPLACES the root store, it does not
            // extend it. That is why the sink gets its own agent (see sink::agent)
            // -- reusing this one would leave a public https webhook with no web
            // PKI roots to verify against.
            let pem = std::fs::read(path).with_context(|| format!("reading ca_bundle {}", path.display()))?;
            let certs = Certificate::from_pem(&pem)
                .map(|c| vec![c])
                .with_context(|| format!("parsing ca_bundle {}", path.display()))?;
            tls = tls.root_certs(RootCerts::Specific(Arc::new(certs)));
        }

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(tls.build())
            .timeout_global(Some(Duration::from_secs(cfg.timeout_secs)))
            // Read the status ourselves so a 401 reports as "check the token"
            // rather than as a transport error.
            .http_status_as_error(false)
            .build()
            .into();

        Ok(Client {
            agent,
            url: task_url(cfg),
            auth: format!("PVEAPIToken={}!{}={}", cfg.user, cfg.token_name, cfg.token_value),
        })
    }

    /// Recent tasks, newest first as PVE returns them.
    ///
    /// This window is capped (~25-50 rows), which is the one structural limit of
    /// polling: a burst larger than the window inside one interval can age a row
    /// out before it is ever seen.
    pub fn tasks(&self) -> Result<Vec<Task>> {
        let mut resp = self
            .agent
            .get(&self.url)
            .header("Authorization", &self.auth)
            .call()
            .context("GET /cluster/tasks")?;

        let status = resp.status().as_u16();
        if status == 401 {
            bail!("PVE returned 401 — check pve.user / token_name / token_value");
        }
        if status == 403 {
            bail!("PVE returned 403 — the token needs Sys.Audit and VM.Audit (role PVEAuditor)");
        }
        if !(200..300).contains(&status) {
            bail!("PVE returned HTTP {status}");
        }

        Ok(resp.body_mut().read_json::<TasksResponse>()?.data)
    }
}

fn task_url(cfg: &Pve) -> String {
    let base = cfg.url.trim_end_matches('/');
    match &cfg.node {
        // Per-node is useful on a cluster where one node's churn is noise; the
        // cluster-wide endpoint is the default because it needs no node name.
        Some(node) => format!("{base}/api2/json/nodes/{node}/tasks?limit=200"),
        None => format!("{base}/api2/json/cluster/tasks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Pve {
        Pve {
            url: "https://10.0.0.100:8006/".into(),
            user: "prometheus@pve".into(),
            token_name: "events".into(),
            token_value: "secret".into(),
            node: None,
            ca_bundle: None,
            verify_tls: true,
            poll_interval_secs: 20,
            timeout_secs: 15,
        }
    }

    #[test]
    fn trailing_slash_in_url_does_not_double_up() {
        assert_eq!(task_url(&cfg()), "https://10.0.0.100:8006/api2/json/cluster/tasks");
    }

    #[test]
    fn a_configured_node_switches_to_the_per_node_endpoint() {
        let mut c = cfg();
        c.node = Some("pve".into());
        assert!(task_url(&c).contains("/nodes/pve/tasks"));
    }

    #[test]
    fn a_missing_ca_bundle_fails_at_startup_not_on_first_poll() {
        let mut c = cfg();
        c.ca_bundle = Some("/nonexistent/ca.crt".into());
        let Err(err) = Client::new(&c) else { panic!("expected a startup failure") };
        assert!(err.to_string().contains("ca_bundle"), "{err}");
    }

    #[test]
    fn disabling_verification_skips_the_ca_bundle_entirely() {
        let mut c = cfg();
        c.verify_tls = false;
        c.ca_bundle = Some("/nonexistent/ca.crt".into());
        assert!(Client::new(&c).is_ok());
    }
}
