//! The task catalogue: which PVE task types are events, and how to name them.
//!
//! Proxmox's own notification system emits five classes only — `vzdump`,
//! `replication`, `package-updates`, `fencing` and `system-mail`. Guest
//! lifecycle and disk operations are not among them, so no notification matcher
//! can route them. They exist only as rows in the cluster task log, and this
//! module is the mapping from such a row to something worth reading.
//!
//! Membership in [`CATALOGUE`] **is** the filter. A task type that is not listed
//! is never forwarded, so a PVE upgrade that introduces a new task type stays
//! silent rather than emitting unlabelled noise.

/// What a task acted on. Decided by task type, never by the shape of the task's
/// `id` — an `imgdel` on storage `local` has no `@` to key off, so a
/// "does it look like a volume?" heuristic mislabels it as a guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Vm,
    Ct,
    /// A storage volume: `local`, `9999@local-lvm`. Already self-describing.
    Storage,
    Node,
}

#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub emoji: &'static str,
    pub verb: &'static str,
    pub scope: Scope,
    pub group: &'static str,
}

const fn spec(emoji: &'static str, verb: &'static str, scope: Scope, group: &'static str) -> Spec {
    Spec { emoji, verb, scope, group }
}

const fn vm(emoji: &'static str, verb: &'static str) -> Spec {
    spec(emoji, verb, Scope::Vm, "vm")
}

const fn ct(emoji: &'static str, verb: &'static str) -> Spec {
    spec(emoji, verb, Scope::Ct, "ct")
}

/// Disk task carrying a *guest* id (`resize` on VM 1440).
const fn vm_disk(emoji: &'static str, verb: &'static str) -> Spec {
    spec(emoji, verb, Scope::Vm, "disk")
}

/// Disk task carrying a *volume* id (`imgdel` on `9999@local-lvm`).
const fn vol_disk(emoji: &'static str, verb: &'static str) -> Spec {
    spec(emoji, verb, Scope::Storage, "disk")
}

const fn sys(emoji: &'static str, verb: &'static str, scope: Scope) -> Spec {
    spec(emoji, verb, scope, "system")
}

/// Every task type this tool knows how to describe, keyed by PVE's `type` field.
pub static CATALOGUE: &[(&str, Spec)] = &[
    // --- VM lifecycle -------------------------------------------------------
    ("qmstart", vm("▶️", "started")),
    ("qmstop", vm("⏹️", "stopped")),
    ("qmshutdown", vm("🛑", "shut down")),
    ("qmreboot", vm("🔄", "rebooted")),
    ("qmreset", vm("🔄", "reset")),
    ("qmsuspend", vm("⏸️", "suspended")),
    ("qmresume", vm("▶️", "resumed")),
    ("qmpause", vm("⏸️", "paused")),
    ("qmcreate", vm("✨", "created")),
    ("qmdestroy", vm("🗑️", "destroyed")),
    ("qmclone", vm("📋", "cloned")),
    ("qmmigrate", vm("🚚", "migrated")),
    ("qmtemplate", vm("🧬", "converted to a template")),
    ("qmrestore", vm("♻️", "restored")),
    ("qmconfig", vm("⚙️", "reconfigured")),
    ("qmsnapshot", vm("📸", "snapshotted")),
    ("qmdelsnapshot", vm("🗑️", "snapshot deleted")),
    ("qmrollback", vm("⏪", "rolled back")),
    // --- LXC lifecycle ------------------------------------------------------
    ("vzstart", ct("▶️", "started")),
    ("vzstop", ct("⏹️", "stopped")),
    ("vzshutdown", ct("🛑", "shut down")),
    ("vzreboot", ct("🔄", "rebooted")),
    ("vzsuspend", ct("⏸️", "suspended")),
    ("vzresume", ct("▶️", "resumed")),
    ("vzcreate", ct("✨", "created")),
    ("vzdestroy", ct("🗑️", "destroyed")),
    ("vzclone", ct("📋", "cloned")),
    ("vzmigrate", ct("🚚", "migrated")),
    ("vztemplate", ct("🧬", "converted to a template")),
    ("vzrestore", ct("♻️", "restored")),
    ("vzsnapshot", ct("📸", "snapshotted")),
    ("vzdelsnapshot", ct("🗑️", "snapshot deleted")),
    ("vzrollback", ct("⏪", "rolled back")),
    ("vzmount", ct("📂", "mounted")),
    ("vzumount", ct("📁", "unmounted")),
    // --- disk / storage -----------------------------------------------------
    ("resize", vm_disk("📏", "disk resized")),
    ("move_disk", vm_disk("📦", "disk moved")),
    ("move_volume", vm_disk("📦", "volume moved")),
    ("diskadd", vm_disk("➕", "disk added")),
    ("imgdel", vol_disk("🗑️", "disk image deleted")),
    ("imgcopy", vol_disk("📋", "disk image copied")),
    ("imgmove", vol_disk("📦", "disk image moved")),
    ("diskinit", vol_disk("💽", "disk initialised")),
    ("unknownimgdel", vol_disk("🗑️", "orphaned image deleted")),
    // --- groups that are OFF by default -------------------------------------
    // PVE notifies on backups and package updates natively, so enabling these
    // means two notifications per event unless the native matcher is dropped.
    ("vzdump", spec("💾", "backed up", Scope::Vm, "backup")),
    ("aptupdate", sys("📦", "package index updated", Scope::Node)),
    ("srvstart", sys("🟢", "service started", Scope::Node)),
    ("srvstop", sys("🔴", "service stopped", Scope::Node)),
    ("srvrestart", sys("🔄", "service restarted", Scope::Node)),
    ("startall", sys("▶️", "start-all-guests ran", Scope::Node)),
    ("stopall", sys("⏹️", "stop-all-guests ran", Scope::Node)),
    ("download", sys("⬇️", "downloaded to storage", Scope::Storage)),
];

/// Groups enabled unless configured otherwise. `backup` and `system` are left
/// out because PVE already notifies on those natively.
pub const DEFAULT_GROUPS: &[&str] = &["vm", "ct", "disk"];

pub fn lookup(task_type: &str) -> Option<Spec> {
    CATALOGUE.iter().find(|(t, _)| *t == task_type).map(|(_, s)| *s)
}

/// Every group name present in the catalogue, for `--list-groups` and for
/// rejecting a typo in `filter.groups` at startup rather than silently matching
/// nothing.
pub fn all_groups() -> Vec<&'static str> {
    let mut g: Vec<_> = CATALOGUE.iter().map(|(_, s)| s.group).collect();
    g.sort_unstable();
    g.dedup();
    g
}

/// Human label for what a task acted on: `VM 1440`, `CT 102`, `9999@local-lvm`.
pub fn subject(spec: &Spec, task_id: &str, node: &str) -> String {
    match spec.scope {
        Scope::Vm => format!("VM {task_id}"),
        Scope::Ct => format!("CT {task_id}"),
        Scope::Node => format!("node {node}"),
        Scope::Storage => {
            // A storage task without an id is still better named by its node
            // than by an empty string.
            if task_id.is_empty() { node.to_string() } else { task_id.to_string() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_scope_is_decided_by_type_not_by_the_id_shape() {
        // The bug this guards: `imgdel` on storage `local` has no "@", so an
        // id-shape heuristic renders it "VM local".
        let s = lookup("imgdel").unwrap();
        assert_eq!(subject(&s, "local", "pve"), "local");
        assert_eq!(subject(&s, "9999@local-lvm", "pve"), "9999@local-lvm");
    }

    #[test]
    fn resize_carries_a_guest_id_so_it_stays_vm_scoped() {
        let s = lookup("resize").unwrap();
        assert_eq!(s.group, "disk");
        assert_eq!(subject(&s, "1440", "pve"), "VM 1440");
    }

    #[test]
    fn lxc_tasks_are_labelled_ct() {
        let s = lookup("vzstart").unwrap();
        assert_eq!(subject(&s, "102", "pve"), "CT 102");
    }

    #[test]
    fn native_notification_classes_are_not_in_the_default_groups() {
        // vzdump and aptupdate exist in the catalogue but must be opt-in, or the
        // user gets one notification from PVE and one from us.
        for t in ["vzdump", "aptupdate"] {
            let g = lookup(t).unwrap().group;
            assert!(!DEFAULT_GROUPS.contains(&g), "{t} is on by default");
        }
    }

    #[test]
    fn catalogue_has_no_duplicate_keys() {
        let mut seen: Vec<&str> = CATALOGUE.iter().map(|(t, _)| *t).collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "duplicate task type in CATALOGUE");
    }
}
