//! Incremental, read-only IMAP backup.
//!
//! Copies every message from every mailbox (recursively) into a local
//! maildir-style tree, keyed by UID. Uses `EXAMINE` (read-only SELECT) and
//! `BODY.PEEK[]` so the server state — including read/unread flags — is never
//! modified. Re-runs only download messages whose UID is not on disk yet.

use anyhow::{bail, Context, Result};
use clap::Parser;
use configparser::ini::Ini;
use imap::types::Flag;
use imap::{ClientBuilder, Connection, ConnectionMode, Session, TlsKind};
use imap_proto::NameAttribute;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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
    host: String,
    port: u16,
    user: String,
    password: String,
    output: PathBuf,
    jobs: usize,
    batch_size: usize,
    folders: Vec<String>,
    dry_run: bool,
    tls_insecure: bool,
    mode: ConnectionMode,
    restic_repo: Option<String>,
    restic_tag: String,
    restic_args: Vec<String>,
    no_restic: bool,
}

fn ini_str(ini: &Ini, section: &str, key: &str) -> Option<String> {
    ini.get(section, key).filter(|s| !s.trim().is_empty())
}

fn ini_num<T: std::str::FromStr>(ini: &Ini, section: &str, key: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match ini_str(ini, section, key) {
        Some(s) => s
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("invalid value for [{section}] {key}: {s:?}: {e}")),
        None => Ok(None),
    }
}

fn resolve(args: Args) -> Result<Config> {
    let mut ini = Ini::new();
    if let Some(path) = &args.config {
        ini.load(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    }

    let host = args
        .host
        .or_else(|| ini_str(&ini, "imap", "host"))
        .context("IMAP host required (--host or [imap] host)")?;
    let port = args
        .port
        .or(ini_num(&ini, "imap", "port")?)
        .unwrap_or(993);
    let user = args
        .user
        .or_else(|| ini_str(&ini, "imap", "username"))
        .or_else(|| ini_str(&ini, "imap", "user"))
        .context("IMAP username required (--user or [imap] username)")?;
    let password = args
        .password
        .or_else(|| ini_str(&ini, "imap", "password"))
        .or_else(|| env::var("CRYOMAIL_PASSWORD").ok())
        .or_else(|| env::var("EMAIL_BACKUP_PASSWORD").ok())
        .filter(|s| !s.is_empty())
        .map(Ok)
        .unwrap_or_else(|| {
            rpassword::prompt_password(format!("IMAP password for {user}@{host}: "))
        })
        .context("reading IMAP password")?;
    let output = args
        .output
        .or_else(|| ini_str(&ini, "backup", "output").map(PathBuf::from))
        .context("output directory required (--output or [backup] output)")?;

    let mut folders = args.folders;
    if folders.is_empty() && let Some(list) = ini_str(&ini, "backup", "folders") {
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
    let mode = match tls.to_ascii_lowercase().as_str() {
        "auto" => ConnectionMode::AutoTls,
        "tls" | "ssl" => ConnectionMode::Tls,
        "starttls" => ConnectionMode::StartTls,
        "none" | "off" | "plaintext" => ConnectionMode::Plaintext,
        other => bail!("invalid tls mode {other:?} (auto|tls|starttls|none)"),
    };

    let restic_repo = args
        .restic_repo
        .or_else(|| ini_str(&ini, "restic", "repo"))
        .or_else(|| env::var("RESTIC_REPOSITORY").ok())
        .filter(|s| !s.is_empty());
    let mut restic_args: Vec<String> = Vec::new();
    if let Some(s) = ini_str(&ini, "restic", "args") {
        restic_args.extend(
            shlex::split(&s)
                .with_context(|| format!("invalid [restic] args: {s:?}"))?,
        );
    }
    restic_args.extend(args.restic_arg);

    Ok(Config {
        host,
        port,
        user,
        password,
        output,
        jobs: args.jobs.or(ini_num(&ini, "backup", "jobs")?).unwrap_or(4).max(1),
        batch_size: args
            .batch_size
            .or(ini_num(&ini, "backup", "batch_size")?)
            .unwrap_or(32)
            .max(1),
        folders,
        dry_run: args.dry_run,
        tls_insecure: args.tls_insecure,
        mode,
        restic_repo,
        restic_tag: args
            .restic_tag
            .or_else(|| ini_str(&ini, "restic", "tag"))
            .unwrap_or_else(|| "cryomail".into()),
        restic_args,
        no_restic: args.no_restic,
    })
}

// ---------------------------------------------------------------------------
// Local state: one maildir per IMAP mailbox plus a state.json cache.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct FolderState {
    uid_validity: Option<u32>,
    #[serde(default)]
    messages: BTreeMap<u32, MsgEntry>,
}

#[derive(Serialize, Deserialize)]
struct MsgEntry {
    /// Path relative to the maildir, e.g. "cur/1234.0_1.u42.host:2,S"
    file: String,
    /// Raw IMAP flag atoms, e.g. ["\\Seen", "\\Flagged"]
    #[serde(default)]
    flags: Vec<String>,
}

/// IMAP system flag -> maildir flag letter.
fn flag_letter(f: &Flag) -> Option<char> {
    match f {
        Flag::Seen => Some('S'),
        Flag::Answered => Some('R'),
        Flag::Flagged => Some('F'),
        Flag::Deleted => Some('T'),
        Flag::Draft => Some('D'),
        _ => None,
    }
}

fn letter_flag(c: char) -> Option<&'static str> {
    match c {
        'S' => Some("\\Seen"),
        'R' => Some("\\Answered"),
        'F' => Some("\\Flagged"),
        'T' => Some("\\Deleted"),
        'D' => Some("\\Draft"),
        _ => None,
    }
}

fn letters(flags: &[Flag]) -> String {
    let mut v: Vec<char> = flags.iter().filter_map(flag_letter).collect();
    v.sort_unstable();
    v.dedup();
    v.into_iter().collect()
}

fn flag_strings(flags: &[Flag]) -> Vec<String> {
    let mut v: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
    v.sort();
    v
}

/// Globally unique maildir base name; embeds the UID so local state can be
/// rebuilt from filenames alone if state.json is lost.
fn unique_base(uid: u32) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let host = env::var("HOSTNAME").unwrap_or_else(|_| "backup".into());
    format!(
        "{}.{:09}.{}_{}.u{}.{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        uid,
        host
    )
}

/// Maildir location for a message with the given flags.
/// No flags -> `new/<base>`; otherwise `cur/<base>:2,<letters>`.
fn rel_path(base: &str, flags: &[Flag]) -> String {
    let l = letters(flags);
    if l.is_empty() {
        format!("new/{base}")
    } else {
        format!("cur/{base}:2,{l}")
    }
}

/// Inverse of rel_path: (base, flag letters) from a maildir basename.
fn parse_basename(name: &str) -> Option<(&str, Vec<char>)> {
    match name.split_once(":2,") {
        Some((base, fl)) => Some((base, fl.chars().collect())),
        None if !name.contains(':') => Some((name, vec![])),
        None => None,
    }
}

/// Extract the UID embedded in a base name by unique_base().
fn base_uid(base: &str) -> Option<u32> {
    base.split('.')
        .find_map(|seg| seg.strip_prefix('u')?.parse().ok())
}

fn load_state(dir: &Path) -> FolderState {
    let mut state: FolderState = fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // The filesystem is the truth: rebuild entries for files on disk that
    // state.json does not know about (e.g. after an interrupted run), and
    // drop entries whose file vanished.
    for sub in ["cur", "new"] {
        if let Ok(rd) = fs::read_dir(dir.join(sub)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some((base, fl)) = parse_basename(&name) else {
                    continue;
                };
                let Some(uid) = base_uid(base) else { continue };
                let mut flags: Vec<String> = fl
                    .iter()
                    .filter_map(|c| letter_flag(*c).map(String::from))
                    .collect();
                flags.sort();
                // Disk is authoritative for the file location; keep the
                // richer flag list from state.json if we have one.
                state
                    .messages
                    .entry(uid)
                    .and_modify(|m| m.file = format!("{sub}/{name}"))
                    .or_insert_with(|| MsgEntry {
                        file: format!("{sub}/{name}"),
                        flags,
                    });
            }
        }
    }
    state.messages.retain(|_, m| dir.join(&m.file).is_file());
    state
}

fn save_state(dir: &Path, state: &FolderState) -> Result<()> {
    let tmp = dir.join("state.json.tmp");
    fs::write(&tmp, serde_json::to_string(state)?)?;
    fs::rename(&tmp, dir.join("state.json"))?;
    Ok(())
}

fn create_maildir(dir: &Path) -> Result<()> {
    for sub in ["tmp", "new", "cur"] {
        fs::create_dir_all(dir.join(sub))?;
    }
    Ok(())
}

/// Percent-encode a mailbox name segment for use as a directory name.
fn sanitize_segment(seg: &str) -> String {
    if seg.is_empty() || seg.chars().all(|c| c == '.') {
        return "%2E".repeat(seg.len().max(1));
    }
    let mut out = String::with_capacity(seg.len());
    for c in seg.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '+' | ' ' | '@' => out.push(c),
            c if (c as u32) >= 0x80 => out.push(c),
            c => {
                for &b in c.encode_utf8(&mut [0; 4]).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

fn folder_dir(output: &Path, name: &str, delimiter: Option<&str>) -> PathBuf {
    let segs: Vec<&str> = match delimiter {
        Some(d) if !d.is_empty() => name.split(d).collect(),
        _ => vec![name],
    };
    segs.iter()
        .map(|s| sanitize_segment(s))
        .fold(output.to_path_buf(), |p, s| p.join(s))
}

// ---------------------------------------------------------------------------
// Read-only IMAP access — layered safeguards against modifying the server:
//
//   1. COMPILE-TIME: ReadOnlySession exposes ONLY non-mutating commands.
//      There is no way to reach STORE/EXPUNGE/APPEND/COPY/MOVE/CREATE/
//      DELETE/RENAME/SUBSCRIBE/SELECT/CHECK/CLOSE through it — the inner
//      Session is never re-exposed (no Deref, no accessor).
//   2. RUNTIME: every FETCH/UID FETCH query is checked against a whitelist
//      of side-effect-free items before it is sent. BODY[], BODY[...],
//      RFC822, RFC822.HEADER/TEXT — all of which set \Seen — are rejected.
//   3. SERVER-ENFORCED: mailboxes are opened with EXAMINE (RFC 3501
//      read-only select). In this state the server MUST reject flag updates
//      and MUST NOT alter \Recent, so even a hypothetical bug above cannot
//      change mailbox state.
//   4. SEMANTIC: message bodies are fetched with BODY.PEEK[] only, which per
//      RFC 3501 never sets \Seen.
//
// The only IMAP commands this program can emit are:
//   LOGIN, LIST, EXAMINE, UID FETCH (validated), LOGOUT.
// For real belt-and-suspenders, also use a dedicated read-only or
// app-password account on the server side — see README.
// ---------------------------------------------------------------------------

/// FETCH items that provably have no side effects on server state.
const SAFE_FETCH_ITEMS: &[&str] = &[
    "FLAGS",
    "UID",
    "INTERNALDATE",
    "RFC822.SIZE",
    "ENVELOPE",
    "BODY", // no section => bodystructure metadata, no \Seen
    "BODYSTRUCTURE",
    "MODSEQ",
    "EMAILID",
    "THREADID",
];

/// Reject any FETCH query that could have side effects. Whitelist approach:
/// anything not explicitly known to be safe is refused.
fn check_fetch_query(query: &str) -> Result<()> {
    let q = query.trim();
    let q = q.strip_prefix('(').unwrap_or(q);
    let q = q.strip_suffix(')').unwrap_or(q);
    let b = q.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'(' || b[i] == b')') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && !matches!(b[i], b'[' | b'<' | b'(' | b')')
        {
            i += 1;
        }
        let name = q[start..i].to_ascii_uppercase();
        if name.is_empty() {
            continue;
        }

        // Optional [section] and/or <partial> suffix.
        let mut has_section = false;
        if i < b.len() && b[i] == b'[' {
            has_section = true;
            while i < b.len() && b[i] != b']' {
                i += 1;
            }
            i += 1;
        }
        if i < b.len() && b[i] == b'<' {
            while i < b.len() && b[i] != b'>' {
                i += 1;
            }
            i += 1;
        }

        match name.as_str() {
            // PEEK sections never set \Seen — the only body access allowed.
            "BODY.PEEK" => {
                if !has_section {
                    bail!("FETCH {query:?}: BODY.PEEK requires a [section]")
                }
            }
            n if SAFE_FETCH_ITEMS.contains(&n) => {
                if has_section {
                    bail!(
                        "FETCH {query:?}: item {n} with a [section] can fetch body \
                         data without PEEK and set \\Seen"
                    )
                }
            }
            _ => bail!("FETCH {query:?}: item {name} is not on the read-only whitelist"),
        }
    }
    Ok(())
}

/// An IMAP session that is physically incapable of mutating the server.
/// Wraps an authenticated `Session` and exposes only read-only commands.
struct ReadOnlySession {
    session: Session<Connection>,
}

#[allow(dead_code)] // expose the full safe surface; unused parts stay available
impl ReadOnlySession {
    /// LIST is a pure query.
    fn list(
        &mut self,
        reference: Option<&str>,
        pattern: Option<&str>,
    ) -> imap::Result<imap::types::Names> {
        self.session.list(reference, pattern)
    }

    /// EXAMINE is the read-only form of SELECT (RFC 3501 §6.3.2): the server
    /// opens the mailbox read-only and must not touch \Recent.
    fn examine(&mut self, mailbox: &str) -> imap::Result<imap::types::Mailbox> {
        self.session.examine(mailbox)
    }

    /// UID FETCH with a validated read-only query.
    fn uid_fetch(&mut self, set: &str, query: &str) -> Result<imap::types::Fetches> {
        check_fetch_query(query)?;
        self.session.uid_fetch(set, query).map_err(Into::into)
    }

    /// FETCH (by sequence number) with a validated read-only query.
    fn fetch(&mut self, set: &str, query: &str) -> Result<imap::types::Fetches> {
        check_fetch_query(query)?;
        self.session.fetch(set, query).map_err(Into::into)
    }

    /// NOOP: safe, useful as a keep-alive.
    fn noop(&mut self) -> imap::Result<()> {
        self.session.noop()
    }

    fn logout(&mut self) -> imap::Result<()> {
        self.session.logout()
    }
}

fn connect(cfg: &Config) -> Result<ReadOnlySession> {
    let client = ClientBuilder::new(&cfg.host, cfg.port)
        .mode(cfg.mode.clone())
        .tls_kind(TlsKind::Rust)
        .danger_skip_tls_verify(cfg.tls_insecure)
        .connect()
        .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
    let session = client
        .login(&cfg.user, &cfg.password)
        .map_err(|(e, _)| e)
        .context("IMAP login failed")?;
    Ok(ReadOnlySession { session })
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
    fetched: Vec<(u32, String, Vec<String>)>, // (uid, rel path, flags)
    failed: Vec<u32>,
}

fn write_message(dir: &Path, uid: u32, flags: &[Flag], body: &[u8]) -> std::io::Result<String> {
    let base = unique_base(uid);
    let rel = rel_path(&base, flags);
    fs::write(dir.join("tmp").join(&base), body)?;
    fs::rename(dir.join("tmp").join(&base), dir.join(&rel))?;
    Ok(rel)
}

fn worker(
    cfg: Arc<Config>,
    rx: Arc<Mutex<Receiver<Task>>>,
    tx: Sender<FolderResult>,
) {
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
                match connect(&cfg) {
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
            match sess.uid_fetch(&set, "(FLAGS BODY.PEEK[])") {
                Ok(fetches) => {
                    let mut seen = Vec::new();
                    for f in fetches.iter() {
                        let uid = f.uid.unwrap_or(0);
                        seen.push(uid);
                        match f.body() {
                            Some(body) => match write_message(&task.dir, uid, f.flags(), body) {
                                Ok(rel) => {
                                    result.fetched.push((uid, rel, flag_strings(f.flags())));
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
    println!("running restic: -r {repo} backup {} --tag {}", cfg.output.display(), cfg.restic_tag);
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

    let mut session = connect(&cfg)?;
    println!("connected to {}:{} as {}", cfg.host, cfg.port, cfg.user);

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
        if state.uid_validity.is_some_and(|uv| uv != uid_validity)
            && !state.messages.is_empty()
        {
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
        for (uid, rel, flags) in res.fetched {
            state.messages.insert(uid, MsgEntry { file: rel, flags });
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_encodes_unsafe_chars() {
        assert_eq!(sanitize_segment("INBOX"), "INBOX");
        assert_eq!(sanitize_segment("Sent Items"), "Sent Items");
        assert_eq!(sanitize_segment("a/b\\c"), "a%2Fb%5Cc");
        assert_eq!(sanitize_segment(".."), "%2E%2E");
        assert_eq!(sanitize_segment("100%"), "100%25");
        assert_eq!(sanitize_segment("Entwürfe"), "Entwürfe");
    }

    #[test]
    fn maildir_round_trip() {
        let base = unique_base(42);
        assert_eq!(base_uid(&base), Some(42));

        // no flags -> new/, no ":2," suffix
        let rel = rel_path(&base, &[]);
        assert!(rel.starts_with("new/"));
        let (b, fl) = parse_basename(&rel[4..]).unwrap();
        assert_eq!(b, base);
        assert!(fl.is_empty());

        // flags -> cur/ with sorted letters
        let flags = vec![Flag::Seen, Flag::Flagged, Flag::Answered];
        let rel = rel_path(&base, &flags);
        assert!(rel.starts_with("cur/"));
        assert!(rel.ends_with(":2,FRS"));
        let (b, fl) = parse_basename(rel.strip_prefix("cur/").unwrap()).unwrap();
        assert_eq!(b, base);
        assert_eq!(fl, vec!['F', 'R', 'S']);
    }

    #[test]
    fn fetch_query_whitelist() {
        // safe queries — everything this program sends
        for q in [
            "(FLAGS)",
            "(UID FLAGS)",
            "(FLAGS BODY.PEEK[])",
            "FLAGS BODY.PEEK[] INTERNALDATE",
            "BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)]",
            "ENVELOPE RFC822.SIZE MODSEQ BODYSTRUCTURE",
            "UID BODY",
        ] {
            assert!(check_fetch_query(q).is_ok(), "{q} should be allowed");
        }

        // dangerous queries — anything that can set \Seen or worse
        for q in [
            "RFC822",                    // sets \Seen
            "(RFC822)",                  // sets \Seen
            "BODY[]",                    // sets \Seen
            "BODY[HEADER]",              // sets \Seen
            "BODY[TEXT]",                // sets \Seen
            "RFC822.HEADER",             // sets \Seen
            "RFC822.TEXT",               // sets \Seen
            "BINARY[]",                  // not whitelisted
            "FLAGS BODY[]",              // mixed in
            "BODY.PEEK",                 // PEEK without section
            "STORE +FLAGS",              // not a fetch item at all
            "FLAGS; STORE +FLAGS",       // injection attempt
        ] {
            assert!(check_fetch_query(q).is_err(), "{q} should be rejected");
        }
    }

    #[test]
    fn folder_dir_splits_on_delimiter() {
        let out = Path::new("/backup");
        assert_eq!(
            folder_dir(out, "INBOX/Sub", Some("/")),
            Path::new("/backup/INBOX/Sub")
        );
        // escaping a literal slash inside a segment
        assert_eq!(
            folder_dir(out, "a/b", Some("|")),
            Path::new("/backup/a%2Fb")
        );
    }
}
