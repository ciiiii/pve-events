//! Persisted watermark and seen-UPID set.
//!
//! Two variables are what make restarts safe:
//!
//! * `started_at` — set to *now* on the very first run and never moved.
//!   `/cluster/tasks` always returns a backlog, so without a watermark every
//!   fresh deploy would fire a burst of notifications for events long past.
//! * `seen` — UPIDs already delivered, so a restart inside the poll window does
//!   not re-send. Bounded, because this file is written on every delivery.
//!
//! A UPID is recorded only *after* the sink accepts it. That ordering is what
//! makes a webhook outage self-healing: the event stays unrecorded and the next
//! poll retries it, for as long as it remains in the window PVE returns.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Well above the ~25–50 rows `/cluster/tasks` returns, so nothing is ever
/// re-sent because it fell out of the set while still visible to the API.
const SEEN_LIMIT: usize = 1000;

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    pub started_at: i64,
    /// Insertion-ordered, oldest first, so trimming drops the oldest.
    seen: Vec<String>,
    #[serde(skip)]
    index: HashSet<String>,
    #[serde(skip)]
    path: PathBuf,
}

impl State {
    /// Loads existing state, or creates fresh state watermarked at `now`.
    ///
    /// A corrupt or truncated file is treated as absent: refusing to start
    /// because of an unreadable cache would be worse than losing the watermark,
    /// and the `fresh` flag lets the caller say so in the log.
    pub fn load(path: &Path, now: i64) -> (State, bool) {
        let parsed =
            std::fs::read_to_string(path).ok().and_then(|raw| serde_json::from_str::<State>(&raw).ok());

        match parsed {
            Some(mut s) => {
                s.index = s.seen.iter().cloned().collect();
                s.path = path.to_path_buf();
                (s, false)
            }
            None => (
                State { started_at: now, seen: Vec::new(), index: HashSet::new(), path: path.to_path_buf() },
                true,
            ),
        }
    }

    pub fn has_seen(&self, upid: &str) -> bool {
        self.index.contains(upid)
    }

    pub fn record(&mut self, upid: &str) -> Result<()> {
        if !self.index.insert(upid.to_string()) {
            return Ok(());
        }
        self.seen.push(upid.to_string());
        if self.seen.len() > SEEN_LIMIT {
            for old in self.seen.drain(..self.seen.len() - SEEN_LIMIT) {
                self.index.remove(&old);
            }
        }
        self.persist()
    }

    /// Write via a temp file and rename, so a crash mid-write cannot leave
    /// truncated JSON where the watermark used to be.
    pub fn persist(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming into {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own per test — these run in parallel, and a shared
    /// one wiped at setup means whichever test starts second deletes the other's
    /// state mid-write.
    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pve-events-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn first_run_watermarks_at_now_and_reports_itself_fresh() {
        let path = tmpdir("first").join("seen.json");
        let (s, fresh) = State::load(&path, 1_700_000_000);
        assert!(fresh);
        assert_eq!(s.started_at, 1_700_000_000);
    }

    #[test]
    fn watermark_and_seen_survive_a_restart() {
        let path = tmpdir("restart").join("restart.json");
        let (mut s, _) = State::load(&path, 42);
        s.record("UPID:a").unwrap();

        // A later "now" must not move the watermark, or the restart would
        // re-skip events that arrived while the process was down.
        let (s2, fresh) = State::load(&path, 999);
        assert!(!fresh);
        assert_eq!(s2.started_at, 42);
        assert!(s2.has_seen("UPID:a"));
    }

    #[test]
    fn seen_set_is_trimmed_but_index_stays_consistent() {
        let path = tmpdir("trim").join("trim.json");
        let (mut s, _) = State::load(&path, 0);
        for i in 0..SEEN_LIMIT + 10 {
            s.record(&format!("UPID:{i}")).unwrap();
        }
        assert_eq!(s.seen.len(), SEEN_LIMIT);
        assert_eq!(s.index.len(), SEEN_LIMIT);
        assert!(!s.has_seen("UPID:0"), "oldest should be trimmed");
        assert!(s.has_seen(&format!("UPID:{}", SEEN_LIMIT + 9)));
    }

    #[test]
    fn recording_the_same_upid_twice_is_a_no_op() {
        let path = tmpdir("dup").join("dup.json");
        let (mut s, _) = State::load(&path, 0);
        s.record("UPID:a").unwrap();
        s.record("UPID:a").unwrap();
        assert_eq!(s.seen.len(), 1);
    }

    #[test]
    fn corrupt_state_is_treated_as_absent_rather_than_fatal() {
        let path = tmpdir("corrupt").join("corrupt.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let (s, fresh) = State::load(&path, 7);
        assert!(fresh);
        assert_eq!(s.started_at, 7);
    }
}
