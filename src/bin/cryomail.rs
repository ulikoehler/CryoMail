//! cryomail — incremental, read-only IMAP backup to a local maildir tree.
//!
//! Copies every message from every mailbox (recursively) into a local
//! maildir-style tree, keyed by UID. Uses `EXAMINE` (read-only SELECT) and
//! `BODY.PEEK[]` so the server state — including read/unread flags — is never
//! modified. Re-runs only download messages whose UID is not on disk yet.

use anyhow::{Context, Result, bail};
use clap::Parser;
use cryomail::*;
use imap::types::Flag;
use imap_proto::NameAttribute;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

// ---------------------------------------------------------------------------
// Configuration: ini file provides defaults, command line overrides them.
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "cryomail",
    version,
    about = "Incremental recursive IMAP backup (never modifies the server)",
    long_about = "Backs up an entire IMAP account into a local maildir tree.\n\
                  Read-only on the server: uses EXAMINE and BODY.PEEK[].\n\
                  Incremental: a message is downloaded exactly once, keyed by UID.\n\n\
                  All settings can come from an ini file (-c) and/or CLI flags;\n\
                  CLI flags always win over the ini file.\n\
                  Restore with the companion `cryomail-restore` binary."
)]
struct Args {
    /// Path to an ini config file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// IMAP server hostname [imap] host
    #[arg(long)]
    host: Option<String>,

    /// IMAP server port [imap] port (default: 993)
    #[arg(long)]
    port: Option<u16>,

    /// IMAP username [imap] username
    #[arg(short, long)]
    user: Option<String>,

    /// IMAP password [imap] password — prefer the CRYOMAIL_PASSWORD
    /// env var or the ini file; a CLI password is visible in `ps`
    #[arg(short, long)]
    password: Option<String>,

    /// Output directory for the backup [backup] output
    #[arg(short, long, value_name = "DIR")]
    output: Option<PathBuf>,

    /// Number of parallel IMAP connections [backup] jobs (default: 4)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Messages fetched per IMAP command [backup] batch_size (default: 32)
    #[arg(long)]
    batch_size: Option<usize>,

    /// Only back up these mailboxes (exact LIST names, repeatable)
    #[arg(short, long = "folder")]
    folders: Vec<String>,

    /// TLS mode: auto (TLS on 993, else STARTTLS), tls, starttls, none
    /// [imap] tls (default: auto)
    #[arg(long)]
    tls: Option<String>,

    /// Scan and report what would be downloaded, but write nothing
    #[arg(long)]
    dry_run: bool,

    /// Skip TLS certificate verification (DANGEROUS, last resort only)
    #[arg(long)]
    tls_insecure: bool,

    /// Restic repository; after a successful sync run
    /// `restic -r <repo> backup <output>` [restic] repo
    /// (falls back to RESTIC_REPOSITORY)
    #[arg(long, value_name = "REPO")]
    restic_repo: Option<String>,

    /// Tag for the restic snapshot [restic] tag (default: cryomail)
    #[arg(long)]
    restic_tag: Option<String>,

    /// Extra argument for `restic backup` (repeatable);
    /// ini alternative: [restic] args = "..."
    #[arg(long, value_name = "ARG")]
    restic_arg: Vec<String>,

    /// Disable the restic step even if a repo is configured
    #[arg(long)]
    no_restic: bool,
}

struct Config {
    conn: ConnectParams,
    output: PathBuf,
    jobs: usize,
    batch_size: usize,
    folders: Vec<String>,
    dry_run: bool,
    restic_repo: Option<String>,
    restic_tag: String,
    restic_args: Vec<String>,
    no_restic: bool,
}

fn resolve(args: Args) -> Result<Config> {
    let ini = load_ini(args.config.as_deref())?;

    let host = args
        .host
        .or_else(|| ini_str(&ini, "imap", "host"))
        .context("IMAP host required (--host or [imap] host)")?;
    let port = args.port.or(ini_num(&ini, "imap", "port")?).unwrap_or(993);
    let user = args
        .user
        .or_else(|| ini_str(&ini, "imap", "username"))
        .or_else(|| ini_str(&ini, "imap", "user"))
        .context("IMAP username required (--user or [imap] username)")?;
    let password = resolve_password(
        args.password,
        ini_str(&ini, "imap", "password"),
        &user,
        &host,
    )?;
    let output = args
        .output
        .or_else(|| ini_str(&ini, "backup", "output").map(PathBuf::from))
        .context("output directory required (--output or [backup] output)")?;

    let mut folders = args.folders;
    if folders.is_empty()
        && let Some(list) = ini_str(&ini, "backup", "folders")
    {
        folders = list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    let tls = args
        .tls
        .or_else(|| ini_str(&ini, "imap", "tls"))
        .unwrap_or_else(|| "auto".into());
    let mode = parse_tls_mode(&tls)?;

    let restic_repo = args
        .restic_repo
        .or_else(|| ini_str(&ini, "restic", "repo"))
        .or_else(|| env::var("RESTIC_REPOSITORY").ok())
        .filter(|s| !s.is_empty());
    let mut restic_args: Vec<String> = Vec::new();
    if let Some(s) = ini_str(&ini, "restic", "args") {
        restic_args
            .extend(shlex::split(&s).with_context(|| format!("invalid [restic] args: {s:?}"))?);
    }
    restic_args.extend(args.restic_arg);

    Ok(Config {
        conn: ConnectParams {
            host,
            port,
            user,
            password,
            mode,
            tls_insecure: args.tls_insecure,
        },
        output,
        jobs: args
            .jobs
            .or(ini_num(&ini, "backup", "jobs")?)
            .unwrap_or(4)
            .max(1),
        batch_size: args
            .batch_size
            .or(ini_num(&ini, "backup", "batch_size")?)
            .unwrap_or(32)
            .max(1),
        folders,
        dry_run: args.dry_run,
        restic_repo,
        restic_tag: args
            .restic_tag
            .or_else(|| ini_str(&ini, "restic", "tag"))
            .unwrap_or_else(|| "cryomail".into()),
        restic_args,
        no_restic: args.no_restic,
    })
}

fn connect_ro(cfg: &Config) -> Result<ReadOnlySession> {
    connect(&cfg.conn).map(ReadOnlySession::new)
}

// ---------------------------------------------------------------------------
// Work items
// ---------------------------------------------------------------------------

struct Task {
    folder_idx: usize,
    mailbox: String,
    dir: PathBuf,
    uids: Vec<u32>,
}

struct FolderResult {
    folder_idx: usize,
    /// (uid, rel path, flags, internal_date as RFC3339)
    fetched: Vec<(u32, String, Vec<String>, Option<String>)>,
    failed: Vec<u32>,
}

fn worker(cfg: Arc<Config>, rx: Arc<Mutex<Receiver<Task>>>, tx: Sender<FolderResult>) {
    let mut session: Option<ReadOnlySession> = None;
    let mut examined = String::new();

    loop {
        let task = rx.lock().unwrap().recv();
        let Ok(task) = task else { break };

        let mut result = FolderResult {
            folder_idx: task.folder_idx,
            fetched: vec![],
            failed: vec![],
        };

        'attempt: for attempt in 0..2 {
            if session.is_none() {
                match connect_ro(&cfg) {
                    Ok(s) => {
                        session = Some(s);
                        examined.clear();
                    }
                    Err(e) if attempt == 0 => {
                        eprintln!("worker connect failed, retrying: {e:#}");
                        continue;
                    }
                    Err(e) => {
                        eprintln!("worker connect failed: {e:#}");
                        result.failed.extend(&task.uids);
                        break 'attempt;
                    }
                }
            }
            let sess = session.as_mut().unwrap();

            if examined != task.mailbox {
                match sess.examine(&task.mailbox) {
                    Ok(_) => examined = task.mailbox.clone(),
                    Err(e) => {
                        eprintln!("EXAMINE {:?} failed: {e:#}", task.mailbox);
                        session = None;
                        if attempt == 0 {
                            continue;
                        }
                        result.failed.extend(&task.uids);
                        break 'attempt;
                    }
                }
            }

            let set = task
                .uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            match sess.uid_fetch(&set, "(FLAGS INTERNALDATE BODY.PEEK[])") {
                Ok(fetches) => {
                    let mut seen = Vec::new();
                    for f in fetches.iter() {
                        let uid = f.uid.unwrap_or(0);
                        seen.push(uid);
                        match f.body() {
                            Some(body) => match write_message(&task.dir, uid, f.flags(), body) {
                                Ok(rel) => {
                                    result.fetched.push((
                                        uid,
                                        rel,
                                        flag_strings(f.flags()),
                                        f.internal_date().map(|d| d.to_rfc3339()),
                                    ));
                                }
                                Err(e) => {
                                    eprintln!("write failed for uid {uid}: {e:#}");
                                    result.failed.push(uid);
                                }
                            },
                            None => result.failed.push(uid),
                        }
                    }
                    for &u in &task.uids {
                        if !seen.contains(&u) {
                            result.failed.push(u);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("UID FETCH {set} in {:?} failed: {e:#}", task.mailbox);
                    session = None; // reconnect next attempt
                    if attempt == 0 {
                        continue;
                    }
                    result.failed.extend(&task.uids);
                }
            }
            break 'attempt;
        }

        let _ = tx.send(result);
    }

    if let Some(mut s) = session {
        let _ = s.logout();
    }
}

// ---------------------------------------------------------------------------
// Restic integration: snapshot the local maildir tree into a repository.
// Inherits the environment, so RESTIC_PASSWORD / RESTIC_PASSWORD_FILE /
// RESTIC_PASSWORD_COMMAND and friends work as usual.
// ---------------------------------------------------------------------------

fn run_restic(cfg: &Config) -> Result<()> {
    let repo = cfg.restic_repo.as_deref().unwrap();
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("--repo")
        .arg(repo)
        .arg("backup")
        .arg(&cfg.output)
        .arg("--tag")
        .arg(&cfg.restic_tag)
        .args(&cfg.restic_args);
    println!(
        "running restic: -r {repo} backup {} --tag {}",
        cfg.output.display(),
        cfg.restic_tag
    );
    let status = cmd
        .status()
        .context("failed to run restic — is it installed and on PATH?")?;
    if !status.success() {
        bail!("restic exited with {status}");
    }
    println!("restic snapshot complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Arc::new(resolve(args)?);

    let mut session = connect_ro(&cfg)?;
    println!(
        "connected to {}:{} as {}",
        cfg.conn.host, cfg.conn.port, cfg.conn.user
    );

    // Recursive folder discovery: LIST "" "*" returns every mailbox.
    // \NoSelect entries are pure hierarchy nodes — not openable — skip them.
    let names = session.list(Some(""), Some("*"))?;
    let mut mailboxes: Vec<(String, Option<String>)> = names
        .iter()
        .filter(|n| {
            !n.attributes()
                .iter()
                .any(|a| matches!(a, NameAttribute::NoSelect))
        })
        .map(|n| (n.name().to_string(), n.delimiter().map(str::to_string)))
        .collect();
    mailboxes.sort();
    mailboxes.dedup_by(|a, b| a.0 == b.0);
    if !cfg.folders.is_empty() {
        mailboxes.retain(|(n, _)| cfg.folders.contains(n));
    }
    println!("found {} mailboxes", mailboxes.len());

    let (task_tx, task_rx) = channel::<Task>();
    let (res_tx, res_rx) = channel::<FolderResult>();
    let task_rx = Arc::new(Mutex::new(task_rx));

    let mut workers = Vec::new();
    if !cfg.dry_run {
        for _ in 0..cfg.jobs {
            let (cfg, rx, tx) = (cfg.clone(), task_rx.clone(), res_tx.clone());
            workers.push(thread::spawn(move || worker(cfg, rx, tx)));
        }
    }
    drop(res_tx);

    let mut states: Vec<(PathBuf, FolderState)> = Vec::new();
    let mut total_missing = 0usize;
    let mut total_remote = 0usize;
    let mut scan_errors = 0usize;

    for (idx, (mailbox, delim)) in mailboxes.iter().enumerate() {
        let dir = folder_dir(&cfg.output, mailbox, delim.as_deref());
        let mut state = load_state(&dir);

        let mbox = match session.examine(mailbox) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping {mailbox:?}: EXAMINE failed: {e:#}");
                scan_errors += 1;
                states.push((dir, state));
                continue;
            }
        };
        let uid_validity = mbox.uid_validity.unwrap_or(0);
        total_remote += mbox.exists as usize;

        // UIDVALIDITY changed -> UIDs are meaningless; set old backup aside.
        if state.uid_validity.is_some_and(|uv| uv != uid_validity) && !state.messages.is_empty() {
            let stale = dir.with_file_name(format!(
                "{}.stale-uv{}",
                dir.file_name().unwrap_or_default().to_string_lossy(),
                state.uid_validity.unwrap_or(0)
            ));
            eprintln!(
                "{mailbox:?}: UIDVALIDITY changed {} -> {uid_validity}; \
                 moving old copy to {}",
                state.uid_validity.unwrap_or(0),
                stale.display()
            );
            fs::rename(&dir, &stale)?;
            state = FolderState::default();
        }
        state.uid_validity = Some(uid_validity);

        // Remote UID -> flags map (metadata only, cheap).
        let mut remote: BTreeMap<u32, Vec<Flag>> = BTreeMap::new();
        if mbox.exists > 0 {
            match session.uid_fetch("1:*", "(FLAGS)") {
                Ok(fetches) => {
                    for f in fetches.iter() {
                        if let Some(u) = f.uid {
                            remote.insert(
                                u,
                                f.flags()
                                    .iter()
                                    .map(|fl| Flag::from(fl.to_string()))
                                    .collect(),
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("skipping {mailbox:?}: UID FETCH FLAGS failed: {e:#}");
                    scan_errors += 1;
                    states.push((dir, state));
                    continue;
                }
            }
        }

        // Apply flag changes to already-downloaded messages (rename only).
        let mut missing: Vec<u32> = Vec::new();
        let mut flag_updates = 0usize;
        for (uid, flags) in &remote {
            match state.messages.get_mut(uid) {
                None => missing.push(*uid),
                Some(m) => {
                    let cur = flag_strings(flags);
                    if m.flags == cur {
                        continue;
                    }
                    flag_updates += 1;
                    if cfg.dry_run {
                        continue;
                    }
                    let name = Path::new(&m.file)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let Some((base, _)) = parse_basename(&name) else {
                        continue;
                    };
                    let new_rel = rel_path(base, flags);
                    if new_rel == m.file
                        || fs::rename(dir.join(&m.file), dir.join(&new_rel)).is_ok()
                    {
                        m.file = new_rel;
                        m.flags = cur;
                    }
                }
            }
        }

        total_missing += missing.len();
        println!(
            "{mailbox}: {} remote, {} local, {} to fetch{}",
            remote.len(),
            state.messages.len(),
            missing.len(),
            if flag_updates > 0 {
                format!(", {flag_updates} flag updates")
            } else {
                String::new()
            }
        );

        if !cfg.dry_run && !missing.is_empty() {
            create_maildir(&dir)?;
            for chunk in missing.chunks(cfg.batch_size) {
                let _ = task_tx.send(Task {
                    folder_idx: idx,
                    mailbox: mailbox.clone(),
                    dir: dir.clone(),
                    uids: chunk.to_vec(),
                });
            }
        }
        states.push((dir, state));
    }
    drop(task_tx);

    // Collect worker results.
    let mut fetched = 0usize;
    let mut failed = 0usize;
    for res in res_rx {
        let state = &mut states[res.folder_idx].1;
        for (uid, rel, flags, internal_date) in res.fetched {
            state.messages.insert(
                uid,
                MsgEntry {
                    file: rel,
                    flags,
                    internal_date,
                },
            );
            fetched += 1;
        }
        failed += res.failed.len();
    }
    for w in workers {
        let _ = w.join();
    }

    // Persist per-folder state.
    if !cfg.dry_run {
        for (dir, state) in &states {
            if !state.messages.is_empty() || dir.exists() {
                create_maildir(dir)?;
                save_state(dir, state)?;
            }
        }
    }

    println!(
        "done: {total_remote} messages on server, {fetched} downloaded, \
         {failed} failed, {total_missing} were missing, {scan_errors} folders skipped{}",
        if cfg.dry_run { " (dry run)" } else { "" }
    );

    session.logout()?;

    if cfg.restic_repo.is_some() && !cfg.no_restic && !cfg.dry_run {
        if failed > 0 || scan_errors > 0 {
            eprintln!("sync had errors; skipping restic snapshot");
        } else {
            run_restic(&cfg)?;
        }
    }

    if failed > 0 || scan_errors > 0 {
        bail!("backup completed with errors");
    }
    Ok(())
}
