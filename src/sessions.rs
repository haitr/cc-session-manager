//! Claude Code session store: scan, parse, preview, trash, restore, purge.
//!
//! Trash layout: `<claude>/.session-cleaner-trash/<unix-secs>-<uuid>/{session.jsonl, metadata.json, artifacts/}`.
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A session written to this recently counts as in use.
const ACTIVE_SECS: u64 = 600;
const USER: &str = "\"type\":\"user\"";
const ASSISTANT: &str = "\"type\":\"assistant\"";
const TOOL_RESULT: &str = "\"toolUseResult\"";
const OWNER: &str = "\"type\":\"session-owner\"";
const TITLE_TAGS: [&str; 3] = ["\"type\":\"custom-title\"", "\"type\":\"ai-title\"", "\"type\":\"last-prompt\""];
/// Harness-injected text that is not something the user typed.
const INJECTED: [&str; 4] = ["<local-command", "<command-", "<system-reminder", "<task-notification"];

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub path:    PathBuf,
    pub uuid:    String,
    /// Working directory recorded in the transcript; the project dir name is a lossy encoding of it.
    pub cwd:     String,
    pub label:   String,
    /// Label is a user-chosen `/rename` title.
    pub starred: bool,
    /// Account that owns the session (last `session-owner` record); None when never recorded.
    pub owner:   Option<String>,
    pub size:    u64,
    pub mtime:   SystemTime,
}

impl Session {
    /// Protected from trashing: owned by a running Claude Code process, or written to recently.
    // ponytail: a stale <pid>.json left by a crashed process keeps its session protected until
    // that file is gone; check the pid is alive if that ever bites.
    pub fn active(&self, live: &HashSet<String>) -> bool {
        live.contains(&self.uuid) || self.mtime.elapsed().map(|d| d.as_secs() < ACTIVE_SECS).unwrap_or(true)
    }
}

pub struct TrashItem {
    pub dir:      PathBuf,
    pub original: PathBuf,
    pub trashed:  SystemTime,
    pub size:     u64,
}

pub enum ScanEvent {
    Total(usize),
    Parsed(Option<Session>),
    Done,
}

pub fn claude_home() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".claude")
}

fn trash_dir(root: &Path) -> PathBuf {
    root.join(".session-cleaner-trash")
}

fn read_dir(dir: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
}

/// Session ids owned by running Claude Code processes (`<claude>/sessions/<pid>.json`).
pub fn live_ids(root: &Path) -> HashSet<String> {
    read_dir(&root.join("sessions"))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| serde_json::from_slice::<Value>(&fs::read(p).ok()?).ok())
        .filter_map(|v| v["sessionId"].as_str().map(str::to_string))
        .collect()
}

/// Bump when `parse` changes what it extracts, so stale cached metadata is discarded.
const CACHE_VERSION: u64 = 2;

fn cache_path(root: &Path) -> PathBuf {
    root.join("ccsm-cache.json")
}

/// Session metadata from the last run, keyed by transcript path. Any problem reading it
/// (missing, corrupt, other version) just means an empty cache and a full scan.
fn load_cache(root: &Path) -> HashMap<PathBuf, Session> {
    let cached = fs::read(cache_path(root)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    match cached {
        Some((CACHE_VERSION, sessions)) => {
            Vec::<Session>::into_iter(sessions).map(|s| (s.path.clone(), s)).collect()
        }
        _ => HashMap::new(),
    }
}

/// Remember `sessions` for the next launch. Best effort: a failed write only costs that
/// launch a full scan. Written to a temp file first so a crash cannot leave half a cache.
pub fn save_cache(root: &Path, sessions: &[Session]) {
    let tmp = cache_path(root).with_extension("json.tmp");
    if serde_json::to_vec(&(CACHE_VERSION, sessions)).is_ok_and(|bytes| fs::write(&tmp, bytes).is_ok()) {
        let _ = fs::rename(&tmp, cache_path(root));
    }
}

/// Load every `projects/*/*.jsonl` on a worker thread, streaming results. A transcript whose
/// size and mtime match the cache is not opened at all; the rest are parsed in full.
// ponytail: one thread for the transcripts that do need parsing (first run, or after a
// version bump); fan out if that cold scan is too slow.
pub fn scan(root: PathBuf) -> Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let files: Vec<PathBuf> = read_dir(&root.join("projects"))
            .flat_map(|d| read_dir(&d))
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        let _ = tx.send(ScanEvent::Total(files.len()));
        let mut cache = load_cache(&root);
        for file in files {
            let unchanged = |s: &Session| {
                fs::metadata(&file).is_ok_and(|m| m.len() == s.size && m.modified().is_ok_and(|t| t == s.mtime))
            };
            let session = cache.remove(&file).filter(unchanged).or_else(|| parse(&file).ok());
            if tx.send(ScanEvent::Parsed(session)).is_err() {
                return;
            }
        }
        let _ = tx.send(ScanEvent::Done);
    });
    rx
}

fn for_each_line(mut reader: impl BufRead, mut f: impl FnMut(&str)) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        f(&String::from_utf8_lossy(&buf));
    }
}

fn flatten_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Typed text of a user/assistant record, line breaks kept; None for tool traffic and
/// harness-injected messages.
fn message_text(v: &Value) -> Option<String> {
    if v["isMeta"] == true || v["isSidechain"] == true {
        return None;
    }
    let parts: Vec<&str> = match &v["message"]["content"] {
        Value::String(s) => vec![s],
        Value::Array(a) => a.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect(),
        _ => return None,
    };
    let typed: Vec<&str> =
        parts.into_iter().filter(|t| !INJECTED.iter().any(|tag| t.trim_start().starts_with(tag))).collect();
    let text = typed.join("
").trim().to_string();
    (!text.is_empty()).then_some(text)
}

pub fn parse(path: &Path) -> io::Result<Session> {
    let meta = fs::metadata(path)?;
    let (mut cwd, mut custom, mut ai, mut prompt, mut user) = (None, None, None, None, None);
    let mut owner = None;
    let set = |slot: &mut Option<String>, v: &Value| {
        if let Some(s) = v.as_str().filter(|s| !s.trim().is_empty()) {
            *slot = Some(flatten_ws(s));
        }
    };
    for_each_line(BufReader::new(File::open(path)?), |line| {
        // Transcripts are mostly bulky tool/API records; only JSON-parse lines that can matter.
        let wanted = (cwd.is_none() && line.contains("\"cwd\":"))
            || TITLE_TAGS.iter().any(|t| line.contains(t))
            || line.contains(OWNER)
            || (line.contains(USER) && !line.contains(TOOL_RESULT));
        if !wanted {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if cwd.is_none()
            && let Some(dir) = v["cwd"].as_str()
        {
            cwd = Some(dir.to_string());
        }
        match v["type"].as_str() {
            Some("custom-title") => set(&mut custom, &v["customTitle"]),
            Some("ai-title") => set(&mut ai, &v["aiTitle"]),
            Some("last-prompt") => set(&mut prompt, &v["lastPrompt"]),
            Some("session-owner") => set(&mut owner, &v["accountUuid"]),
            Some("user") => {
                if let Some(text) = message_text(&v) {
                    user = Some(flatten_ws(&text));
                }
            }
            _ => {}
        }
    })?;
    let project = path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy().into_owned();
    let starred = custom.is_some();
    let label = custom.or(ai).or(prompt).or(user).unwrap_or_else(|| "(empty)".into());
    Ok(Session {
        path: path.to_path_buf(),
        uuid: path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
        cwd: cwd.unwrap_or(project),
        label: label.chars().take(200).collect(),
        starred,
        owner,
        size: meta.len(),
        mtime: meta.modified()?,
    })
}

/// Typed messages as `(is_user, text)`, oldest first: the last `limit` of them, read from
/// the final `tail` bytes only when given. `flat` collapses each message onto one capped line.
fn messages(path: &Path, tail: Option<u64>, limit: usize, flat: Option<usize>) -> Vec<(bool, String)> {
    let Ok(mut file) = File::open(path) else { return Vec::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut partial = tail.is_some_and(|t| len > t && file.seek(SeekFrom::Start(len - t)).is_ok());
    let mut out = VecDeque::new();
    let _ = for_each_line(BufReader::new(file), |line| {
        if std::mem::take(&mut partial) {
            return; // first line after a seek starts mid-record
        }
        if !(line.contains(ASSISTANT) || (line.contains(USER) && !line.contains(TOOL_RESULT))) {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let is_user = v["type"] == "user";
        if !is_user && v["type"] != "assistant" {
            return;
        }
        if let Some(text) = message_text(&v) {
            if out.len() == limit {
                out.pop_front();
            }
            let text = match flat {
                Some(max_chars) => flatten_ws(&text).chars().take(max_chars).collect(),
                None => text.replace('\t', "    "),
            };
            out.push_back((is_user, text));
        }
    });
    out.into()
}

/// Last `n` typed messages for the side pane.
// ponytail: only the final 512 KB is read, so cursor movement stays instant on huge
// transcripts; the detail view (`transcript`) reads everything.
pub fn preview(path: &Path, n: usize) -> Vec<(bool, String)> {
    messages(path, Some(512 * 1024), n, Some(600))
}

/// Every typed message, for the detail view.
// ponytail: synchronous full read when the view opens; hand it to a worker if huge
// transcripts make that stall.
pub fn transcript(path: &Path) -> Vec<(bool, String)> {
    messages(path, None, usize::MAX, None)
}

/// The signed-in account as `(accountUuid, organizationUuid)`, from `.claude.json` inside the
/// Claude home or next to it.
pub fn current_account(root: &Path) -> Option<(String, String)> {
    [Some(root), root.parent()].into_iter().flatten().find_map(|dir| {
        let config: Value = serde_json::from_slice(&fs::read(dir.join(".claude.json")).ok()?).ok()?;
        let account = &config["oauthAccount"];
        Some((account["accountUuid"].as_str()?.to_string(), account["organizationUuid"].as_str()?.to_string()))
    })
}

/// Point every `session-owner` record at `account`/`org`; returns how many were rewritten.
/// All other lines are copied byte-for-byte into a temp file that then replaces the original,
/// so the transcript is never truncated in place. The mtime is kept, so the session neither
/// jumps to the top of the list nor starts counting as recently active.
pub fn set_owner(root: &Path, s: &Session, account: &str, org: &str) -> Result<usize> {
    if s.active(&live_ids(root)) {
        bail!("{} is in use by Claude Code", s.uuid);
    }
    let tmp = s.path.with_extension("jsonl.ccsm-tmp");
    let result = (|| -> Result<usize> {
        let mut reader = BufReader::new(File::open(&s.path)?);
        let mut out = BufWriter::new(File::create(&tmp)?);
        let (mut buf, mut changed) = (Vec::new(), 0);
        while reader.read_until(b'\n', &mut buf)? != 0 {
            let record = std::str::from_utf8(&buf)
                .ok()
                .filter(|line| line.contains(OWNER))
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|v| v["type"] == "session-owner");
            match record {
                Some(mut v) => {
                    v["accountUuid"] = account.into();
                    v["organizationUuid"] = org.into();
                    serde_json::to_writer(&mut out, &v)?;
                    if buf.ends_with(b"\n") {
                        out.write_all(b"\n")?;
                    }
                    changed += 1;
                }
                None => out.write_all(&buf)?,
            }
            buf.clear();
        }
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        file.set_modified(s.mtime)?;
        drop(file);
        if changed > 0 {
            fs::rename(&tmp, &s.path).context("replacing transcript")?;
        }
        Ok(changed)
    })();
    if !matches!(result, Ok(n) if n > 0) {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Move a session (and its sibling artifacts dir) into the trash.
// ponytail: plain rename, so the trash must sit on the same volume as the store (it does:
// both live under the Claude home). A copy+delete fallback is where data loss would hide.
pub fn trash(root: &Path, s: &Session) -> Result<()> {
    if s.active(&live_ids(root)) {
        bail!("{} is in use by Claude Code", s.uuid);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let dir = trash_dir(root).join(format!("{now}-{}", s.uuid));
    fs::create_dir_all(trash_dir(root))?;
    fs::create_dir(&dir)?; // must be new: never rename over an existing trash item
    let moved = dir.join("session.jsonl");
    let artifacts = s.path.with_extension("");
    let result = (|| -> Result<()> {
        let meta = json!({ "original_path": s.path.to_string_lossy() });
        fs::write(dir.join("metadata.json"), serde_json::to_vec_pretty(&meta)?)?;
        fs::rename(&s.path, &moved).context("moving transcript")?;
        if artifacts.is_dir() {
            fs::rename(&artifacts, dir.join("artifacts")).context("moving artifacts")?;
        }
        Ok(())
    })();
    if result.is_err() {
        // Put the transcript back rather than leave a half-trashed session.
        if moved.exists() {
            fs::rename(&moved, &s.path).context("rolling back transcript")?;
        }
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

pub fn list_trash(root: &Path) -> Vec<TrashItem> {
    let mut items: Vec<TrashItem> = read_dir(&trash_dir(root))
        .filter_map(|dir| {
            let meta: Value = serde_json::from_slice(&fs::read(dir.join("metadata.json")).ok()?).ok()?;
            let secs = dir.file_name()?.to_str()?.split('-').next()?.parse().unwrap_or(0);
            Some(TrashItem {
                original: PathBuf::from(meta["original_path"].as_str()?),
                trashed: UNIX_EPOCH + Duration::from_secs(secs),
                size: fs::metadata(dir.join("session.jsonl")).ok()?.len(),
                dir,
            })
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.trashed));
    items
}

/// Put a trashed session back at its original path; never overwrites.
pub fn restore(item: &TrashItem) -> Result<()> {
    let artifacts = item.original.with_extension("");
    if item.original.exists() || artifacts.exists() {
        bail!("{} already exists", item.original.display());
    }
    if let Some(parent) = item.original.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(item.dir.join("session.jsonl"), &item.original)?;
    if item.dir.join("artifacts").is_dir() {
        fs::rename(item.dir.join("artifacts"), artifacts)?;
    }
    fs::remove_dir_all(&item.dir)?;
    Ok(())
}

pub fn purge(item: &TrashItem) -> io::Result<()> {
    fs::remove_dir_all(&item.dir)
}

pub fn age_secs(t: SystemTime) -> u64 {
    t.elapsed().map(|d| d.as_secs()).unwrap_or(0)
}

pub fn age(t: SystemTime) -> String {
    match age_secs(t) {
        s if s < 60 => "now".into(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    const UUID: &str = "11111111-2222-3333-4444-555555555555";
    pub(crate) const OLD: Duration = Duration::from_secs(3_600);
    pub(crate) const USER_LINE: &str = r#"{"type":"user","cwd":"D:\\proj","message":{"content":"first  prompt\nhere"}}"#;

    /// Fresh fake Claude home holding one session last written `age` ago.
    pub(crate) fn fixture(name: &str, lines: &[&str], age: Duration) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("ccsm-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("projects").join("D--proj");
        fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{UUID}.jsonl"));
        fs::write(&path, lines.join("\n")).unwrap();
        backdate(&path, age);
        (root, path)
    }

    pub(crate) fn backdate(path: &Path, age: Duration) {
        touch(path, SystemTime::now() - age);
    }

    fn touch(path: &Path, mtime: SystemTime) {
        File::options().write(true).open(path).unwrap().set_modified(mtime).unwrap();
    }

    fn scan_all(root: &Path) -> Vec<Session> {
        scan(root.to_path_buf())
            .into_iter()
            .filter_map(|event| match event {
                ScanEvent::Parsed(session) => session,
                _ => None,
            })
            .collect()
    }

    #[test]
    fn scan_cache() {
        let (root, path) = fixture("cache", &[OWNER_LINE, USER_LINE], OLD);
        let cold = scan_all(&root);
        assert_eq!((cold.len(), cold[0].label.as_str()), (1, "first prompt here"));
        save_cache(&root, &cold);
        let (size, mtime) = (cold[0].size as usize, cold[0].mtime);
        let label = |root: &Path| scan_all(root)[0].label.clone();

        // Same size and mtime but junk content: answered from the cache, so the file was not read.
        fs::write(&path, "x".repeat(size)).unwrap();
        touch(&path, mtime);
        let warm = scan_all(&root);
        assert_eq!((warm[0].label.as_str(), warm[0].owner.as_deref()), ("first prompt here", Some("someone-else")));
        assert_eq!((warm[0].uuid.as_str(), warm[0].cwd.as_str(), warm[0].mtime), (UUID, "D:\\proj", mtime));

        // A different size, or a different mtime alone, is re-parsed.
        fs::write(&path, USER_LINE.replace("first", "changed")).unwrap();
        touch(&path, mtime);
        assert_eq!(label(&root), "changed prompt here");
        fs::write(&path, "x".repeat(size)).unwrap();
        touch(&path, mtime + Duration::from_secs(1));
        assert_eq!(label(&root), "(empty)");

        // Another cache version, or garbage, is ignored (junk content gets parsed).
        touch(&path, mtime);
        assert_eq!(label(&root), "first prompt here");
        let saved = fs::read_to_string(cache_path(&root)).unwrap();
        assert!(saved.starts_with("[2,"));
        fs::write(cache_path(&root), saved.replacen("[2,", "[999,", 1)).unwrap();
        assert_eq!(label(&root), "(empty)");
        fs::write(cache_path(&root), "not json").unwrap();
        assert_eq!(label(&root), "(empty)");
    }

    pub(crate) const OWNER_LINE: &str =
        r#"{"type":"session-owner","accountUuid":"someone-else","organizationUuid":"their-org","sessionId":"x"}"#;
    pub(crate) const ACCOUNT_JSON: &str = r#"{"oauthAccount":{"accountUuid":"me","organizationUuid":"my-org"}}"#;

    #[test]
    fn ownership() {
        let (root, path) = fixture("owner", &[OWNER_LINE, USER_LINE, OWNER_LINE], OLD);
        assert_eq!(current_account(&root), None);
        fs::write(root.join(".claude.json"), ACCOUNT_JSON).unwrap();
        let (account, org) = current_account(&root).unwrap();
        let before = parse(&path).unwrap();
        assert_eq!(before.owner.as_deref(), Some("someone-else"));

        assert_eq!(set_owner(&root, &before, &account, &org).unwrap(), 2);
        let after = parse(&path).unwrap();
        assert_eq!(after.owner.as_deref(), Some("me"));
        assert_eq!(after.mtime, before.mtime);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(text.lines().nth(1), Some(USER_LINE)); // untouched lines stay byte-identical
        assert!(text.contains("my-org") && !text.contains("someone-else") && !text.contains("their-org"));
        assert!(!text.ends_with('\n')); // no newline invented at EOF
        assert!(!path.with_extension("jsonl.ccsm-tmp").exists());

        // Nothing to rewrite: file untouched, no temp left behind.
        let (root, path) = fixture("owner-none", &[USER_LINE], OLD);
        let s = parse(&path).unwrap();
        assert_eq!((s.owner.clone(), set_owner(&root, &s, "me", "my-org").unwrap()), (None, 0));
        assert_eq!(fs::read_to_string(&path).unwrap(), USER_LINE);
        assert!(!path.with_extension("jsonl.ccsm-tmp").exists());

        // In-use sessions are never rewritten.
        let (root, path) = fixture("owner-active", &[OWNER_LINE], Duration::ZERO);
        assert!(set_owner(&root, &parse(&path).unwrap(), "me", "my-org").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), OWNER_LINE);
    }

    #[test]
    fn label_priority_and_cwd() {
        let tool = r#"{"type":"user","toolUseResult":{},"message":{"content":[{"type":"tool_result","content":"x"}]}}"#;
        let injected =
            r#"{"type":"user","message":{"content":[{"type":"text","text":"<system-reminder>x</system-reminder>"}]}}"#;
        let (_, path) = fixture("label-user", &[USER_LINE, tool, injected], OLD);
        let s = parse(&path).unwrap();
        assert_eq!((s.label.as_str(), s.cwd.as_str(), s.starred), ("first prompt here", "D:\\proj", false));
        assert_eq!(s.uuid, UUID);

        let prompt = r#"{"type":"last-prompt","lastPrompt":"the prompt"}"#;
        let (_, path) = fixture("label-prompt", &[USER_LINE, prompt], OLD);
        assert_eq!(parse(&path).unwrap().label, "the prompt");

        let ai = r#"{"type":"ai-title","aiTitle":"ai one"}"#;
        let ai2 = r#"{"type":"ai-title","aiTitle":"ai two"}"#;
        let (_, path) = fixture("label-ai", &[USER_LINE, ai, prompt, ai2], OLD);
        assert_eq!(parse(&path).unwrap().label, "ai two"); // last occurrence wins

        let custom = r#"{"type":"custom-title","customTitle":"mine"}"#;
        let (_, path) = fixture("label-custom", &[custom, USER_LINE, ai, prompt], OLD);
        let s = parse(&path).unwrap();
        assert_eq!((s.label.as_str(), s.starred), ("mine", true));
    }

    #[test]
    fn preview_keeps_last_typed_messages() {
        let asst = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"answer"}]}}"#;
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"x"}]}}"#;
        let (_, path) = fixture("preview", &[USER_LINE, asst, tool, USER_LINE, asst], OLD);
        let got = preview(&path, 2);
        assert_eq!(got, vec![(true, "first prompt here".to_string()), (false, "answer".to_string())]);
        let full = transcript(&path);
        assert_eq!((full.len(), full[0].1.as_str()), (4, "first  prompt
here")); // line breaks kept
    }

    #[test]
    fn trash_restore_round_trip() {
        let (root, path) = fixture("roundtrip", &[USER_LINE], OLD);
        let artifacts = path.with_extension("");
        fs::create_dir_all(artifacts.join("subagents")).unwrap();
        fs::write(artifacts.join("subagents").join("a.jsonl"), "{}").unwrap();

        trash(&root, &parse(&path).unwrap()).unwrap();
        assert!(!path.exists() && !artifacts.exists());
        let items = list_trash(&root);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].original, path);
        assert!(items[0].dir.starts_with(root.join(".session-cleaner-trash")));
        assert!(age_secs(items[0].trashed) < 60);
        assert!(items[0].dir.join("artifacts").join("subagents").join("a.jsonl").exists());

        // Restore refuses to overwrite, and leaves the trash item intact.
        fs::write(&path, "new").unwrap();
        assert!(restore(&items[0]).is_err());
        assert!(items[0].dir.join("session.jsonl").exists());
        fs::remove_file(&path).unwrap();

        restore(&items[0]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), USER_LINE);
        assert!(artifacts.join("subagents").join("a.jsonl").exists());
        assert!(list_trash(&root).is_empty());

        trash(&root, &parse(&path).unwrap()).unwrap();
        purge(&list_trash(&root)[0]).unwrap();
        assert!(list_trash(&root).is_empty() && !path.exists());
    }

    #[test]
    fn active_sessions_are_not_trashed() {
        let (root, path) = fixture("recent", &[USER_LINE], Duration::ZERO);
        assert!(trash(&root, &parse(&path).unwrap()).is_err());
        assert!(path.exists());

        // Old on disk, but owned by a running process.
        let (root, path) = fixture("live", &[USER_LINE], OLD);
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(root.join("sessions").join("42.json"), format!(r#"{{"pid":42,"sessionId":"{UUID}"}}"#)).unwrap();
        assert!(trash(&root, &parse(&path).unwrap()).is_err());
        assert!(path.exists() && list_trash(&root).is_empty());
    }
}
