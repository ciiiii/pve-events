# pve-events

Forward Proxmox VE **guest lifecycle and disk events** to any webhook.

```
▶️  VM 1440 started          ✨ VM 1440 created         📏 VM 1440 disk resized
🛑  CT 102 shut down         🗑️ 9999@local-lvm deleted   ⚠️ VM 1400 shut down — FAILED
```

A single static binary in a ~4 MB `scratch` image. No agent on the hypervisor,
no runtime, no sidecar — it talks to the Proxmox API over the network.

## Why this exists

Proxmox VE 8.3+ has a perfectly good webhook notification target. It just cannot
send any of the above.

The notification system emits **five event classes** — `vzdump`, `replication`,
`package-updates`, `fencing`, `system-mail` — and that is the complete list. On a
PVE 9.2 host, `/usr/share/pve-manager/templates/default/` contains templates for
exactly those five, and the only callers of `PVE::Notify::*` are `APT.pm`,
`Replication.pm`, `VZDump.pm` and `HA/Env/PVE2.pm`.

There is no notification event for "a VM started", so no matcher can route one.

Those events exist only in the **cluster task log**. `pve-events` polls it,
resolves each row against a catalogue, and forwards what you asked for.

## How it works

```
GET /api2/json/cluster/tasks          every 20s, PVEAPIToken auth
        ↓
   drop: still running · already sent · older than the watermark · not in the catalogue
        ↓
   render: catalogue[type] → emoji, verb, scope → title + body, info|error
        ↓
   sink: discord │ slack │ ntfy │ gotify │ telegram │ portal │ webhook │ stdout
        ↓
   record the UPID — only after the sink returns 2xx
```

Recording *after* delivery is what makes a webhook outage self-healing: a failed
send stays unrecorded and the next poll retries it.

On a **first run the watermark is set to now** and nothing is replayed. The API
always returns a backlog, and firing a burst of stale notifications on every
fresh deploy is not a useful default. `backfill_from = 0` overrides it.

## Quick start

```bash
# On the Proxmox host: a read-only token is all this needs.
pveum user add monitor@pve
pveum acl modify / --users monitor@pve --roles PVEAuditor
pveum user token add monitor@pve events --privsep 0
```

`PVEAuditor` grants the `Sys.Audit` + `VM.Audit` that `/cluster/tasks` requires.

```bash
docker run -d --name pve-events --restart unless-stopped \
  -v pve-events-state:/data \
  -e PVE_URL=https://10.0.0.100:8006 \
  -e PVE_USER=monitor@pve \
  -e PVE_TOKEN_NAME=events \
  -e PVE_TOKEN_VALUE=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx \
  -e PVE_EVENTS_SINK=discord \
  -e PVE_EVENTS_URL=https://discord.com/api/webhooks/... \
  ghcr.io/ciiiii/pve-events:latest
```

See what it would send, without sending anything:

```bash
docker run --rm --env-file pve-events.env \
  -e PVE_EVENTS_BACKFILL_FROM=0 \
  ghcr.io/ciiiii/pve-events:latest --once --dry-run
```

### TLS

The Proxmox UI certificate is self-signed or issued by a private CA, which the
built-in Mozilla roots cannot verify. Mount that CA rather than disabling
verification:

```bash
-v /path/to/ca.crt:/etc/ssl/pve-ca.crt:ro -e PVE_CA_BUNDLE=/etc/ssl/pve-ca.crt
```

`PVE_VERIFY_TLS=false` exists, but it is the last resort.

## Sinks

`kind` picks a preset; a preset is only a default body template plus a
`Content-Type`, and every part of it can be overridden.

| kind | `url` is | notes |
|---|---|---|
| `discord` | the webhook URL | embed, colour-coded by severity |
| `slack` | the webhook URL | |
| `ntfy` | `https://ntfy.sh/<topic>` | title goes in `X-Title`; failures get priority 4 |
| `gotify` | `https://gotify/message?token=...` | |
| `telegram` | `https://api.telegram.org/bot<TOKEN>/sendMessage?chat_id=<ID>` | Markdown |
| `portal` | any endpoint taking `{title, message, status, target}` | successes are sent `status: none` so routine churn stays out of an unread badge |
| `webhook` | anything | you supply `body` |
| `stdout` | — | prints; what `--dry-run` switches to |

### Custom payloads

```toml
[sink]
kind = "webhook"
url = "https://example.com/hook"
body = '''
{"text": "{{title}}", "level": "{{severity}}", "vm": "{{guest}}"}
'''
# Optional: a different shape for failures.
body_error = '''
{"text": "{{title}}", "level": "error", "page_me": true}
'''

[sink.headers]
Authorization = "Bearer hunter2"
```

`{{field}}` is **JSON-string-escaped** — PVE error text routinely contains
apostrophes and quotes (`Configuration file 'nodes/pve/qemu-server/106.conf'
does not exist`), and an unescaped substitution produces invalid JSON that the
receiving end rejects with a 400 nobody ever sees. Use `{{raw:field}}` for
numbers and pre-built JSON.

Available fields: `title` `body` `severity` `color` `emoji` `verb` `subject`
`guest` `node` `user` `tokenid` `status` `type` `group` `upid` `duration`
`endtime`.

An unknown field name fails at **startup**, not at 3am on the first failure.

## Filtering

```toml
[filter]
groups = ["vm", "ct", "disk"]      # see --list-groups
min_severity = "error"             # failures only
exclude_users = ["root@pam!*"]     # silence a Terraform/CI token
exclude_types = ["qmconfig"]
guests = ["1440", "102"]           # empty = all
```

Every list is an allowlist when non-empty, "allow all" when empty, and supports a
trailing `*` wildcard.

> **`exclude_users` matches `user!tokenid`.** Proxmox returns those as two
> separate fields (`user: "root@pam"`, `tokenid: "terraform"`) even though the
> UPID reads `root@pam!terraform`. This tool recomposes them, so the identity you
> see in the PVE UI is the one you filter on.

**`backup` and `system` are off by default.** `vzdump` and `aptupdate` are in the
catalogue, but PVE notifies on those natively — turning them on here means two
notifications per event unless you drop the matching native matcher.

## Configuration

Every setting has a TOML key and a `PVE_*` environment variable; **the
environment wins**, so a committed config file can carry the boring settings
while the token stays in the environment. Neither source is mandatory on its own.

See [`config.example.toml`](config.example.toml) for the annotated full set.

| env | default |
|---|---|
| `PVE_URL` `PVE_USER` `PVE_TOKEN_NAME` `PVE_TOKEN_VALUE` | required |
| `PVE_NODE` | unset (whole cluster) |
| `PVE_CA_BUNDLE` / `PVE_VERIFY_TLS` | unset / `true` |
| `PVE_EVENTS_POLL_INTERVAL` | `20` |
| `PVE_EVENTS_SINK` / `PVE_EVENTS_URL` | required |
| `PVE_EVENTS_METHOD` / `PVE_EVENTS_HEADERS` | `POST` / unset |
| `PVE_EVENTS_BODY` / `PVE_EVENTS_BODY_ERROR` | preset |
| `PVE_EVENTS_GROUPS` | `vm,ct,disk` |
| `PVE_EVENTS_INCLUDE_TYPES` / `PVE_EVENTS_EXCLUDE_TYPES` | unset |
| `PVE_EVENTS_MIN_SEVERITY` | `info` |
| `PVE_EVENTS_GUESTS` / `PVE_EVENTS_EXCLUDE_GUESTS` | unset |
| `PVE_EVENTS_NODES` / `PVE_EVENTS_USERS` / `PVE_EVENTS_EXCLUDE_USERS` | unset |
| `PVE_EVENTS_STATE` | `/data/state.json` in the image |
| `PVE_EVENTS_BACKFILL_FROM` | unset (start from now) |

List variables are comma-separated. An **empty** variable counts as unset, so
compose's `PVE_NODE:` and systemd's `Environment=X=` do not blank a configured
value.

## Known limits

These are properties of the data source, not bugs to be fixed later.

- **A guest shut down from inside produces no task.** Powering off from within
  the VM changes its state without PVE running a task, so nothing appears here.
  Pair this with `pve_up` from
  [prometheus-pve-exporter](https://github.com/prometheus-pve/prometheus-pve-exporter)
  if you need that axis too.
- **Burst overflow.** `/cluster/tasks` returns a capped recent window (~25–50
  rows). Something generating more tasks than that inside one poll interval can
  age a row out before it is seen. Lower `poll_interval_secs`, or set `node` to
  use the per-node endpoint, which accepts a larger `limit`.
- **Poll, not push.** Proxmox exposes no event stream. Tailing
  `/var/log/pve/tasks/index` would be push-ish, but requires running *on* the
  hypervisor; this runs anywhere.

## Development

```bash
cargo test          # unit tests + an end-to-end run against a real API response
cargo clippy --all-targets -- -D warnings
cargo fmt
```

`tests/fixtures/cluster-tasks.json` is a real PVE 9.2 reply with hostnames
scrubbed. That matters: it carries the shapes a hand-written fixture gets wrong —
a running task with no `endtime`, an `imgdel` whose id is a bare storage name
with no `@`, `tokenid` split out from `user`, and failure rows whose `status` is
free-form English.

## Licence

MIT
