//! Durable session records (`scrollback-persist`): one `<key>.vt` file per keyed session under
//! `$XDG_STATE_HOME/crucible-term/sessions/` (directory 0700, files 0600), so a session's
//! history survives a reboot and is replayed above the new child of the next open with that key.
//!
//! - **Format.** One header line, `cterm-session 1 key=<key> cols=<c> rows=<r> lines=<n>
//!   saved=<unix> cwd=<pct-encoded>`, then the generic VT snapshot
//!   ([`crucible_term_core::snapshot`]) of the tail (at most `persist-lines` history rows plus the
//!   screen). Unknown fields are ignored; an unknown version or a malformed record is skipped with
//!   one log line and left on disk. Never raw PTY bytes, never the environment.
//! - **Writes.** The loop thread only copies rows ([`capture`]); one writer thread encodes,
//!   writes `.<key>.vt.<pid>.tmp` (0600), fsyncs and renames it over the record. A capture is
//!   taken when the session has been quiet for [`Timing::quiet`], or when dirty output has gone
//!   unsaved for [`Timing::max_age`], or when the session finishes; never while the alternate
//!   screen is active, and never when nothing changed since the last one.
//! - **Ownership.** A daemon touches only the records of keys it holds the `<key>.lock` flock
//!   for ([`Owned`]), from the keyed open until the record is deleted (or the daemon exits).
//!   Daemons are per display but share this directory, so a key another live daemon owns is
//!   never replayed, written or deleted here, and the startup sweep skips it.
//! - **Lifecycle.** A finished session's record is deleted [`Timing::grace`] after its last
//!   write; SIGTERM/SIGHUP ([`Persist::shutdown`]) cancels every pending deletion and writes
//!   every live session, so a reboot keeps the records and a real close removes them.
//! - **Retention.** At startup ([`sweep`]) records older than `persist-days` are deleted, and so
//!   are temp files whose pid is no longer a running `cterm daemon`.

use super::TermId;
use crucible_term_core::{Grid, Session};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// The record format version this build writes and reads.
pub const VERSION: u32 = 1;
/// TEST-ONLY: `quiet,max_age,grace` in ms (shortens the cadence) and logs every save.
pub const TIMING_VAR: &str = "CTERM_DEBUG_PERSIST_MS";
/// Largest record read back.
const MAX_RECORD: u64 = 64 << 20;
/// How long shutdown waits for the writer to drain.
const JOIN_BOUND: Duration = Duration::from_secs(10);
/// The separator written below a replayed record (faint, so it stays unobtrusive).
const SEPARATOR: &str = "-- restored from record --";

/// The capture cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Capture after this long without output.
    pub quiet: Duration,
    /// Capture output that has been dirty and unsaved this long, even if never quiet.
    pub max_age: Duration,
    /// Delete a finished session's record this long after it finished (at least 5 s by
    /// default, so a shutdown signal racing the shells' exits keeps it).
    pub grace: Duration,
    /// Log every save (test mode).
    pub log_saves: bool,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            quiet: Duration::from_secs(2),
            max_age: Duration::from_secs(30),
            grace: Duration::from_secs(6),
            log_saves: false,
        }
    }
}

impl Timing {
    /// The default cadence, or the TEST-ONLY `quiet,max_age,grace` ms override.
    pub fn from_env(v: Option<&str>) -> Timing {
        let ms: Option<Vec<u64>> =
            v.and_then(|v| v.split(',').map(|p| p.trim().parse().ok()).collect());
        match ms.as_deref() {
            Some(&[q, m, g]) => Timing {
                quiet: Duration::from_millis(q),
                max_age: Duration::from_millis(m),
                grace: Duration::from_millis(g),
                log_saves: true,
            },
            _ => Timing::default(),
        }
    }
}

/// A key usable as a file name: 1-128 of `[A-Za-z0-9_-]` (CDE keys are 32 hex digits).
pub fn valid_key(k: &str) -> bool {
    (1..=128).contains(&k.len())
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Percent-encode `s` for a header field (everything outside `[A-Za-z0-9/._~-]`).
pub fn pct_encode(s: &[u8]) -> String {
    let mut o = String::new();
    for &b in s {
        if b.is_ascii_alphanumeric() || b"/._~-".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// The header line (with its newline).
pub fn header(key: &str, cols: usize, rows: usize, lines: usize, saved: u64, cwd: &[u8]) -> String {
    format!(
        "cterm-session {VERSION} key={key} cols={cols} rows={rows} lines={lines} saved={saved} cwd={}\n",
        pct_encode(cwd)
    )
}

/// A record read back from disk.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    /// Screen size it was saved at.
    pub cols: usize,
    /// Screen rows it was saved at.
    pub rows: usize,
    /// The VT snapshot.
    pub body: Vec<u8>,
}

/// Parse a record's bytes for `key`: the header must be version [`VERSION`], name `key`, and
/// carry sane `cols`/`rows`; unknown fields are ignored.
pub fn parse(bytes: &[u8], key: &str) -> Result<Record, String> {
    let nl = bytes
        .iter()
        .take(4096)
        .position(|&b| b == b'\n')
        .ok_or("no header line")?;
    let head = std::str::from_utf8(&bytes[..nl]).map_err(|_| "header is not UTF-8")?;
    let mut words = head.split(' ');
    if words.next() != Some("cterm-session") {
        return Err("not a session record".into());
    }
    match words.next() {
        Some(v) if v == VERSION.to_string() => {}
        v => return Err(format!("unknown version {:?}", v.unwrap_or(""))),
    }
    let (mut k, mut cols, mut rows) = (None, None, None);
    for w in words {
        let Some((n, v)) = w.split_once('=') else {
            continue;
        };
        let dim = || {
            v.parse::<usize>()
                .ok()
                .filter(|&d| (1..=10_000).contains(&d))
        };
        match n {
            "key" => k = Some(v),
            "cols" => cols = dim(),
            "rows" => rows = dim(),
            _ => {}
        }
    }
    if k != Some(key) {
        return Err("key mismatch".into());
    }
    match (cols, rows) {
        (Some(cols), Some(rows)) => Ok(Record {
            cols,
            rows,
            body: bytes[nl + 1..].to_vec(),
        }),
        _ => Err("bad cols/rows".into()),
    }
}

/// Copy the tail of `s` for a record: at most `lines` history rows, the screen and the cursor.
/// The only record work on the loop thread.
pub fn capture(s: &Session, lines: usize) -> Grid {
    let (cols, rows) = s.size();
    let h = s.history_len();
    Grid {
        cols,
        rows,
        history: (h.saturating_sub(lines)..h)
            .map(|i| s.history_row(i))
            .collect(),
        screen: (0..rows).map(|y| s.screen_row(y)).collect(),
        cursor: s.cursor(),
    }
}

/// Replay `rec` into a fresh session for a window of `cols` x `rows`: fed at the saved size
/// (the snapshot's same-size guarantee), resized, the whole restored screen scrolled into
/// history, and a separator line written with the cursor on the fresh line below it. Anything
/// the replay would answer (DSR, DA) and every event (title, OSC 52) is discarded. Returns the
/// session and the number of restored rows now in history.
pub fn restored(rec: &Record, cols: usize, rows: usize, scrollback: usize) -> (Session, usize) {
    let mut s = Session::new(rec.cols, rec.rows, scrollback);
    s.feed(&rec.body);
    // Resized before the push: growing pulls history back onto the screen.
    s.resize(cols, rows);
    let (_, saved_rows) = s.size();
    let last = (0..saved_rows)
        .rev()
        .find(|&y| !s.screen_row(y).text().trim().is_empty())
        .map_or(0, |y| y + 1)
        .max(s.cursor().1 + 1)
        .min(saved_rows);
    // Line feeds on the bottom row scroll every row above them into history.
    s.feed(format!("\x1b[{saved_rows};1H{}", "\n".repeat(last)).as_bytes());
    let n = s.history_len();
    let sep: String = SEPARATOR.chars().take(cols.max(1)).collect();
    s.feed(format!("\x1b[H\x1b[2m{sep}\x1b[0m\r\n").as_bytes());
    let _ = s.take_replies();
    let _ = s.take_events();
    let _ = s.take_damage();
    (s, n)
}

/// The per-key lock, held while this daemon owns the key's record.
#[derive(Debug)]
pub struct Lock {
    /// Held only for its flock (released on drop).
    _file: File,
}

/// Why a key's lock was not taken.
#[derive(Debug, PartialEq, Eq)]
pub enum LockErr {
    /// Another live daemon (or, in-process, another owner) holds it.
    Busy,
    /// The lock file could not be opened.
    Io(String),
}

/// Take `<dir>/<key>.lock` (non-blocking flock), re-checking after the flock that the path still
/// names the locked inode (a sweeper may have unlinked it between open and flock).
pub fn lock(dir: &Path, key: &str) -> Result<Lock, LockErr> {
    let path = dir.join(format!("{key}.lock"));
    for _ in 0..3 {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| LockErr::Io(format!("{}: {e}", path.display())))?;
        match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(LockErr::Busy),
            Err(e) => return Err(LockErr::Io(format!("{}: flock: {e}", path.display()))),
        }
        let same = match (f.metadata(), std::fs::symlink_metadata(&path)) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => false,
        };
        if same {
            return Ok(Lock { _file: f });
        }
    }
    Err(LockErr::Busy)
}

/// Read `<dir>/<key>.vt`: `Ok(None)` when absent; an error (to log, record left on disk) for a
/// symlink, a non-regular or oversized file, or an unparsable record.
pub fn load(dir: &Path, key: &str) -> Result<Option<Record>, String> {
    let path = dir.join(format!("{key}.vt"));
    let mut f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(format!("{}: is a symlink", path.display()));
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let m = f
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !m.is_file() || m.len() > MAX_RECORD {
        return Err(format!("{}: not a regular record file", path.display()));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&bytes, key)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Write `bytes` as `<dir>/<key>.vt` atomically: a 0600 temp named with this pid, fsync, rename,
/// fsync the directory.
pub fn write_atomic(dir: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{key}.vt.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let r = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(format!("{key}.vt")))?;
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Delete `<key>.vt` and then its lock file, while still holding the lock.
fn delete(dir: &Path, key: &str, lock: Lock) {
    let _ = std::fs::remove_file(dir.join(format!("{key}.vt")));
    let _ = std::fs::remove_file(dir.join(format!("{key}.lock")));
    drop(lock);
}

/// Whether `pid` is a running `cterm daemon` (its argv is `…cterm… daemon`).
fn is_cterm_daemon(pid: u32) -> bool {
    let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let mut args = cmd.split(|&b| b == 0);
    let argv0 = args.next().unwrap_or_default();
    let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or_default();
    base.starts_with(b"cterm") && args.next() == Some(b"daemon".as_slice())
}

/// The startup sweep of `dir`: delete records older than `days` (0: none) and their lock files,
/// unlocked lock files with no record, and temp files whose pid is no longer a running
/// `cterm daemon`. Keys another daemon holds are skipped. Returns the deleted names.
pub fn sweep(dir: &Path, days: u64, now: SystemTime) -> Vec<String> {
    let mut gone = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return gone;
    };
    let max_age = Duration::from_secs(days.saturating_mul(86_400));
    // Listed first: the loop below creates and removes lock files.
    let names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    for name in names {
        let path = dir.join(&name);
        if let Some(rest) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".tmp")) {
            let pid = rest.rsplit('.').next().and_then(|p| p.parse::<u32>().ok());
            if pid.is_some_and(|p| p != std::process::id() && !is_cterm_daemon(p)) {
                let _ = std::fs::remove_file(&path);
                gone.push(name);
            }
            continue;
        }
        let (key, is_vt) = if let Some(k) = name.strip_suffix(".vt") {
            (k, true)
        } else if let Some(k) = name.strip_suffix(".lock") {
            (k, false)
        } else {
            continue;
        };
        if !valid_key(key) {
            continue;
        }
        if !is_vt && dir.join(format!("{key}.vt")).exists() {
            continue; // handled with its record
        }
        let old = || {
            days > 0
                && std::fs::symlink_metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|age| age > max_age)
        };
        if is_vt && !old() {
            continue;
        }
        let Ok(l) = lock(dir, key) else {
            continue;
        };
        delete(dir, key, l);
        gone.push(name);
    }
    gone
}

/// The records directory under the state dir, created 0700.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// The record identity of one session, kept apart from the reattach key (which a detached
/// timeout clears): the key, its lock, and what was last saved.
#[derive(Debug)]
pub struct Owned {
    /// The session key the record is named by.
    pub key: String,
    lock: Lock,
    /// Output generation of the last capture.
    saved_gen: u64,
    /// When unsaved output was first seen.
    dirty_since: Option<Instant>,
}

/// What the loop needs to know about one session's output (from the backend).
pub struct View<'a> {
    /// The session.
    pub session: &'a Session,
    /// Bumped on every PTY read.
    pub generation: u64,
    /// When output was last read.
    pub last_output: Instant,
    /// The child exited or the window was asked to close (removed at the next reap).
    pub finishing: bool,
}

enum Job {
    Write {
        key: String,
        cwd: Vec<u8>,
        grid: Grid,
    },
    Delete {
        key: String,
        lock: Lock,
    },
    Quit(mpsc::Sender<()>),
}

/// How [`Persist::claim`] answered a keyed open.
pub enum Claim {
    /// Newly owned: replay the record if there is one.
    Fresh(Owned),
    /// Taken over from this daemon's own finished session awaiting deletion: no replay (that
    /// session was closed), the new session overwrites the record.
    Reused(Owned),
    /// Not persisted (another daemon owns the key, or the key is unusable).
    None,
}

/// The daemon's persistence state (present only with `scrollback-persist`).
pub struct Persist {
    /// The records directory.
    pub dir: PathBuf,
    lines: usize,
    /// The capture cadence.
    pub timing: Timing,
    tx: mpsc::Sender<Job>,
    /// Finished sessions' records awaiting deletion.
    pending: Vec<(Instant, Owned)>,
}

impl Persist {
    /// Create `dir`, sweep it and start the writer thread.
    pub fn start(dir: PathBuf, lines: usize, days: u64, timing: Timing) -> Result<Persist, String> {
        ensure_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for n in sweep(&dir, days, SystemTime::now()) {
            eprintln!("crucible-term: daemon: swept record file {n}");
        }
        let (tx, rx) = mpsc::channel::<Job>();
        let wdir = dir.clone();
        let log = timing.log_saves;
        std::thread::Builder::new()
            .name("cterm-records".into())
            .spawn(move || writer(&wdir, &rx, log))
            .map_err(|e| format!("writer thread: {e}"))?;
        Ok(Persist {
            dir,
            lines,
            timing,
            tx,
            pending: Vec::new(),
        })
    }

    /// Take ownership of `key`'s record for a new session (see [`Claim`]).
    pub fn claim(&mut self, key: &str, id: TermId) -> Claim {
        if !valid_key(key) {
            eprintln!(
                "crucible-term: daemon: session {id}: key is not a record name; not persisted"
            );
            return Claim::None;
        }
        if let Some(i) = self.pending.iter().position(|(_, o)| o.key == key) {
            let (_, mut o) = self.pending.swap_remove(i);
            o.saved_gen = 0;
            o.dirty_since = None;
            return Claim::Reused(o);
        }
        match lock(&self.dir, key) {
            Ok(lock) => Claim::Fresh(Owned {
                key: key.to_string(),
                lock,
                saved_gen: 0,
                dirty_since: None,
            }),
            Err(LockErr::Busy) => {
                eprintln!(
                    "crucible-term: daemon: session {id}: its record is owned by another daemon; not persisted"
                );
                Claim::None
            }
            Err(LockErr::Io(e)) => {
                eprintln!("crucible-term: daemon: session {id}: record lock: {e}; not persisted");
                Claim::None
            }
        }
    }

    /// Mark `o` as saved at `generation` (the replayed record already holds that state).
    pub fn saved_at(o: &mut Owned, generation: u64) {
        o.saved_gen = generation;
        o.dirty_since = None;
    }

    /// Capture `v` into `o`'s record if it changed and is due (`force`: due now). Never on the
    /// alternate screen.
    pub fn tick(&self, o: &mut Owned, v: &View<'_>, now: Instant, force: bool) {
        if v.generation == o.saved_gen {
            o.dirty_since = None;
            return;
        }
        if v.session.modes().alt_screen {
            return;
        }
        let since = *o.dirty_since.get_or_insert(now);
        let due = force
            || v.finishing
            || now >= v.last_output + self.timing.quiet
            || now >= since + self.timing.max_age;
        if !due {
            return;
        }
        let grid = capture(v.session, self.lines);
        let cwd = v
            .session
            .cwd()
            .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
            .unwrap_or_default();
        let _ = self.tx.send(Job::Write {
            key: o.key.clone(),
            cwd,
            grid,
        });
        o.saved_gen = v.generation;
        o.dirty_since = None;
    }

    /// When `o` next needs a [`Persist::tick`] (`None`: not dirty, or on the alternate screen).
    pub fn due(&self, o: &Owned, v: &View<'_>) -> Option<Instant> {
        if v.generation == o.saved_gen || v.session.modes().alt_screen {
            return None;
        }
        let quiet = v.last_output + self.timing.quiet;
        Some(
            o.dirty_since
                .map_or(quiet, |d| quiet.min(d + self.timing.max_age)),
        )
    }

    /// A finished session's record: delete it after the grace.
    pub fn release(&mut self, o: Owned, now: Instant) {
        self.pending.push((now + self.timing.grace, o));
    }

    /// The earliest pending deletion.
    pub fn next_deletion(&self) -> Option<Instant> {
        self.pending.iter().map(|(t, _)| *t).min()
    }

    /// Delete the records whose grace has passed.
    pub fn expire(&mut self, now: Instant) {
        let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(t, _)| *t <= now);
        self.pending = keep;
        for (_, o) in due {
            let _ = self.tx.send(Job::Delete {
                key: o.key,
                lock: o.lock,
            });
        }
    }

    /// An idle exit (`daemon-linger`): every pending deletion runs now, then the writer drains.
    pub fn close_out(mut self) {
        self.expire(Instant::now() + Duration::from_secs(86_400 * 365));
        self.quit();
    }

    /// Stop the writer after its queue, waiting at most [`JOIN_BOUND`]; whether it finished.
    fn quit(&self) -> bool {
        let (done_tx, done_rx) = mpsc::channel();
        let _ = self.tx.send(Job::Quit(done_tx));
        done_rx.recv_timeout(JOIN_BOUND).is_ok()
    }

    /// SIGTERM/SIGHUP: cancel every pending deletion (the records stay), write every live
    /// session in `live` (not on the alternate screen), and wait for the writer within a bound.
    pub fn shutdown<'a>(mut self, live: impl Iterator<Item = (&'a mut Owned, Option<View<'a>>)>) {
        let kept = self.pending.len();
        self.pending.clear();
        let now = Instant::now();
        let mut n = 0;
        for (o, v) in live {
            if let Some(v) = v {
                self.tick(o, &v, now, true);
                n += 1;
            }
        }
        let flushed = self.quit();
        eprintln!(
            "crucible-term: daemon: records saved for {n} live session(s), {kept} pending deletion(s) kept{}",
            if flushed {
                ""
            } else {
                "; writer did not finish in time"
            }
        );
    }
}

/// The writer thread: encode and write captures, delete records, until `Quit`.
fn writer(dir: &Path, rx: &mpsc::Receiver<Job>, log: bool) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Write { key, cwd, grid } => {
                let saved = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let lines = grid.history.len() + grid.screen.len();
                let mut bytes = header(&key, grid.cols, grid.rows, lines, saved, &cwd).into_bytes();
                bytes.extend_from_slice(&crucible_term_core::snapshot::encode(&grid));
                match write_atomic(dir, &key, &bytes) {
                    Ok(()) if log => {
                        eprintln!("crucible-term: daemon: saved record {key} ({lines} rows)");
                    }
                    Ok(()) => {}
                    Err(e) => eprintln!("crucible-term: daemon: record {key}: {e}"),
                }
            }
            Job::Delete { key, lock } => {
                delete(dir, &key, lock);
                if log {
                    eprintln!("crucible-term: daemon: deleted record {key}");
                }
            }
            Job::Quit(done) => {
                let _ = done.send(());
                return;
            }
        }
    }
}

/// The records directory from an env lookup.
pub fn records_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    super::paths::sessions_dir(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cterm-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        ensure_dir(&d).unwrap();
        d
    }

    fn record_of(s: &Session, key: &str) -> Vec<u8> {
        let g = capture(s, 2000);
        let mut b = header(key, g.cols, g.rows, 0, 0, b"/tmp/a b").into_bytes();
        b.extend_from_slice(&crucible_term_core::snapshot::encode(&g));
        b
    }

    #[test]
    fn header_round_trips_and_bad_records_are_refused() {
        let mut s = Session::new(20, 4, 100);
        s.feed(b"hello\r\nworld");
        let b = record_of(&s, "k1");
        assert!(
            String::from_utf8_lossy(&b).starts_with(
                "cterm-session 1 key=k1 cols=20 rows=4 lines=0 saved=0 cwd=/tmp/a%20b\n"
            )
        );
        let r = parse(&b, "k1").unwrap();
        assert_eq!((r.cols, r.rows), (20, 4));
        assert!(parse(&b, "k2").is_err(), "key mismatch");
        let v2 = String::from_utf8_lossy(&b).replacen("session 1 ", "session 2 ", 1);
        assert!(
            parse(v2.as_bytes(), "k1")
                .unwrap_err()
                .contains("unknown version")
        );
        assert!(parse(b"garbage", "k1").is_err());
        assert!(parse(b"cterm-session 1 key=k1 cols=0 rows=4\n", "k1").is_err());
        let extra = b"cterm-session 1 future=x key=k1 cols=3 rows=2\nab";
        assert_eq!(parse(extra, "k1").unwrap().body, b"ab");
        assert!(valid_key("0123abcdef") && !valid_key("../x") && !valid_key(""));
    }

    /// E1: replayed at the saved geometry, then resized; the whole restored screen (cursor row
    /// included) sits in history above the separator, and the fresh line is below it.
    #[test]
    fn replay_at_a_different_geometry_keeps_everything_above_the_new_output() {
        let mut s = Session::new(30, 5, 1000);
        for i in 0..12 {
            s.feed(format!("line {i}\r\n").as_bytes());
        }
        s.feed(b"$ prompt");
        let rec = parse(&record_of(&s, "k"), "k").unwrap();
        let (mut r, n) = restored(&rec, 50, 8, 1000);
        assert_eq!(r.size(), (50, 8));
        let hist: Vec<String> = (0..r.history_len())
            .map(|i| r.history_row(i).text())
            .collect();
        let all = hist.join("\n");
        assert!(all.contains("line 0") && all.contains("line 11"), "{all}");
        assert!(hist.last().unwrap().contains("$ prompt"), "{hist:?}");
        assert!(n >= 13, "{n}");
        assert!(r.screen_row(0).text().contains("restored from record"));
        assert_eq!(r.cursor(), (0, 1));
        r.feed(b"new shell");
        assert_eq!(r.screen_row(1).text(), "new shell");
        // Smaller window than the record.
        let (r2, _) = restored(&rec, 10, 3, 1000);
        assert_eq!(r2.size(), (10, 3));
        assert!(r2.screen_row(0).text().starts_with("-- restore"));
    }

    /// Replay output never reaches the PTY: queries in a record get no reply.
    #[test]
    fn replay_discards_replies_and_events() {
        let rec = Record {
            cols: 10,
            rows: 3,
            body: b"\x1b[6n\x1b[c\x1b]52;c;aGk=\x07\x1b]0;t\x07x".to_vec(),
        };
        let (mut r, _) = restored(&rec, 10, 3, 100);
        assert!(r.take_replies().is_empty());
        assert!(r.take_events().is_empty());
    }

    #[test]
    fn writes_are_atomic_0600_and_symlinks_are_refused() {
        let d = tmpdir("write");
        assert_eq!(load(&d, "k").unwrap(), None);
        let mut s = Session::new(10, 2, 10);
        s.feed(b"hi");
        write_atomic(&d, "k", &record_of(&s, "k")).unwrap();
        let m = std::fs::metadata(d.join("k.vt")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(load(&d, "k").unwrap().is_some());
        std::os::unix::fs::symlink(d.join("k.vt"), d.join("l.vt")).unwrap();
        assert!(load(&d, "l").unwrap_err().contains("symlink"));
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert!(
            names
                .iter()
                .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_lock_is_exclusive_and_the_sweep_skips_locked_keys() {
        let d = tmpdir("sweep");
        let held = lock(&d, "a").unwrap();
        assert_eq!(lock(&d, "a").unwrap_err(), LockErr::Busy);
        let old = SystemTime::now() + Duration::from_secs(20 * 86_400);
        for k in ["a", "b", "c"] {
            std::fs::write(d.join(format!("{k}.vt")), b"x").unwrap();
        }
        std::fs::write(d.join(".b.vt.999999999.tmp"), b"x").unwrap();
        std::fs::write(d.join(format!(".c.vt.{}.tmp", std::process::id())), b"x").unwrap();
        let mut gone = sweep(&d, 14, old);
        gone.sort();
        assert_eq!(gone, [".b.vt.999999999.tmp", "b.vt", "c.vt"]);
        assert!(d.join("a.vt").exists(), "locked key kept");
        assert!(sweep(&d, 0, old).is_empty(), "days 0 never ages out");
        drop(held);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Capture cost of the default tail on a dense 10k-row 200x50 session (plan: 9-13 ms read
    /// plus 2-2.6 ms encode on host-a). `cargo test --release -- --ignored capture_cost --nocapture`.
    #[test]
    #[ignore = "measurement"]
    fn capture_cost() {
        let mut s = Session::new(200, 50, 10_000);
        let row: String = (0..140)
            .map(|i| format!("\x1b[3{}m{}", i % 8, char::from(b'a' + (i % 26) as u8)))
            .collect();
        for _ in 0..10_050 {
            s.feed(format!("{row}\x1b[0m\r\n").as_bytes());
        }
        for _ in 0..5 {
            let t = Instant::now();
            let g = capture(&s, 2000);
            let read = t.elapsed();
            let t = Instant::now();
            let b = crucible_term_core::snapshot::encode(&g);
            eprintln!(
                "capture 2000 rows: read {:?} encode {:?} bytes {}",
                read,
                t.elapsed(),
                b.len()
            );
        }
    }
}
