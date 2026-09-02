//! pve-events — forward Proxmox VE guest and disk task events to any webhook.
//!
//! Proxmox's notification system emits five classes (vzdump, replication,
//! package-updates, fencing, system-mail) and none of them is "a VM started".
//! Those events live only in the cluster task log, so this polls it.
//!
//! Log lines carry no timestamps on purpose: docker, podman and journald all add
//! their own, and a second one inside the message is just noise.

mod catalogue;
mod config;
mod event;
mod filter;
mod pve;
mod sink;
mod state;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::event::{Event, Task};
use crate::filter::Filter;
use crate::state::State;

const USAGE: &str = "\
pve-events — forward Proxmox VE guest and disk task events to any webhook

USAGE:
    pve-events [OPTIONS]

OPTIONS:
    -c, --config <PATH>   TOML config file (optional; env alone is enough)
        --dry-run         Render events to stdout instead of sending them
        --once            Poll once and exit, instead of looping
        --list-groups     Print the task types in each filter group and exit
    -h, --help            Print this help
    -V, --version         Print version

Configuration is documented at https://github.com/ciiiii/pve-events
";

struct Args {
    config: Option<PathBuf>,
    dry_run: bool,
    once: bool,
}

/// `Ok(None)` means a flag already did its job (help/version/list) and the
/// process should exit successfully without running the loop.
fn parse_args() -> Result<Option<Args>> {
    let mut args = Args { config: None, dry_run: false, once: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                args.config = Some(PathBuf::from(it.next().context("--config needs a path")?));
            }
            "--dry-run" => args.dry_run = true,
            "--once" => args.once = true,
            "--list-groups" => {
                print_groups();
                return Ok(None);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("pve-events {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(Some(args))
}

fn print_groups() {
    for group in catalogue::all_groups() {
        let types: Vec<&str> =
            catalogue::CATALOGUE.iter().filter(|(_, s)| s.group == group).map(|(t, _)| *t).collect();
        let default = if catalogue::DEFAULT_GROUPS.contains(&group) { " (on by default)" } else { "" };
        println!("{group}{default}:\n  {}\n", types.join(" "));
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// One pass over a batch of tasks; returns how many were delivered.
///
/// `deliver` is a parameter so the whole pipeline is testable without a network.
fn process(
    tasks: &[Task],
    filter: &Filter,
    state: &mut State,
    deliver: &mut dyn FnMut(&Event) -> Result<()>,
) -> usize {
    let mut sent = 0;

    // Oldest first, so a burst arrives in the order it actually happened.
    let mut ordered: Vec<&Task> = tasks.iter().collect();
    ordered.sort_by_key(|t| t.endtime.unwrap_or(i64::MAX));

    for task in ordered {
        // Still running: the outcome is not known yet, and the next poll will see
        // it finished. Skipping *without recording* is what makes that work.
        let Some(endtime) = task.endtime else { continue };
        if endtime < state.started_at || state.has_seen(&task.upid) {
            continue;
        }
        let Some(ev) = Event::from_task(task) else { continue };
        if !filter.allows(&ev) {
            // Recorded anyway: a filtered event is a decision, not a failure, and
            // re-evaluating it every poll only produces log noise.
            let _ = state.record(&task.upid);
            continue;
        }
        match deliver(&ev) {
            Ok(()) => {
                eprintln!("sent: {}", ev.title);
                if let Err(e) = state.record(&task.upid) {
                    // Delivery already happened; failing to persist only risks a
                    // duplicate after a restart. Worth shouting about, not dying.
                    eprintln!("warn: could not persist state: {e:#}");
                }
                sent += 1;
            }
            // Deliberately not recorded, so the next poll retries for as long as
            // PVE still returns the row.
            Err(e) => eprintln!("error: delivering {}: {e:#}", task.upid),
        }
    }
    sent
}

fn run() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };

    let mut cfg = Config::load(args.config.as_ref())?;
    if args.dry_run {
        cfg.sink.kind = sink::SinkKind::Stdout;
    }

    let client = pve::Client::new(&cfg.pve)?;
    let sink_agent = sink::agent(cfg.pve.timeout_secs);
    let watermark = cfg.backfill_from.unwrap_or_else(now);
    let (mut state, fresh) = State::load(&cfg.state_path, watermark);
    if fresh {
        state.persist().with_context(|| format!("writing {}", cfg.state_path.display()))?;
        match cfg.backfill_from {
            None => eprintln!(
                "no state at {} — starting from now; existing task history is not replayed",
                cfg.state_path.display()
            ),
            Some(from) => eprintln!("no state at {} — backfilling from {from}", cfg.state_path.display()),
        }
    }

    eprintln!(
        "watching {} every {}s → {:?} sink; groups: {}",
        cfg.pve.url,
        cfg.pve.poll_interval_secs,
        cfg.sink.kind,
        cfg.filter.groups.join(",")
    );

    let interval = Duration::from_secs(cfg.pve.poll_interval_secs);
    loop {
        match client.tasks() {
            Ok(tasks) => {
                process(&tasks, &cfg.filter, &mut state, &mut |ev| cfg.sink.deliver(&sink_agent, ev));
            }
            // A hypervisor reboot or a network blip must not end the process —
            // that is the exact window whose events matter most.
            Err(e) => eprintln!("error: poll failed: {e:#}"),
        }
        if args.once {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(upid: &str, task_type: &str, id: &str, endtime: Option<i64>) -> Task {
        Task {
            upid: upid.into(),
            task_type: task_type.into(),
            id: id.into(),
            node: "pve".into(),
            user: "root@pam".into(),
            tokenid: None,
            status: "OK".into(),
            starttime: 100,
            endtime,
        }
    }

    fn state_at(started_at: i64) -> State {
        let path = std::env::temp_dir()
            .join(format!("pve-events-main-{}-{started_at}", std::process::id()))
            .join("state.json");
        let _ = std::fs::remove_file(&path);
        let (mut s, _) = State::load(&path, started_at);
        s.started_at = started_at;
        s
    }

    #[test]
    fn running_tasks_are_skipped_and_not_recorded() {
        let mut st = state_at(0);
        let tasks = vec![task("UPID:running", "qmstart", "100", None)];
        let mut seen = vec![];
        process(&tasks, &Filter::default(), &mut st, &mut |e| {
            seen.push(e.title.clone());
            Ok(())
        });
        assert!(seen.is_empty());
        // Must stay unrecorded, or the finished row would never be delivered.
        assert!(!st.has_seen("UPID:running"));
    }

    #[test]
    fn events_older_than_the_watermark_are_never_replayed() {
        let mut st = state_at(500);
        let tasks = vec![
            task("UPID:old", "qmstart", "100", Some(499)),
            task("UPID:new", "qmstart", "101", Some(501)),
        ];
        let mut seen = vec![];
        process(&tasks, &Filter::default(), &mut st, &mut |e| {
            seen.push(e.guest.clone());
            Ok(())
        });
        assert_eq!(seen, vec!["101"]);
    }

    #[test]
    fn a_burst_is_delivered_oldest_first() {
        let mut st = state_at(0);
        let tasks = vec![
            task("UPID:c", "qmstart", "3", Some(300)),
            task("UPID:a", "qmstart", "1", Some(100)),
            task("UPID:b", "qmstart", "2", Some(200)),
        ];
        let mut seen = vec![];
        process(&tasks, &Filter::default(), &mut st, &mut |e| {
            seen.push(e.guest.clone());
            Ok(())
        });
        assert_eq!(seen, vec!["1", "2", "3"]);
    }

    #[test]
    fn a_failed_delivery_is_retried_on_the_next_poll() {
        let mut st = state_at(0);
        let tasks = vec![task("UPID:a", "qmstart", "1", Some(100))];

        let mut attempts = 0;
        process(&tasks, &Filter::default(), &mut st, &mut |_| {
            attempts += 1;
            anyhow::bail!("webhook down")
        });
        assert_eq!(attempts, 1);
        assert!(!st.has_seen("UPID:a"), "a failed send must not be recorded");

        // Same batch again: it is retried, and this time it sticks.
        process(&tasks, &Filter::default(), &mut st, &mut |_| Ok(()));
        assert!(st.has_seen("UPID:a"));

        // And is not sent a third time.
        let mut third = 0;
        process(&tasks, &Filter::default(), &mut st, &mut |_| {
            third += 1;
            Ok(())
        });
        assert_eq!(third, 0);
    }

    #[test]
    fn filtered_events_are_recorded_so_they_are_not_reconsidered() {
        let mut st = state_at(0);
        let tasks = vec![task("UPID:dump", "vzdump", "1", Some(100))];
        process(&tasks, &Filter::default(), &mut st, &mut |_| panic!("must not deliver a filtered event"));
        assert!(st.has_seen("UPID:dump"));
    }

    #[test]
    fn unknown_task_types_are_ignored_without_erroring() {
        let mut st = state_at(0);
        let tasks = vec![task("UPID:x", "somefuturetask", "1", Some(100))];
        let sent = process(&tasks, &Filter::default(), &mut st, &mut |_| {
            panic!("must not deliver an uncatalogued task")
        });
        assert_eq!(sent, 0);
    }
}
