//! The vault: one Keychain item per login (not per surface), service `agent-switch`, account
//! `<id>:<store>` (`store` is `claude-oauth` or `openai-auth`). Refresh tokens rotate, so one item per
//! login keeps copies from drifting. Nothing is ever written in plaintext.
//!
//! An item holds a small JSON envelope: the login blob embedded as raw JSON, a lineage fingerprint
//! (a hash of the refresh token, never the token) and the last-refresh time.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use crate::config::validate_account_id;
use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::locks::AccountLock;
use crate::paths::Paths;

pub const SERVICE: &str = "agent-switch";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The login as JSON text, restricted to printable ASCII.
    pub blob: String,
    /// Identifies the refresh-token chain this blob belongs to.
    pub fingerprint: String,
    /// Unix seconds of the last refresh (or capture).
    pub refreshed_at: u64,
}

#[derive(Serialize, Deserialize)]
struct Envelope<'a> {
    v: u8,
    fp: String,
    t: u64,
    #[serde(borrow)]
    blob: &'a RawValue,
}

/// First 16 hex chars of SHA-256 over the refresh token. Safe to store in `state.json` and logs.
pub fn lineage_fingerprint(refresh_token: &str) -> String {
    let digest = Sha256::digest(refresh_token.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Make a JSON blob storable: valid JSON, printable ASCII only. Control characters (pretty-printing)
/// are removed by re-serializing; non-ASCII characters become `\uXXXX`, which is the same JSON value.
pub fn normalize_blob(blob: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(blob).map_err(|e| Error::Invalid(format!("login blob is not valid JSON: {e}")))?;
    let compact = if blob.chars().any(|c| c.is_control()) {
        serde_json::to_string(&value).map_err(|e| Error::Invalid(e.to_string()))?
    } else {
        blob.to_string()
    };
    let mut out = String::with_capacity(compact.len());
    let mut units = [0u16; 2];
    for c in compact.chars() {
        if (' '..='~').contains(&c) {
            out.push(c);
        } else {
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    Ok(out)
}

pub struct Vault {
    keychain: Arc<dyn Keychain>,
}

/// Longest part value. The `security -i` line limit is 4090 bytes including the command and the account name.
const PART_LEN: usize = 3000;

/// Head item of a value that did not fit one Keychain item: the envelope text is split over `n` part items
/// named `<account>#<id>.<k>`, where `id` is a hash of the whole text.
#[derive(Serialize, Deserialize)]
struct Manifest {
    v: u8,
    n: usize,
    len: usize,
    id: String,
}

fn parse_manifest(head: &str) -> Option<Manifest> {
    serde_json::from_str::<Manifest>(head).ok().filter(|m| m.v == 2)
}

fn part_name(name: &str, id: &str, k: usize) -> String {
    format!("{name}#{id}.{k}")
}

fn account_name(id: &str, store: &str) -> Result<String> {
    validate_account_id(id)?;
    if !store.chars().all(|c| c.is_ascii_lowercase() || c == '-') || store.is_empty() {
        return Err(Error::Invalid(format!("invalid vault store `{store}`")));
    }
    Ok(format!("{id}:{store}"))
}

impl Vault {
    pub fn new(keychain: Arc<dyn Keychain>) -> Self {
        Vault { keychain }
    }

    pub fn backend(&self) -> &'static str {
        self.keychain.name()
    }

    pub fn get(&self, id: &str, store: &str) -> Result<Option<Slot>> {
        let name = account_name(id, store)?;
        // A concurrent `put` of a chunked value swaps the head and then removes the old parts, so a reader
        // that fetched the old head can find its parts gone. Read the head again once before giving up.
        for _ in 0..2 {
            let Some(head) = self.keychain.get(SERVICE, &name)? else {
                return Ok(None);
            };
            let text = match parse_manifest(&head) {
                Some(m) => match self.read_parts(&name, &m) {
                    Ok(text) => text,
                    Err(e) => {
                        if self.keychain.get(SERVICE, &name)?.as_deref() == Some(head.as_str()) {
                            return Err(e);
                        }
                        continue;
                    }
                },
                None => head,
            };
            let env: Envelope = serde_json::from_str(&text)
                .map_err(|_| Error::Keychain(format!("vault item {name} is not a valid agent-switch envelope")))?;
            return Ok(Some(Slot { blob: env.blob.get().to_string(), fingerprint: env.fp, refreshed_at: env.t }));
        }
        Err(Error::Keychain(format!("vault item {name} was being rewritten while it was read; try again")))
    }

    pub fn put(&self, id: &str, store: &str, slot: &Slot) -> Result<()> {
        let name = account_name(id, store)?;
        let blob = normalize_blob(&slot.blob)?;
        let raw = RawValue::from_string(blob).map_err(|e| Error::Invalid(e.to_string()))?;
        let env = Envelope { v: 1, fp: slot.fingerprint.clone(), t: slot.refreshed_at, blob: &raw };
        let text = serde_json::to_string(&env).map_err(|e| Error::Invalid(e.to_string()))?;
        let old = self.keychain.get(SERVICE, &name)?.and_then(|h| parse_manifest(&h));
        let new_id = match self.keychain.set(SERVICE, &name, &text) {
            Ok(()) => None,
            // `/usr/bin/security` refuses a value over its stdin limit before running anything, so nothing
            // was written. Keep one item per login by splitting it over part items and a manifest.
            Err(Error::TooLarge { .. }) => Some(self.write_parts(&name, &text, old.as_ref().map(|o| o.id.as_str()))?),
            Err(e) => return Err(e),
        };
        // The new head is committed; parts of the previous chunked value are now unreachable.
        if let Some(old) = old.filter(|o| new_id.as_deref() != Some(o.id.as_str())) {
            self.delete_parts(&name, &old);
        }
        Ok(())
    }

    pub fn delete(&self, id: &str, store: &str) -> Result<bool> {
        let name = account_name(id, store)?;
        let old = self.keychain.get(SERVICE, &name)?.and_then(|h| parse_manifest(&h));
        let existed = self.keychain.delete(SERVICE, &name)?;
        if let Some(old) = old {
            self.delete_parts(&name, &old);
        }
        Ok(existed)
    }

    /// Parts first, manifest last: a crash in between leaves the previous complete value readable. When a
    /// write fails part-way, the parts written so far are removed, unless they belong to the value that is
    /// still live (`live_id`).
    fn write_parts(&self, name: &str, text: &str, live_id: Option<&str>) -> Result<String> {
        let id = lineage_fingerprint(text);
        let pieces: Vec<&str> = text.as_bytes().chunks(PART_LEN).map(|c| std::str::from_utf8(c).expect("vault text is ASCII")).collect();
        let manifest = Manifest { v: 2, n: pieces.len(), len: text.len(), id: id.clone() };
        let written = (|| -> Result<()> {
            for (k, piece) in pieces.iter().enumerate() {
                self.keychain.set(SERVICE, &part_name(name, &id, k), piece)?;
            }
            let head = serde_json::to_string(&manifest).map_err(|e| Error::Invalid(e.to_string()))?;
            self.keychain.set(SERVICE, name, &head)
        })();
        if let Err(e) = written {
            if live_id != Some(id.as_str()) {
                self.delete_parts(name, &manifest);
            }
            return Err(e);
        }
        Ok(manifest.id)
    }

    fn read_parts(&self, name: &str, m: &Manifest) -> Result<String> {
        let mut text = String::with_capacity(m.len);
        for k in 0..m.n {
            let piece = self
                .keychain
                .get(SERVICE, &part_name(name, &m.id, k))?
                .ok_or_else(|| Error::Keychain(format!("vault item {name} is incomplete (part {k} of {} is missing)", m.n)))?;
            text.push_str(&piece);
        }
        if text.len() != m.len || lineage_fingerprint(&text) != m.id {
            return Err(Error::Keychain(format!("vault item {name} failed its integrity check")));
        }
        Ok(text)
    }

    /// Best effort: an orphaned part is only wasted space, never a wrong value.
    fn delete_parts(&self, name: &str, old: &Manifest) {
        for k in 0..old.n {
            let _ = self.keychain.delete(SERVICE, &part_name(name, &old.id, k));
        }
    }

    pub fn exists(&self, id: &str, store: &str) -> Result<bool> {
        Ok(self.get(id, store)?.is_some())
    }
}

/// Refresh one login at most once across processes: take the account lock, re-read the slot, and refresh
/// only if it is still stale (`is_fresh` false). A second process waiting on the lock finds the first
/// one's result and sends no request. Returns the slot and whether this call refreshed it.
pub fn refresh_if_stale(
    paths: &Paths,
    vault: &Vault,
    id: &str,
    store: &str,
    lock_timeout: Duration,
    is_fresh: impl Fn(&Slot) -> bool,
    refresh: impl FnOnce(&Slot) -> Result<Slot>,
) -> Result<(Slot, bool)> {
    let _lock = AccountLock::acquire(paths, id, lock_timeout)?;
    let current = vault.get(id, store)?.ok_or_else(|| Error::NotFound(format!("vault slot {id}:{store}")))?;
    if is_fresh(&current) {
        return Ok((current, false));
    }
    let next = refresh(&current)?;
    vault.put(id, store, &next)?;
    Ok((next, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn vault() -> (Vault, Arc<MemoryKeychain>) {
        let kc = Arc::new(MemoryKeychain::new());
        (Vault::new(kc.clone()), kc)
    }

    fn slot(blob: &str, t: u64) -> Slot {
        Slot { blob: blob.into(), fingerprint: lineage_fingerprint("rt-1"), refreshed_at: t }
    }

    #[test]
    fn put_get_delete_round_trip() {
        let (v, kc) = vault();
        let s = slot(r#"{"claudeAiOauth":{"refreshToken":"rt-1"}}"#, 7);
        assert!(v.get("a", "claude-oauth").unwrap().is_none());
        v.put("a", "claude-oauth", &s).unwrap();
        assert_eq!(v.get("a", "claude-oauth").unwrap().unwrap(), s);
        assert!(kc.get(SERVICE, "a:claude-oauth").unwrap().is_some());
        assert!(v.delete("a", "claude-oauth").unwrap());
        assert!(!v.delete("a", "claude-oauth").unwrap());
        assert!(kc.is_empty());
    }

    #[test]
    fn holds_an_8kb_blob() {
        let (v, _) = vault();
        let s = slot(&format!(r#"{{"k":"{}"}}"#, "x".repeat(8192)), 1);
        v.put("big", "openai-auth", &s).unwrap();
        assert_eq!(v.get("big", "openai-auth").unwrap().unwrap().blob.len(), s.blob.len());
    }

    #[test]
    fn slots_are_per_login_and_per_store() {
        let (v, _) = vault();
        v.put("a", "claude-oauth", &slot("{}", 1)).unwrap();
        assert!(v.get("a", "openai-auth").unwrap().is_none());
        assert!(v.get("b", "claude-oauth").unwrap().is_none());
    }

    #[test]
    fn rejects_bad_ids_stores_and_blobs() {
        let (v, _) = vault();
        assert!(v.get("../x", "claude-oauth").is_err());
        assert!(v.get("a", "bad store").is_err());
        assert!(v.put("a", "claude-oauth", &slot("not json", 1)).is_err());
    }

    #[test]
    fn corrupt_envelope_is_an_error_not_a_miss() {
        let (v, kc) = vault();
        kc.set(SERVICE, "a:claude-oauth", "garbage").unwrap();
        assert!(v.get("a", "claude-oauth").is_err());
    }

    #[test]
    fn normalizes_pretty_and_non_ascii_json() {
        let n = normalize_blob("{\n  \"name\": \"caf\u{e9} \u{1f600}\"\n}").unwrap();
        assert!(n.bytes().all(|b| (0x20..=0x7e).contains(&b)), "{n}");
        let v: serde_json::Value = serde_json::from_str(&n).unwrap();
        assert_eq!(v["name"], "caf\u{e9} \u{1f600}");
    }

    #[test]
    fn fingerprint_is_stable_and_not_the_token() {
        let f = lineage_fingerprint("rt-1");
        assert_eq!(f, lineage_fingerprint("rt-1"));
        assert_ne!(f, lineage_fingerprint("rt-2"));
        assert_eq!(f.len(), 16);
        assert!(!f.contains("rt-1"));
    }

    #[test]
    fn two_threads_refreshing_one_account_send_one_request() {
        let home = tempfile::tempdir().unwrap();
        let paths = Arc::new(Paths::from_home(home.path()));
        let kc = Arc::new(MemoryKeychain::new());
        let v = Arc::new(Vault::new(kc));
        v.put("acme", "claude-oauth", &slot(r#"{"expiresAt":0}"#, 0)).unwrap();
        let endpoint_hits = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (paths, v, hits) = (paths.clone(), v.clone(), endpoint_hits.clone());
                std::thread::spawn(move || {
                    refresh_if_stale(
                        &paths,
                        &v,
                        "acme",
                        "claude-oauth",
                        Duration::from_secs(5),
                        |s| s.refreshed_at > 0,
                        |_old| {
                            hits.fetch_add(1, Ordering::SeqCst); // the fake token endpoint
                            std::thread::sleep(Duration::from_millis(100));
                            Ok(slot(r#"{"expiresAt":9}"#, 100))
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(endpoint_hits.load(Ordering::SeqCst), 1);
        assert_eq!(results.iter().filter(|(_, refreshed)| *refreshed).count(), 1);
        assert!(results.iter().all(|(s, _)| s.refreshed_at == 100));
    }

    #[test]
    fn a_failed_refresh_leaves_the_slot_untouched() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(home.path());
        let (v, _) = vault();
        let before = slot(r#"{"a":1}"#, 0);
        v.put("acme", "claude-oauth", &before).unwrap();
        let r = refresh_if_stale(&paths, &v, "acme", "claude-oauth", Duration::from_secs(1), |_| false, |_| {
            Err(Error::Invalid("endpoint down".into()))
        });
        assert!(r.is_err());
        assert_eq!(v.get("acme", "claude-oauth").unwrap().unwrap(), before);
    }

    /// A backend with the `security -i` size limit, and an optional account whose writes fail.
    struct Limited {
        inner: MemoryKeychain,
        max: usize,
        fail_on: std::sync::Mutex<Option<String>>,
    }

    impl Limited {
        fn new(max: usize) -> Arc<Limited> {
            Arc::new(Limited { inner: MemoryKeychain::new(), max, fail_on: std::sync::Mutex::new(None) })
        }
    }

    impl Keychain for Limited {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
            self.inner.get(service, account)
        }
        fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
            if value.len() > self.max {
                return Err(Error::TooLarge { len: value.len(), limit: self.max });
            }
            if self.fail_on.lock().unwrap().as_deref() == Some(account) {
                return Err(Error::Keychain("denied".into()));
            }
            self.inner.set(service, account, value)
        }
        fn delete(&self, service: &str, account: &str) -> Result<bool> {
            self.inner.delete(service, account)
        }
        fn name(&self) -> &'static str {
            "limited"
        }
    }

    fn big(fill: char, n: usize) -> Slot {
        slot(&format!(r#"{{"k":"{}"}}"#, fill.to_string().repeat(n)), 5)
    }

    #[test]
    fn a_login_over_the_backend_limit_is_split_and_read_back_whole() {
        let kc = Limited::new(4000);
        let v = Vault::new(kc.clone());
        let s = big('a', 7000);
        v.put("big", "openai-auth", &s).unwrap();
        assert_eq!(v.get("big", "openai-auth").unwrap().unwrap(), s);
        assert!(kc.inner.len() >= 4, "manifest plus at least three parts, got {}", kc.inner.len());
    }

    #[test]
    fn rewriting_removes_the_parts_it_replaces() {
        let kc = Limited::new(4000);
        let v = Vault::new(kc.clone());
        v.put("big", "openai-auth", &big('a', 7000)).unwrap();
        v.put("big", "openai-auth", &big('b', 7000)).unwrap();
        let n_two = kc.inner.len();
        v.put("big", "openai-auth", &big('c', 7000)).unwrap();
        assert_eq!(kc.inner.len(), n_two, "old parts must not pile up");
        v.put("big", "openai-auth", &slot("{}", 1)).unwrap();
        assert_eq!(kc.inner.len(), 1);
        assert_eq!(v.get("big", "openai-auth").unwrap().unwrap().blob, "{}");
    }

    #[test]
    fn delete_removes_every_part() {
        let kc = Limited::new(4000);
        let v = Vault::new(kc.clone());
        v.put("big", "openai-auth", &big('a', 9000)).unwrap();
        assert!(v.delete("big", "openai-auth").unwrap());
        assert!(kc.inner.is_empty());
        assert!(!v.delete("big", "openai-auth").unwrap());
    }

    #[test]
    fn a_failed_write_keeps_the_previous_value_readable() {
        let kc = Limited::new(4000);
        let v = Vault::new(kc.clone());
        let before = big('a', 7000);
        v.put("big", "openai-auth", &before).unwrap();
        let items_before = kc.inner.len();
        *kc.fail_on.lock().unwrap() = Some("big:openai-auth".into()); // the manifest write fails last
        assert!(v.put("big", "openai-auth", &big('b', 7000)).is_err());
        assert_eq!(v.get("big", "openai-auth").unwrap().unwrap(), before);
        assert_eq!(kc.inner.len(), items_before, "the parts of the failed write are removed");
    }

    #[test]
    fn a_reader_racing_a_rewrite_retries_instead_of_reporting_corruption() {
        use std::sync::Mutex;
        /// Runs `hook` once, right before the first part is read: a writer replaces the value there.
        struct Racing {
            inner: Arc<Limited>,
            hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        }
        impl Keychain for Racing {
            fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
                if account.contains('#') {
                    if let Some(h) = self.hook.lock().unwrap().take() {
                        h();
                    }
                }
                self.inner.get(service, account)
            }
            fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
                self.inner.set(service, account, value)
            }
            fn delete(&self, service: &str, account: &str) -> Result<bool> {
                self.inner.delete(service, account)
            }
            fn name(&self) -> &'static str {
                "racing"
            }
        }
        let kc = Limited::new(4000);
        let writer = Vault::new(kc.clone());
        writer.put("big", "openai-auth", &big('a', 7000)).unwrap();
        let after = big('b', 7000);
        let racing = Arc::new(Racing {
            inner: kc.clone(),
            hook: Mutex::new(Some(Box::new({
                let after = after.clone();
                move || Vault::new(kc.clone()).put("big", "openai-auth", &after).unwrap()
            }))),
        });
        let reader = Vault::new(racing);
        assert_eq!(reader.get("big", "openai-auth").unwrap().unwrap(), after);
    }

    #[test]
    fn a_missing_or_altered_part_is_an_error_not_a_miss() {
        let kc = Limited::new(4000);
        let v = Vault::new(kc.clone());
        v.put("big", "openai-auth", &big('a', 7000)).unwrap();
        let head = kc.inner.get(SERVICE, "big:openai-auth").unwrap().unwrap();
        let m = parse_manifest(&head).unwrap();
        let part = part_name("big:openai-auth", &m.id, 1);
        let original = kc.inner.get(SERVICE, &part).unwrap().unwrap();
        kc.inner.set(SERVICE, &part, &original.replace('a', "b")).unwrap();
        assert!(v.get("big", "openai-auth").unwrap_err().to_string().contains("integrity"));
        kc.inner.delete(SERVICE, &part).unwrap();
        assert!(v.get("big", "openai-auth").unwrap_err().to_string().contains("incomplete"));
    }
}
