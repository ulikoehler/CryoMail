//! cryomail-restore — structured restore of a CryoMail backup to an IMAP server.
//!
//! Walks the local maildir tree produced by `cryomail`, recreates the mailbox
//! hierarchy on the target server (CREATE), and uploads every message with
//! APPEND, preserving flags and INTERNALDATE. Incremental: per-folder
//! `restore-state.json` records which local files are already on the server,
//! so re-runs only append what is missing.
//!
//! Unlike `cryomail` this binary writes to the server BY DESIGN — APPEND and
//! CREATE are its whole job. It never touches existing messages: no STORE,
//! no EXPUNGE, only mailbox creation and message upload.

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use clap::Parser;
use cryomail::*;
use imap::types::Flag;
use imap::{Connection, Session};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Parser, Debug)]
#[command(
    name = "cryomail-restore",
    version,
    about = "Restore a CryoMail backup to an IMAP server (writes to the server!)",
    long_about = "Restores a maildir tree produced by `cryomail` into an IMAP account.\n\
                  Creates mailboxes and APPENDs messages preserving flags and\n\
                  INTERNALDATE. Incremental: messages already uploaded are skipped.\n\n\
                  This tool writes to the target server by design.\n\
                  All settings can come from an ini file (-c) and/or CLI flags;\n\
                  CLI flags always win over the ini file."
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

    /// Backup directory to restore from [restore] input
    #[arg(short, long, value_name = "DIR")]
    input: Option<PathBuf>,

    /// Prepend this mailbox prefix on the target, e.g. "Restored"
    /// [restore] prefix
    #[arg(long)]
    prefix: Option<String>,

    /// Restore only these mailboxes (repeatable) [restore] folders (comma-sep)
    #[arg(short, long, value_name = "NAME")]
    folder: Vec<String>,

    /// Number of parallel IMAP connections [restore] jobs (default: 4)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Messages appended per task chunk [restore] batch_size (default: 64)
    #[arg(long)]
    batch_size: Option<usize>,

    /// TLS mode: auto (TLS on 993, else STARTTLS), tls, starttls, none
    /// [imap] tls (default: auto)
    #[arg(long)]
    tls: Option<String>,

    /// Skip TLS certificate verification (DANGEROUS, last resort only)
    #[arg(long)]
    tls_insecure: bool,

    /// Plan only: list mailboxes/messages, upload nothing
    #[arg(long)]
    dry_run: bool,
}

struct Config {
    conn: ConnectParams,
    input: PathBuf,
    prefix: String,
    folders: Option<HashSet<String>>,
    jobs: usize,
    batch_size: usize,
    dry_run: bool,
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
    let input = args
        .input
        .or_else(|| ini_str(&ini, "restore", "input").map(PathBuf::from))
        .context("input directory required (--input or [restore] input)")?;
    if !input.is_dir() {
        bail!("input directory {} does not exist", input.display());
    }
    let tls = args
        .tls
        .or_else(|| ini_str(&ini, "imap", "tls"))
        .unwrap_or_else(|| "auto".into());

    Ok(Config {
        conn: ConnectParams {
            host,
            port,
            user,
            password,
            mode: parse_tls_mode(&tls)?,
            tls_insecure: args.tls_insecure,
        },
        input,
        prefix: args
            .prefix
            .or_else(|| ini_str(&ini, "restore", "prefix"))
            .unwrap_or_default(),
        folders: {
            let mut f: Vec<String> = args.folder;
            if let Some(s) = ini_str(&ini, "restore", "folders") {
                f.extend(s.split(',').map(|x| x.trim().to_string()));
            }
            (!f.is_empty()).then(|| f.into_iter().collect())
        },
        jobs: args
            .jobs
            .or(ini_num(&ini, "restore", "jobs")?)
            .unwrap_or(4)
            .max(1),
        batch_size: args
            .batch_size
            .or(ini_num(&ini, "restore", "batch_size")?)
            .unwrap_or(64)
            .max(1),
        dry_run: args.dry_run,
    })
}

// ---------------------------------------------------------------------------
// Per-folder restore progress (incremental resume)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct RestoreState {
    /// UIDVALIDITY of the target mailbox at last upload; if it changes the
    /// server-side mailbox was recreated and we start over.
    server_uid_validity: Option<u32>,
    /// Maildir-relative files already uploaded, e.g. "cur/x.u42.host:2,S"
    #[serde(default)]
    done: BTreeSet<String>,
}

fn load_restore_state(dir: &Path) -> RestoreState {
    fs::read_to_string(dir.join("restore-state.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_restore_state(dir: &Path, state: &RestoreState) -> Result<()> {
    let tmp = dir.join("restore-state.json.tmp");
    fs::write(&tmp, serde_json::to_string(state)?)?;
    fs::rename(&tmp, dir.join("restore-state.json"))?;
    Ok(())
}

/// One local message file queued for upload.
#[derive(Clone)]
struct MsgFile {
    /// path relative to the folder's maildir, e.g. "cur/x.u42.host:2,S"
    rel: String,
    /// ordering key: embedded backup UID (or u32::MAX if none)
    uid: u32,
    flags: Vec<String>,
    internal_date: Option<String>,
}

/// Collect message files from a maildir dir, sorted by backup UID.
fn collect_messages(dir: &Path, state: &FolderState) -> Vec<MsgFile> {
    let mut files = Vec::new();
    for sub in ["cur", "new"] {
        if let Ok(rd) = fs::read_dir(dir.join(sub)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some((base, letters)) = parse_basename(&name) else {
                    continue;
                };
                let uid = base_uid(base).unwrap_or(u32::MAX);
                let rel = format!("{sub}/{name}");
                // Prefer full flag atoms + date from state.json when present.
                let entry = state.messages.get(&uid).filter(|m| m.file == rel);
                let mut flags = entry.map(|m| m.flags.clone()).unwrap_or_else(|| {
                    letters
                        .iter()
                        .filter_map(|c| letter_flag(*c).map(String::from))
                        .collect()
                });
                flags.retain(|f| f != "\\Recent");
                files.push(MsgFile {
                    rel,
                    uid,
                    flags,
                    internal_date: entry.and_then(|m| m.internal_date.clone()),
                });
            }
        }
    }
    files.sort_by_key(|f| f.uid);
    files
}

fn parse_flags(flags: &[String]) -> Vec<Flag<'static>> {
    flags
        .iter()
        .map(|s| Flag::from(s.clone()))
        .filter(|f| !matches!(f, Flag::Recent))
        .collect()
}

// ---------------------------------------------------------------------------
// Worker: one IMAP connection each, APPENDs chunks of files.
// ---------------------------------------------------------------------------

struct Task {
    folder_idx: usize,
    mailbox: String,
    dir: PathBuf,
    files: Vec<MsgFile>,
}

struct TaskResult {
    folder_idx: usize,
    done: Vec<String>,
    failed: Vec<String>,
}

fn append_one(
    sess: &mut Session<Connection>,
    mailbox: &str,
    file: &MsgFile,
    content: &[u8],
) -> Result<()> {
    let mut cmd = sess.append(mailbox, content);
    cmd.flags(parse_flags(&file.flags));
    if let Some(Ok(dt)) = file
        .internal_date
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
    {
        cmd.internal_date(dt);
    }
    cmd.finish().map(|_| ()).map_err(Into::into)
}

fn worker(cfg: Arc<Config>, rx: Arc<Mutex<Receiver<Task>>>, tx: Sender<TaskResult>) {
    let mut session: Option<Session<Connection>> = None;

    loop {
        let task = rx.lock().unwrap().recv();
        let Ok(task) = task else { break };

        let mut result = TaskResult {
            folder_idx: task.folder_idx,
            done: vec![],
            failed: vec![],
        };

        for attempt in 0..2 {
            if session.is_none() {
                match connect(&cfg.conn) {
                    Ok(s) => session = Some(s),
                    Err(e) => {
                        if attempt == 0 {
                            eprintln!("worker connect failed, retrying: {e:#}");
                            continue;
                        }
                        eprintln!("worker connect failed: {e:#}");
                        result
                            .failed
                            .extend(task.files.iter().map(|f| f.rel.clone()));
                        break;
                    }
                }
            }
            let sess = session.as_mut().unwrap();
            let mut fatal = false;
            for file in &task.files {
                let path = task.dir.join(&file.rel);
                match fs::read(&path) {
                    Ok(content) => match append_one(sess, &task.mailbox, file, &content) {
                        Ok(()) => result.done.push(file.rel.clone()),
                        Err(e) => {
                            eprintln!("APPEND {} -> {:?} failed: {e:#}", file.rel, task.mailbox);
                            result.failed.push(file.rel.clone());
                            session = None; // connection may be broken
                            fatal = true;
                            break;
                        }
                    },
                    Err(e) => {
                        eprintln!("read {} failed: {e:#}", path.display());
                        result.failed.push(file.rel.clone());
                    }
                }
            }
            if fatal && attempt == 0 {
                continue; // reconnect and retry the whole task once
            }
            break;
        }

        let _ = tx.send(result);
    }

    if let Some(mut s) = session {
        let _ = s.logout();
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Arc::new(resolve(args)?);

    let mut session = connect(&cfg.conn)?;
    println!(
        "connected to {}:{} as {}",
        cfg.conn.host, cfg.conn.port, cfg.conn.user
    );
    println!("NOTE: this tool CREATES mailboxes and APPENDs messages on the target.");

    // Target hierarchy delimiter + existing mailbox names.
    let names = session.list(Some(""), Some("*"))?;
    let mut delimiter = "/".to_string();
    let mut existing: HashSet<String> = HashSet::new();
    for n in names.iter() {
        if let Some(d) = n.delimiter() {
            delimiter = d.to_string();
        }
        existing.insert(n.name().to_string());
    }

    // Discover local maildirs and map them to target mailbox names.
    let dirs = find_maildirs(&cfg.input);
    if dirs.is_empty() {
        bail!("no maildir folders found under {}", cfg.input.display());
    }
    let mut folders: Vec<(PathBuf, String)> = dirs
        .iter()
        .map(|d| {
            let rel = d.strip_prefix(&cfg.input).unwrap_or(d.as_path());
            let name = dir_to_mailbox(rel, &delimiter);
            (d.clone(), name)
        })
        .filter(|(_, name)| cfg.folders.as_ref().is_none_or(|f| f.contains(name)))
        .map(|(d, mut name)| {
            if !cfg.prefix.is_empty() {
                name = format!("{}{}{}", cfg.prefix, delimiter, name);
            }
            (d, name)
        })
        .collect();
    folders.sort_by(|a, b| a.1.cmp(&b.1));
    println!(
        "restoring {} mailboxes from {} (delimiter {:?})",
        folders.len(),
        cfg.input.display(),
        delimiter
    );

    // Create mailboxes (with ancestor chain) and plan uploads.
    let (task_tx, task_rx) = channel::<Task>();
    let (res_tx, res_rx) = channel::<TaskResult>();
    let task_rx = Arc::new(Mutex::new(task_rx));

    let mut workers = Vec::new();
    if !cfg.dry_run {
        for _ in 0..cfg.jobs {
            let (cfg, rx, tx) = (cfg.clone(), task_rx.clone(), res_tx.clone());
            workers.push(thread::spawn(move || worker(cfg, rx, tx)));
        }
    }
    drop(res_tx);

    let mut states: Vec<(PathBuf, RestoreState)> = Vec::new();
    let mut total_to_upload = 0usize;
    let mut total_local = 0usize;

    for (idx, (dir, mailbox)) in folders.iter().enumerate() {
        // Create ancestor chain: "A/B/C" needs "A" then "A/B" then "A/B/C".
        let parts: Vec<&str> = mailbox.split(delimiter.as_str()).collect();
        if !cfg.dry_run {
            for i in 1..=parts.len() {
                let name = parts[..i].join(&delimiter);
                if existing.contains(&name) || name.eq_ignore_ascii_case("INBOX") && i == 1 {
                    continue;
                }
                match session.create(&name) {
                    Ok(()) => {
                        println!("created mailbox {name:?}");
                        existing.insert(name);
                    }
                    // Already-exists races and \Noselect parents are fine.
                    Err(e) => eprintln!("CREATE {name:?}: {e:#} (continuing)"),
                }
            }
        }

        let mut rstate = load_restore_state(dir);
        if !cfg.dry_run {
            // Track target UIDVALIDITY; if it changed, uploads so far are gone.
            match session.examine(mailbox) {
                Ok(m) => {
                    let uv = m.uid_validity.unwrap_or(0);
                    if rstate.server_uid_validity.is_some_and(|o| o != uv) {
                        eprintln!(
                            "{mailbox:?}: target UIDVALIDITY changed; re-uploading all messages"
                        );
                        rstate.done.clear();
                    }
                    rstate.server_uid_validity = Some(uv);
                }
                Err(e) => {
                    eprintln!("EXAMINE {mailbox:?} failed: {e:#}");
                    states.push((dir.clone(), rstate));
                    continue;
                }
            }
        }

        let fstate = load_state(dir);
        let files = collect_messages(dir, &fstate);
        total_local += files.len();
        let already = files
            .iter()
            .filter(|f| rstate.done.contains(&f.rel))
            .count();
        let todo: Vec<MsgFile> = files
            .into_iter()
            .filter(|f| !rstate.done.contains(&f.rel))
            .collect();
        total_to_upload += todo.len();
        println!(
            "{mailbox}: {} local, {} already restored, {} to upload",
            already + todo.len(),
            already,
            todo.len()
        );

        if !cfg.dry_run {
            for chunk in todo.chunks(cfg.batch_size) {
                let _ = task_tx.send(Task {
                    folder_idx: idx,
                    mailbox: mailbox.clone(),
                    dir: dir.clone(),
                    files: chunk.to_vec(),
                });
            }
        }
        states.push((dir.clone(), rstate));
    }
    drop(task_tx);

    let mut uploaded = 0usize;
    let mut failed = 0usize;
    for res in res_rx {
        let (_, rstate) = &mut states[res.folder_idx];
        for rel in res.done {
            rstate.done.insert(rel);
            uploaded += 1;
        }
        failed += res.failed.len();
    }
    for w in workers {
        let _ = w.join();
    }

    if !cfg.dry_run {
        for (dir, rstate) in &states {
            save_restore_state(dir, rstate)?;
        }
    }

    println!(
        "done: {total_local} local messages, {uploaded} uploaded, \
         {failed} failed, {total_to_upload} were pending{}",
        if cfg.dry_run { " (dry run)" } else { "" }
    );

    session.logout()?;
    if failed > 0 {
        bail!("restore completed with errors");
    }
    Ok(())
}
