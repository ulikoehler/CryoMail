//! Shared library for the CryoMail tools (`cryomail` backup and
//! `cryomail-restore`): maildir-on-disk layout, per-folder state, ini/CLI
//! helpers, TLS connect, and the read-only IMAP session used by the backup.

use anyhow::{Context, Result, bail};
use configparser::ini::Ini;
use imap::types::Flag;
use imap::{ClientBuilder, Connection, ConnectionMode, Session, TlsKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Config helpers: ini values are defaults, CLI flags override them.
// ---------------------------------------------------------------------------

pub fn ini_str(ini: &Ini, section: &str, key: &str) -> Option<String> {
    ini.get(section, key).filter(|s| !s.trim().is_empty())
}

pub fn ini_num<T: std::str::FromStr>(ini: &Ini, section: &str, key: &str) -> Result<Option<T>>
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

pub fn load_ini(path: Option<&Path>) -> Result<Ini> {
    let mut ini = Ini::new();
    if let Some(path) = path {
        ini.load(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    }
    Ok(ini)
}

pub fn parse_tls_mode(s: &str) -> Result<ConnectionMode> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(ConnectionMode::AutoTls),
        "tls" | "ssl" => Ok(ConnectionMode::Tls),
        "starttls" => Ok(ConnectionMode::StartTls),
        "none" | "off" | "plaintext" => Ok(ConnectionMode::Plaintext),
        other => bail!("invalid tls mode {other:?} (auto|tls|starttls|none)"),
    }
}

/// Password resolution: CLI flag > ini > CRYOMAIL_PASSWORD env > legacy
/// EMAIL_BACKUP_PASSWORD env > interactive prompt.
pub fn resolve_password(
    cli: Option<String>,
    ini_pw: Option<String>,
    user: &str,
    host: &str,
) -> Result<String> {
    cli.or(ini_pw)
        .or_else(|| env::var("CRYOMAIL_PASSWORD").ok())
        .or_else(|| env::var("EMAIL_BACKUP_PASSWORD").ok())
        .filter(|s| !s.is_empty())
        .map(Ok)
        .unwrap_or_else(|| rpassword::prompt_password(format!("IMAP password for {user}@{host}: ")))
        .context("reading IMAP password")
}

// ---------------------------------------------------------------------------
// IMAP connection
// ---------------------------------------------------------------------------

pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub mode: ConnectionMode,
    pub tls_insecure: bool,
}

/// Raw authenticated session. The backup wraps this in `ReadOnlySession`;
/// the restore tool needs the full session (APPEND/CREATE are its job).
pub fn connect(p: &ConnectParams) -> Result<Session<Connection>> {
    let client = ClientBuilder::new(&p.host, p.port)
        .mode(p.mode.clone())
        .tls_kind(TlsKind::Rust)
        .danger_skip_tls_verify(p.tls_insecure)
        .connect()
        .with_context(|| format!("connecting to {}:{}", p.host, p.port))?;
    client
        .login(&p.user, &p.password)
        .map_err(|(e, _)| e)
        .context("IMAP login failed")
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
// The backup can emit only: LOGIN, LIST, EXAMINE, UID FETCH (validated),
// NOOP, LOGOUT. The restore binary deliberately does NOT use this wrapper —
// APPEND/CREATE are its whole job — but it is a separate, explicitly
// write-enabled code path.
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
pub fn check_fetch_query(query: &str) -> Result<()> {
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
pub struct ReadOnlySession {
    session: Session<Connection>,
}

impl ReadOnlySession {
    pub fn new(session: Session<Connection>) -> Self {
        ReadOnlySession { session }
    }

    /// LIST is a pure query.
    pub fn list(
        &mut self,
        reference: Option<&str>,
        pattern: Option<&str>,
    ) -> imap::Result<imap::types::Names> {
        self.session.list(reference, pattern)
    }

    /// EXAMINE is the read-only form of SELECT (RFC 3501 §6.3.2): the server
    /// opens the mailbox read-only and must not touch \Recent.
    pub fn examine(&mut self, mailbox: &str) -> imap::Result<imap::types::Mailbox> {
        self.session.examine(mailbox)
    }

    /// UID FETCH with a validated read-only query.
    pub fn uid_fetch(&mut self, set: &str, query: &str) -> Result<imap::types::Fetches> {
        check_fetch_query(query)?;
        self.session.uid_fetch(set, query).map_err(Into::into)
    }

    /// FETCH (by sequence number) with a validated read-only query.
    pub fn fetch(&mut self, set: &str, query: &str) -> Result<imap::types::Fetches> {
        check_fetch_query(query)?;
        self.session.fetch(set, query).map_err(Into::into)
    }

    /// NOOP: safe, useful as a keep-alive.
    pub fn noop(&mut self) -> imap::Result<()> {
        self.session.noop()
    }

    pub fn logout(&mut self) -> imap::Result<()> {
        self.session.logout()
    }
}

// ---------------------------------------------------------------------------
// Flags <-> maildir letters
// ---------------------------------------------------------------------------

/// IMAP system flag -> maildir flag letter.
pub fn flag_letter(f: &Flag) -> Option<char> {
    match f {
        Flag::Seen => Some('S'),
        Flag::Answered => Some('R'),
        Flag::Flagged => Some('F'),
        Flag::Deleted => Some('T'),
        Flag::Draft => Some('D'),
        _ => None,
    }
}

pub fn letter_flag(c: char) -> Option<&'static str> {
    match c {
        'S' => Some("\\Seen"),
        'R' => Some("\\Answered"),
        'F' => Some("\\Flagged"),
        'T' => Some("\\Deleted"),
        'D' => Some("\\Draft"),
        _ => None,
    }
}

pub fn letters(flags: &[Flag]) -> String {
    let mut v: Vec<char> = flags.iter().filter_map(flag_letter).collect();
    v.sort_unstable();
    v.dedup();
    v.into_iter().collect()
}

pub fn flag_strings(flags: &[Flag]) -> Vec<String> {
    let mut v: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Local state: one maildir per IMAP mailbox plus a state.json index.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
pub struct FolderState {
    pub uid_validity: Option<u32>,
    #[serde(default)]
    pub messages: BTreeMap<u32, MsgEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct MsgEntry {
    /// Path relative to the maildir, e.g. "cur/1234.0_1.u42.host:2,S"
    pub file: String,
    /// Raw IMAP flag atoms, e.g. ["\\Seen", "\\Flagged"]
    #[serde(default)]
    pub flags: Vec<String>,
    /// Original INTERNALDATE (RFC3339), if captured during backup.
    #[serde(default)]
    pub internal_date: Option<String>,
}

/// Globally unique maildir base name; embeds the UID so local state can be
/// rebuilt from filenames alone if state.json is lost.
pub fn unique_base(uid: u32) -> String {
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
pub fn rel_path(base: &str, flags: &[Flag]) -> String {
    let l = letters(flags);
    if l.is_empty() {
        format!("new/{base}")
    } else {
        format!("cur/{base}:2,{l}")
    }
}

/// Inverse of rel_path: (base, flag letters) from a maildir basename.
pub fn parse_basename(name: &str) -> Option<(&str, Vec<char>)> {
    match name.split_once(":2,") {
        Some((base, fl)) => Some((base, fl.chars().collect())),
        None if !name.contains(':') => Some((name, vec![])),
        None => None,
    }
}

/// Extract the UID embedded in a base name by unique_base().
pub fn base_uid(base: &str) -> Option<u32> {
    base.split('.')
        .find_map(|seg| seg.strip_prefix('u')?.parse().ok())
}

pub fn load_state(dir: &Path) -> FolderState {
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
                        internal_date: None,
                    });
            }
        }
    }
    state.messages.retain(|_, m| dir.join(&m.file).is_file());
    state
}

pub fn save_state(dir: &Path, state: &FolderState) -> Result<()> {
    let tmp = dir.join("state.json.tmp");
    fs::write(&tmp, serde_json::to_string(state)?)?;
    fs::rename(&tmp, dir.join("state.json"))?;
    Ok(())
}

pub fn create_maildir(dir: &Path) -> Result<()> {
    for sub in ["tmp", "new", "cur"] {
        fs::create_dir_all(dir.join(sub))?;
    }
    Ok(())
}

pub fn write_message(dir: &Path, uid: u32, flags: &[Flag], body: &[u8]) -> std::io::Result<String> {
    let base = unique_base(uid);
    let rel = rel_path(&base, flags);
    fs::write(dir.join("tmp").join(&base), body)?;
    fs::rename(dir.join("tmp").join(&base), dir.join(&rel))?;
    Ok(rel)
}

// ---------------------------------------------------------------------------
// Mailbox name <-> filesystem path mapping
// ---------------------------------------------------------------------------

/// Percent-encode a mailbox name segment for use as a directory name.
pub fn sanitize_segment(seg: &str) -> String {
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

/// Inverse of sanitize_segment: percent-decode a directory name back into
/// the original mailbox name segment.
pub fn percent_decode(seg: &str) -> String {
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Local directory for an IMAP mailbox name: split on the server hierarchy
/// delimiter and sanitize each segment.
pub fn folder_dir(output: &Path, name: &str, delimiter: Option<&str>) -> PathBuf {
    let segs: Vec<&str> = match delimiter {
        Some(d) if !d.is_empty() => name.split(d).collect(),
        _ => vec![name],
    };
    segs.iter()
        .map(|s| sanitize_segment(s))
        .fold(output.to_path_buf(), |p, s| p.join(s))
}

/// Inverse of folder_dir: recover the mailbox name from a local directory's
/// path relative to the backup root, joining percent-decoded segments with
/// the target server's hierarchy delimiter.
pub fn dir_to_mailbox(rel: &Path, delimiter: &str) -> String {
    rel.iter()
        .map(|s| percent_decode(&s.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(delimiter)
}

/// Recursively find maildir folders under `root` (a dir counts if it has a
/// `cur`/`new`/`tmp` subdir or a state.json). Maildirs nest: `Work` can be a
/// maildir AND contain `Projects/` which is another maildir. Moved-aside
/// `*.stale-uv<N>` copies are never restored.
pub fn find_maildirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
        let is_maildir = ["cur", "new", "tmp"].iter().any(|s| dir.join(s).is_dir())
            || dir.join("state.json").is_file();
        if is_maildir
            && let Ok(rel) = dir.strip_prefix(root)
            && !rel.as_os_str().is_empty()
        {
            out.push(dir.to_path_buf());
        }
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                // cur/new/tmp can only hold messages; stale copies are moved
                // aside by the backup and must not be re-uploaded.
                if matches!(&*name, "cur" | "new" | "tmp") || name.contains(".stale-uv") {
                    continue;
                }
                if e.path().is_dir() {
                    walk(&e.path(), root, out);
                }
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
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
    fn sanitize_round_trip() {
        for s in [
            "INBOX",
            "Sent Items",
            "a/b\\c",
            "..",
            "100%",
            "Entwürfe",
            "Personal &- Family",
            "a|b:c*d",
        ] {
            assert_eq!(percent_decode(&sanitize_segment(s)), s);
        }
    }

    #[test]
    fn dir_to_mailbox_decodes_and_joins() {
        assert_eq!(
            dir_to_mailbox(Path::new("Work/Projects/2024"), "/"),
            "Work/Projects/2024"
        );
        assert_eq!(
            dir_to_mailbox(Path::new("Work/Projects"), "."),
            "Work.Projects"
        );
        assert_eq!(dir_to_mailbox(Path::new("a%2Fb"), "/"), "a/b");
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
            "(FLAGS INTERNALDATE BODY.PEEK[])",
            "FLAGS BODY.PEEK[] INTERNALDATE",
            "BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)]",
            "ENVELOPE RFC822.SIZE MODSEQ BODYSTRUCTURE",
            "UID BODY",
        ] {
            assert!(check_fetch_query(q).is_ok(), "{q} should be allowed");
        }

        // dangerous queries — anything that can set \Seen or worse
        for q in [
            "RFC822",              // sets \Seen
            "(RFC822)",            // sets \Seen
            "BODY[]",              // sets \Seen
            "BODY[HEADER]",        // sets \Seen
            "BODY[TEXT]",          // sets \Seen
            "RFC822.HEADER",       // sets \Seen
            "RFC822.TEXT",         // sets \Seen
            "BINARY[]",            // not whitelisted
            "FLAGS BODY[]",        // mixed in
            "BODY.PEEK",           // PEEK without section
            "STORE +FLAGS",        // not a fetch item at all
            "FLAGS; STORE +FLAGS", // injection attempt
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
