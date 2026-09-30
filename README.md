# CryoMail

**Incremental, strictly read-only IMAP backup — your mail, cryo-preserved.**

CryoMail copies every message from every mailbox of an IMAP account into a
local [maildir](https://en.wikipedia.org/wiki/Maildir) tree — without ever
changing anything on the server, not even read/unread state — then
optionally snapshots the result into a [restic](https://restic.net)
repository for deduplicated, versioned, optionally off-site history.

Designed to run repeatedly — e.g. hourly from cron or a systemd timer —
downloading only what changed.

## Features

- **Never modifies the server** — enforced at multiple levels, see
  [Read-only guarantees](#read-only-guarantees).
- **Incremental**: each message is downloaded exactly once, keyed by UID.
  Re-runs only fetch new messages; flag changes on known messages are
  applied via cheap renames.
- **Recursive**: `LIST "" "*"` discovers all mailboxes, including nested
  ones. `\NoSelect` hierarchy nodes are skipped automatically.
- **Parallel**: a pool of worker threads, each holding its own IMAP
  connection, downloads message batches (`--jobs` / `--batch-size`). A
  single large INBOX parallelizes across workers.
- **Restic integration**: optional `restic backup` after each sync for
  deduplicated, versioned, optionally off-site snapshots.
- **Backup semantics**: messages deleted on the server are kept locally.
  On `UIDVALIDITY` change the old local copy is preserved as
  `<name>.stale-uv<N>` and the folder is re-fetched.
- **Resumable**: progress survives crashes — `state.json` is a cache, and
  the UID embedded in every filename lets it be rebuilt from disk.

## Read-only guarantees

The promise "does not modify any data, including read/unread" is enforced
in layers, not by convention:

| Level | Mechanism |
|---|---|
| Compile-time | All IMAP access goes through `ReadOnlySession`, which exposes **only** `list`, `examine`, `uid_fetch`, `fetch`, `noop`, `logout`. Mutating commands (`SELECT`, `STORE`, `EXPUNGE`, `APPEND`, `COPY`, `CREATE`, `DELETE`, `RENAME`, `SUBSCRIBE`, `CHECK`, `CLOSE`…) are unreachable — the inner `Session` is never exposed. |
| Runtime | Every FETCH query is parsed and checked against a whitelist of side-effect-free items before it hits the wire. `BODY[]`, `BODY[…]`, `RFC822`, `RFC822.HEADER`, `RFC822.TEXT` — anything that sets `\Seen` — is rejected with an error. |
| Server-enforced | Mailboxes are opened with `EXAMINE`, the read-only form of `SELECT` (RFC 3501 §6.3.2). In this state the server MUST refuse flag updates and MUST NOT alter `\Recent` — even a hypothetical bug above cannot change state. |
| Semantic | Message bodies are fetched exclusively via `BODY.PEEK[]`, which per RFC 3501 never sets `\Seen`. |

The complete set of IMAP commands this program can emit is:
`LOGIN`, `LIST`, `EXAMINE`, `UID FETCH` (validated), `NOOP`, `LOGOUT`.

**Server-side hardening (recommended):** create a dedicated read-only
account or app password where supported — e.g. a Dovecot ACL granting only
`l r` (lookup, read — *without* `s`/write-seen or `w`/write-flags), a Gmail
app password used for nothing else, or a mailcow/SOGo read-only share.
Defense in depth means the client's read-only discipline is verified, not
trusted.

## Install

```sh
git clone https://github.com/ulikoehler/CryoMail.git
cd CryoMail
cargo build --release          # binary: target/release/cryomail
# or install into ~/.cargo/bin:
cargo install --path .
```

Requires a recent stable Rust toolchain. TLS is rustls-based (no OpenSSL
dependency). `restic` is only needed if you use the restic integration.

## Quick start

```sh
cp cryomail.example.ini cryomail.ini   # fill in host / username / output
chmod 600 cryomail.ini                 # keep credentials private
export CRYOMAIL_PASSWORD=...           # or put it in the ini / get prompted
cryomail -c cryomail.ini --dry-run     # preview: what would be fetched?
cryomail -c cryomail.ini               # go
```

A typical run looks like:

```
connected to imap.example.com:993 as you@example.com
found 12 mailboxes
INBOX: 18451 remote, 18451 local, 0 to fetch
Archive/2024: 932 remote, 930 local, 2 to fetch
done: 41120 messages on server, 2 downloaded, 0 failed, 2 were missing, 0 folders skipped
running restic: -r /mnt/backups/mail backup ./mail-backup --tag cryomail
snapshot c665afa6 saved
restic snapshot complete
```

## Usage

```sh
# everything on the command line
cryomail --host imap.example.com -u you@example.com -o ./mail-backup

# ini file; CLI flags override ini values
cryomail -c backup.ini --jobs 8

# see what would happen without writing anything
cryomail -c backup.ini --dry-run

# back up only some mailboxes
cryomail -c backup.ini --folder INBOX --folder "Archive.2024"
```

### Options

| Flag | ini key | Default | Description |
|---|---|---|---|
| `-c, --config FILE` | — | — | ini config file |
| `--host HOST` | `[imap] host` | required | IMAP server hostname |
| `--port PORT` | `[imap] port` | `993` | IMAP server port |
| `-u, --user USER` | `[imap] username` | required | IMAP username |
| `-p, --password PW` | `[imap] password` | see below | IMAP password |
| `--tls MODE` | `[imap] tls` | `auto` | `auto` (TLS on 993, STARTTLS else), `tls`, `starttls`, `none` |
| `--tls-insecure` | — | off | skip certificate verification (last resort) |
| `-o, --output DIR` | `[backup] output` | required | backup destination directory |
| `-j, --jobs N` | `[backup] jobs` | `4` | parallel IMAP connections |
| `--batch-size N` | `[backup] batch_size` | `32` | messages per FETCH command |
| `-f, --folder NAME` | `[backup] folders` (comma-sep) | all | restrict to named mailboxes (repeatable) |
| `--dry-run` | — | off | scan and report, write nothing |
| `--restic-repo R` | `[restic] repo` | `RESTIC_REPOSITORY` | restic repository for post-sync snapshot |
| `--restic-tag T` | `[restic] tag` | `cryomail` | snapshot tag |
| `--restic-arg A` | `[restic] args` | — | extra `restic backup` args (repeatable) |
| `--no-restic` | — | off | skip the restic step this run |

**Password resolution order:** `--password` → `[imap] password` →
`CRYOMAIL_PASSWORD` env var → interactive prompt. Prefer the env var or
prompt — a CLI password is visible in `ps`.

## Output layout

One maildir per IMAP mailbox; the server hierarchy delimiter becomes
directories:

```
mail-backup/
  INBOX/
    cur/  new/  tmp/   # maildir: :2,S = seen, :2,F = flagged, ...
    state.json         # UIDVALIDITY + uid -> file/flags index
  Work/Projects/2024/
    cur/  new/  tmp/  state.json
```

- Filenames embed the UID (`.u42.`): unique, and sufficient to rebuild
  `state.json` from disk alone.
- Flag letters follow maildir convention: `S`=seen `R`=answered `F`=flagged
  `T`=trashed `D`=draft. Flagless messages live in `new/`.
- Mailbox names are used as returned by `LIST` (modified UTF-7 for
  non-ASCII, e.g. `Entw&APw-rfe`); path-unsafe characters are
  percent-encoded.
- Messages removed on the server are **kept** — this is a backup, not a
  mirror. If a mailbox's `UIDVALIDITY` changes (server re-created the
  folder, UIDs reset), the old copy is renamed to `<name>.stale-uv<N>`.

## Restic

Set a repository and each successful sync is followed by
`restic -r <repo> backup <output> --tag cryomail`:

```ini
[restic]
repo = sftp:backuphost:/srv/restic/mail   ; or s3:, rest:, /local/path, ...
tag = cryomail
args = --exclude "*.tmp"                   ; extra `restic backup` args
```

The restic process inherits the environment, so `RESTIC_PASSWORD`,
`RESTIC_PASSWORD_FILE`, `RESTIC_PASSWORD_COMMAND`, `AWS_*`, `B2_*` and all
backend options work unchanged. The step is skipped on `--dry-run`,
`--no-restic`, and when the sync reported errors.

This gives you two complementary artifacts: a plain local maildir you can
read directly with any mail client (`mutt`, `notmuch`, Thunderbird import),
and a deduplicated, versioned, restorable restic history.

## Restoring

- **Plain files**: maildir files are RFC822 messages — open `<out>/<folder>`
  in any maildir-capable client, or copy `new/`+`cur/` files wherever needed.
- **Restic**: `restic -r <repo> restore latest --target /tmp/restore` or
  `restic -r <repo> mount /mnt` to browse snapshots.
- **Back to a server**: use any IMAP upload tool (`offlineimap`-style sync,
  `imapsync`, or `doveadm import`) pointing at the maildir.

## Examples

See `examples/`:

- `gmail.ini` — Gmail via app password, notes on `[Gmail]/All Mail`
- `selfhosted.ini` — Dovecot/generic + restic over SFTP
- `restic-s3.ini` — restic on S3-compatible storage
- `cryomail.service` / `cryomail.timer` — systemd user timer

Cron equivalent:

```cron
0 * * * *  CRYOMAIL_PASSWORD=... /usr/local/bin/cryomail -c /etc/cryomail.ini >> /var/log/cryomail.log 2>&1
```

## Notes & limitations

- Tested against GreenMail (CI-style e2e during development); always run
  `--dry-run` once against a new server.
- Mailbox names in modified UTF-7 are *not* decoded for display — folder
  dirs may look like `Entw&APw-rfe`. Cosmetic only.
- `state.json` is per-folder; deleting it is safe (rebuilt from filenames),
  deleting the folder's files is not (re-download).
- If the same account is being actively modified by other clients while
  backing up, results are still consistent per-folder (EXAMINE snapshot),
  but flag renames reflect the most recent server state.

## Development

```sh
cargo test     # unit tests (query whitelist, maildir naming, path sanitize)
cargo clippy   # lint-clean
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
