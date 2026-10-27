//! Sending RCLootCouncil's loot history to raid groups the way the WowUtils Bridge 1.1.0
//! does it, for groups whose admins turned on importing:
//!
//! 1. Awards that left RCLootCouncil are reported (`/loot-history/removals`) so the
//!    group's admins can review them. Never on a partial read, never more than 20 that
//!    vanished at once, and never ones older than two weeks.
//! 2. The site is offered the ids and hashes of awards it might not have yet
//!    (`/loot-history/offer`) and answers which ones it wants and how many right now.
//! 3. Those are sent (`/loot-history`), in batches.
//!
//! What the site acknowledged is kept in a ledger per group, so unchanged awards are
//! never offered again.

use super::rclc::{self, Award};
use super::{ApiError, Inner, API_BASE};
use crate::core::oauth::url_encode;
use crate::core::{self, store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

const LEDGER_SCHEMA: i64 = 1;
const OFFER_MAX_ENTRIES: usize = 5000;
const OFFER_MAX_BYTES: usize = 512_000;
const SEND_MAX_ENTRIES: usize = 2000;
const SEND_MAX_BYTES: usize = 1_000_000;
const REMOVALS_PER_CALL: usize = 20;
/// More acknowledged awards than this vanishing in one read looks like a reset or a
/// different RCLootCouncil profile, not deliberate deletions.
const MASS_VANISH: usize = 20;
/// Awards gone from RCLootCouncil that are older than this are only forgotten locally.
const REMOVAL_WINDOW: i64 = 14 * 86400 - 1;
const MAX_TRIES: u32 = 3;
const FULL_WAIT: i64 = 6 * 3600;
const DEFERRED_RETRY: i64 = 900;
/// A group with nothing to do is looked at again after this long (roster changes).
const IDLE_RECHECK: i64 = 1800;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Rejected {
    h: String,
    rules_version: Option<i64>,
}

/// What the site said about this group's awards so far.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Ledger {
    schema_version: i64,
    /// Award id -> hash the site has.
    acked: BTreeMap<String, String>,
    /// Awards the site refused under its rules version; offered again when either changes.
    rejected: BTreeMap<String, Rejected>,
    /// Award id -> roster version under which its player wasn't on the roster.
    off_roster: BTreeMap<String, String>,
    /// Acknowledged awards that vanished in a way that isn't trusted (see [`plan_removals`]).
    withheld: Vec<String>,
    /// Account keys read last time; `None` before the first complete read.
    accounts: Option<Vec<String>>,
    failures: BTreeMap<String, u32>,
    /// The site keeps no awards older than this (servertime).
    floor: Option<i64>,
    roster_version: String,
    rules_version: Option<i64>,
}

impl Ledger {
    fn path(group: &str) -> PathBuf {
        store::data_dir().join("wowutils").join(format!("loot-history-{group}.json"))
    }

    fn load(group: &str) -> Self {
        std::fs::read(Self::path(group)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&mut self, group: &str) -> std::io::Result<()> {
        self.schema_version = LEDGER_SCHEMA;
        core::write_atomic(&Self::path(group), &serde_json::to_vec(self).unwrap_or_default())
    }

    fn ack(&mut self, a: &Award) {
        self.acked.insert(a.id.clone(), a.h.clone());
        self.rejected.remove(&a.id);
        self.off_roster.remove(&a.id);
        self.failures.remove(&a.id);
    }

    fn reject(&mut self, a: &Award) {
        self.rejected.insert(a.id.clone(), Rejected { h: a.h.clone(), rules_version: self.rules_version });
    }

    /// Whether the site might still want this award (roster aside).
    fn open(&self, a: &Award) -> bool {
        if self.floor.is_some_and(|f| a.servertime().is_some_and(|t| t < f)) {
            return false;
        }
        if self.acked.get(&a.id) == Some(&a.h) {
            return false;
        }
        if self.rejected.get(&a.id).is_some_and(|r| r.h == a.h && r.rules_version == self.rules_version) {
            return false;
        }
        self.off_roster.get(&a.id) != Some(&self.roster_version)
    }
}

#[derive(Debug, Default, PartialEq)]
struct RemovalPlan {
    /// Ask the site to remove these (oldest first).
    removals: Vec<String>,
    /// Too old to bother the admins with: dropped from the ledger only.
    forget: Vec<String>,
    withheld: Vec<String>,
    accounts: Vec<String>,
    /// An account isn't read any more: this many vanished awards were withheld.
    account_gone: usize,
    /// This many vanished in one read: all withheld.
    mass_vanish: usize,
}

/// Which acknowledged awards to report as gone. `whole` = every account's RCLootCouncil
/// file was read completely this time; without that nothing is concluded.
fn plan_removals(l: &Ledger, current: &BTreeSet<String>, accounts: &[String], whole: bool, now: i64) -> Option<RemovalPlan> {
    if !whole {
        return None;
    }
    let account_gone = match &l.accounts {
        None => !l.acked.is_empty(),
        Some(before) => before.iter().any(|a| !accounts.contains(a)),
    };
    let withheld_before: HashSet<&String> = l.withheld.iter().collect();
    let still_withheld: Vec<String> = l.withheld.iter().filter(|id| !current.contains(*id)).cloned().collect();
    let vanished: Vec<String> =
        l.acked.keys().filter(|id| !current.contains(*id) && !withheld_before.contains(id)).cloned().collect();
    let mut plan = RemovalPlan { accounts: accounts.to_vec(), withheld: still_withheld, ..Default::default() };
    if account_gone || vanished.len() > MASS_VANISH {
        if account_gone {
            plan.account_gone = vanished.len();
        } else {
            plan.mass_vanish = vanished.len();
        }
        plan.withheld.extend(vanished);
        return Some(plan);
    }
    let mut recent: Vec<(i64, String)> = Vec::new();
    for id in vanished {
        match id.split('-').next().and_then(|t| t.parse::<i64>().ok()) {
            Some(t) if now - t <= REMOVAL_WINDOW => recent.push((t, id)),
            _ => plan.forget.push(id),
        }
    }
    recent.sort();
    plan.removals = recent.into_iter().map(|x| x.1).collect();
    Some(plan)
}

/// Awards to offer, oldest first.
fn candidates<'a>(l: &Ledger, awards: &'a BTreeMap<String, Award>, roster: &HashSet<String>) -> Vec<&'a Award> {
    let mut out: Vec<&Award> = awards
        .values()
        .filter(|a| a.valid() && roster.contains(&a.player_name().to_lowercase()) && l.open(a))
        .collect();
    out.sort_by_key(|a| (a.servertime().unwrap_or_default(), a.id.clone()));
    out
}

/// Splits `items` into request bodies `{"<key>":[...]}` within the given limits; an item
/// that alone exceeds the byte limit is left out.
fn batches<'a>(items: &[&'a Award], json: impl Fn(&Award) -> Value, max_n: usize, max_bytes: usize) -> Vec<Vec<&'a Award>> {
    let mut out = Vec::new();
    let mut cur: Vec<&Award> = Vec::new();
    let mut bytes = 14;
    for a in items {
        let n = serde_json::to_vec(&json(a)).map_or(usize::MAX, |v| v.len()) + 1;
        if n > max_bytes {
            continue;
        }
        if !cur.is_empty() && (cur.len() >= max_n || bytes + n > max_bytes) {
            out.push(std::mem::take(&mut cur));
            bytes = 14;
        }
        cur.push(*a);
        bytes += n;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn awards_text(n: usize) -> String {
    if n == 1 {
        "1 award".into()
    } else {
        format!("{n} awards")
    }
}

fn ids(v: &Value) -> HashSet<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// What this app last saw of the RCLootCouncil files.
#[derive(Default)]
pub(super) struct RclcCache {
    /// File -> (mtime, awards or why it couldn't be read).
    files: BTreeMap<PathBuf, (i64, Result<Vec<Award>, String>)>,
    /// Account keys that had loot history before (a file losing it isn't a complete read).
    had_history: BTreeSet<String>,
}

/// One read of every account's RCLootCouncil file.
struct RclcRead {
    awards: BTreeMap<String, Award>,
    accounts: Vec<String>,
    whole: bool,
    files: usize,
}

/// Per group, for the page.
#[derive(Debug, Clone, Default)]
pub(super) struct GroupStatus {
    pub next_check: i64,
    pub status: String,
    /// Fingerprint of the awards last looked at; a new one triggers a run right away.
    seen: String,
}

impl Inner {
    fn read_rclc(&self) -> RclcRead {
        let cfg = self.cfg.lock().unwrap().clone();
        let mut cache = self.rclc.lock().unwrap();
        let mut out = RclcRead { awards: BTreeMap::new(), accounts: Vec::new(), whole: true, files: 0 };
        let mut all = Vec::new();
        let mut seen_files = BTreeSet::new();
        for f in self.env.flavors() {
            for (account, sv) in f.accounts() {
                let key = format!("{}/{}", f.folder, account);
                if cfg.disabled_accounts.contains(&key) {
                    continue;
                }
                let akey = super::account_key(&sv);
                let file = sv.join("RCLootCouncil.lua");
                if !file.is_file() {
                    if cache.had_history.contains(&akey) {
                        out.whole = false;
                    }
                    continue;
                }
                out.files += 1;
                seen_files.insert(file.clone());
                let t = super::mtime(&file);
                if cache.files.get(&file).is_none_or(|(m, _)| *m != t) {
                    let r = std::fs::read(&file).map_err(|e| e.to_string()).and_then(|src| match rclc::read(&src) {
                        Ok(rclc::Read::Awards(a)) => Ok(a),
                        Ok(rclc::Read::NoHistory) => Err(String::new()),
                        Err(e) => Err(e.to_string()),
                    });
                    if let Err(e) = &r {
                        if !e.is_empty() {
                            self.common.warn(format!("{key}: couldn't read RCLootCouncil's loot history: {e}"));
                        }
                    }
                    cache.files.insert(file.clone(), (t, r));
                }
                let cache = &mut *cache;
                match &cache.files[&file].1 {
                    Ok(a) => {
                        cache.had_history.insert(akey.clone());
                        out.accounts.push(akey);
                        all.extend(a.iter().cloned());
                    }
                    // No loot history in the file: fine, unless it had some before.
                    Err(e) if e.is_empty() && !cache.had_history.contains(&akey) => out.accounts.push(akey),
                    Err(_) => out.whole = false,
                }
            }
        }
        cache.files.retain(|p, _| seen_files.contains(p));
        out.accounts.sort();
        out.awards = rclc::by_id(all);
        out
    }

    /// Character names on a group's roster (lowercase), from its cached group data.
    fn roster_names(&self, group: &str) -> Result<HashSet<String>, ApiError> {
        let path = Self::group_cache_path(group);
        if !path.is_file() {
            self.fetch_group(group)?;
        }
        Ok(super::group_payload(group)
            .map(|p| super::roster_entries(&p).into_iter().map(|(name, _)| name).collect())
            .unwrap_or_default())
    }

    /// Runs the loot history steps for every group the user chose.
    pub(super) fn sync_loot_history(&self, force: bool) -> Result<(), ApiError> {
        let chosen: Vec<(String, String)> = {
            let cfg = self.cfg.lock().unwrap();
            let st = self.st.lock().unwrap();
            let Some(groups) = &st.groups else { return Ok(()) };
            groups
                .iter()
                .filter(|g| g.can_import_loot_history && cfg.loot_history_groups.contains(&g.id))
                .map(|g| (g.id.clone(), if g.name.is_empty() { g.id.clone() } else { g.name.clone() }))
                .collect()
        };
        if chosen.is_empty() {
            return Ok(());
        }
        let read = self.read_rclc();
        let fingerprint = core::sha256_hex(
            read.awards.values().map(|a| a.h.as_str()).collect::<Vec<_>>().join(",").as_bytes(),
        );
        let now = core::now_unix();
        for (id, name) in chosen {
            let due = {
                let st = self.st.lock().unwrap();
                let s = st.loot.get(&id);
                force || s.is_none_or(|s| now >= s.next_check || s.seen != fingerprint)
            };
            if !due {
                continue;
            }
            if read.files == 0 {
                let mut st = self.st.lock().unwrap();
                let s = st.loot.entry(id.clone()).or_default();
                s.status = "No RCLootCouncil data on this computer yet".into();
                s.next_check = now + IDLE_RECHECK;
                s.seen = fingerprint.clone();
                continue;
            }
            match self.sync_group_loot(&id, &name, &read) {
                Ok((status, next)) => {
                    let mut st = self.st.lock().unwrap();
                    let s = st.loot.entry(id.clone()).or_default();
                    s.status = status;
                    s.next_check = now + next;
                    s.seen = fingerprint.clone();
                }
                Err(e @ ApiError::Status(..)) => {
                    self.common.error(format!("{name}: loot history: {e}"));
                    let mut st = self.st.lock().unwrap();
                    let s = st.loot.entry(id.clone()).or_default();
                    s.status = format!("Problem: {e}");
                    s.next_check = now + super::MIN_POLL * 5;
                    s.seen = fingerprint.clone();
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// One group's turn. Returns the status line and when to look again (seconds).
    fn sync_group_loot(&self, group: &str, name: &str, read: &RclcRead) -> Result<(String, i64), ApiError> {
        let mut l = Ledger::load(group);
        let now = core::now_unix();
        let save = |l: &mut Ledger| {
            if let Err(e) = l.save(group) {
                self.common.error(format!("{name}: couldn't save the loot-history ledger: {e}"));
            }
        };
        let current: BTreeSet<String> = read.awards.keys().cloned().collect();
        let mut removal_retry = None;

        // 1. Awards gone from RCLootCouncil.
        if let Some(plan) = plan_removals(&l, &current, &read.accounts, read.whole, now) {
            if plan.account_gone > 0 {
                self.common.warn(format!(
                    "{name}: an account is no longer read; {} acked awards it may hold were withheld.",
                    plan.account_gone
                ));
            }
            if plan.mass_vanish > 0 {
                self.common.warn(format!(
                    "Some awards disappeared from RCLC at once for {name}, so none of them were removed on the site. \
                     ({} acked awards vanished in one read; withheld.)",
                    plan.mass_vanish
                ));
            }
            for id in &plan.forget {
                l.acked.remove(id);
            }
            l.withheld = plan.withheld.clone();
            l.accounts = Some(plan.accounts.clone());
            save(&mut l);
            if !plan.removals.is_empty() {
                let batch: Vec<&String> = plan.removals.iter().take(REMOVALS_PER_CALL).collect();
                let v: Value = self
                    .call(
                        self.http
                            .post(format!("{API_BASE}/bridge/v1/groups/{}/loot-history/removals", url_encode(group)))
                            .json(&json!({ "ids": batch })),
                    )?
                    .json()?;
                let removed = ids(&v["removed"]);
                for id in removed.iter().chain(&ids(&v["ignored"])) {
                    l.acked.remove(id);
                }
                save(&mut l);
                if !removed.is_empty() {
                    self.common.info(format!(
                        "Asked {name}'s admins to review {} gone from RCLC.",
                        awards_text(removed.len())
                    ));
                }
                if !ids(&v["deferred"]).is_empty() || plan.removals.len() > batch.len() {
                    removal_retry = Some(if plan.removals.len() > batch.len() { super::MIN_POLL } else { DEFERRED_RETRY });
                }
            }
        }

        // 2. Offer what the site might not have.
        let roster = self.roster_names(group)?;
        if roster.is_empty() {
            return Ok(("Add your raiders to the roster on the site to send loot history".into(), IDLE_RECHECK));
        }
        let open = candidates(&l, &read.awards, &roster);
        let waiting_roster = read
            .awards
            .values()
            .filter(|a| a.valid() && l.open(a) && !roster.contains(&a.player_name().to_lowercase()))
            .count();
        let roster_note = if waiting_roster > 0 {
            format!(
                " · {waiting_roster} {} for a roster match",
                if waiting_roster == 1 { "waits" } else { "wait" }
            )
        } else {
            String::new()
        };
        if open.is_empty() {
            let next = removal_retry.unwrap_or(IDLE_RECHECK);
            return Ok((format!("Nothing new{roster_note}"), next));
        }
        let offered = batches(&open, Award::offer_json, OFFER_MAX_ENTRIES, OFFER_MAX_BYTES).into_iter().next().unwrap_or_default();
        let body = json!({ "entries": offered.iter().map(|a| a.offer_json()).collect::<Vec<_>>() });
        let resp: Value = self
            .call(
                self.http
                    .post(format!("{API_BASE}/bridge/v1/groups/{}/loot-history/offer", url_encode(group)))
                    .json(&body),
            )?
            .json()?;
        let retry_after = resp["retryAfterSeconds"].as_i64().unwrap_or(1).max(1);
        l.roster_version = resp["rosterVersion"].as_str().unwrap_or_default().to_string();
        if let Some(f) = resp["floor"].as_i64() {
            l.floor = Some(f);
        }
        let no_roster = resp["noRoster"].as_bool().unwrap_or(false);
        let want = ids(&resp["want"]);
        if !no_roster {
            let off = ids(&resp["offRoster"]);
            for a in &offered {
                if off.contains(&a.id) {
                    l.off_roster.insert(a.id.clone(), l.roster_version.clone());
                } else if !want.contains(&a.id) {
                    // Not wanted: the site already has this version (unless it's below the floor).
                    if l.floor.is_none_or(|f| a.servertime().is_some_and(|t| t >= f)) {
                        l.ack(a);
                    }
                } else {
                    l.off_roster.remove(&a.id);
                }
            }
        }
        save(&mut l);
        if no_roster {
            return Ok(("Add your raiders to the roster on the site to send loot history".into(), IDLE_RECHECK));
        }
        let full = resp["full"].as_bool().unwrap_or(false);
        if full {
            self.common.warn(format!(
                "{name}: Loot history is full on the site, so new awards wait here until there's room. \
                 Clear out old entries in Manage loot history."
            ));
            return Ok(("Loot history is full on the site".into(), FULL_WAIT));
        }
        let wanted: Vec<&Award> = offered.iter().copied().filter(|a| want.contains(&a.id)).collect();
        let send_now = usize::try_from(resp["sendNow"].as_i64().unwrap_or(0)).unwrap_or(0).min(wanted.len());
        let to_send = &wanted[..send_now];
        let more = send_now < wanted.len() || offered.len() < open.len();

        // 3. Send them.
        let mut sent = 0;
        let mut next = if more { retry_after } else { removal_retry.unwrap_or(IDLE_RECHECK) };
        for batch in batches(to_send, Award::upload_json, SEND_MAX_ENTRIES, SEND_MAX_BYTES) {
            let body = json!({ "entries": batch.iter().map(|a| a.upload_json()).collect::<Vec<_>>() });
            let r: Value = self
                .call(
                    self.http
                        .post(format!("{API_BASE}/bridge/v1/groups/{}/loot-history", url_encode(group)))
                        .json(&body),
                )?
                .json()?;
            if let Some(rv) = r["rulesVersion"].as_i64() {
                l.rules_version = Some(rv);
            }
            let count = |k: &str| r[k].as_i64().unwrap_or(0);
            let (skipped, off, capped, deferred, failed) =
                (ids(&r["skipped"]), ids(&r["offRoster"]), ids(&r["capped"]), ids(&r["deferred"]), ids(&r["failed"]));
            let total = count("inserted") + count("modified") + count("adopted") + count("unchanged")
                + (skipped.len() + off.len() + capped.len() + deferred.len() + failed.len()) as i64;
            let counts_ok = total == batch.len() as i64;
            if !counts_ok {
                self.common.warn(format!(
                    "{name}: the send reply accounts for a different number of rows than the {} sent; none were acked.",
                    batch.len()
                ));
            }
            let mut gave_up = 0;
            for a in &batch {
                if failed.contains(&a.id) {
                    let n = l.failures.get(&a.id).copied().unwrap_or(0) + 1;
                    if n >= MAX_TRIES {
                        l.failures.remove(&a.id);
                        l.reject(a);
                        gave_up += 1;
                    } else {
                        l.failures.insert(a.id.clone(), n);
                    }
                    continue;
                }
                l.failures.remove(&a.id);
                if skipped.contains(&a.id) {
                    l.reject(a);
                } else if off.contains(&a.id) {
                    l.off_roster.insert(a.id.clone(), l.roster_version.clone());
                } else if capped.contains(&a.id) || deferred.contains(&a.id) {
                    // Stays open, offered again later.
                } else if counts_ok {
                    l.ack(a);
                }
            }
            save(&mut l);
            let n = (count("inserted") + count("modified") + count("adopted")) as usize;
            sent += n;
            if n > 0 {
                self.common.good(format!("Sent {} to {name}'s loot history.", awards_text(n)));
            }
            if gave_up > 0 {
                self.common.error(format!(
                    "{} for {name} couldn't be saved on the site after three tries, so this app stopped sending {}.",
                    awards_text(gave_up),
                    if gave_up == 1 { "it" } else { "them" }
                ));
            }
            if !capped.is_empty() {
                self.common.warn(format!(
                    "{name}: Loot history is full on the site, so new awards wait here until there's room. \
                     Clear out old entries in Manage loot history."
                ));
                return Ok(("Loot history is full on the site".into(), FULL_WAIT));
            }
            if !deferred.is_empty() || !failed.is_empty() {
                next = next.min(retry_after.max(super::MIN_POLL));
            }
        }
        let status = if sent > 0 {
            format!("{} sent {}{roster_note}", awards_text(sent), core::fmt_time(now))
        } else if more || !to_send.is_empty() {
            format!("Waiting for the site{roster_note}")
        } else {
            format!("Nothing new{roster_note}")
        };
        Ok((status, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn award(id: &str, player: &str) -> Award {
        let src = format!(
            r#"RCLootCouncilLootDB = {{ ["factionrealm"] = {{ ["A - B"] = {{ ["{player}"] = {{
                {{ ["id"] = "{id}", ["lootWon"] = "|Hitem:1::|h[X]|h|r" }},
            }} }} }} }}"#
        );
        let rclc::Read::Awards(mut a) = rclc::read(src.as_bytes()).unwrap() else { panic!() };
        a.remove(0)
    }

    #[test]
    fn candidates_respect_ledger_and_roster() {
        let a = award("1780000000-1", "Abe-Realm");
        let b = award("1780000001-1", "Bob-Realm");
        let awards = rclc::by_id([a.clone(), b.clone()]);
        let roster: HashSet<String> = ["abe".to_string(), "bob".to_string()].into();
        let mut l = Ledger::default();
        assert_eq!(candidates(&l, &awards, &roster).len(), 2);
        l.ack(&a);
        assert_eq!(candidates(&l, &awards, &roster).iter().map(|x| &x.id).collect::<Vec<_>>(), [&b.id]);
        l.off_roster.insert(b.id.clone(), "r1".into());
        assert_eq!(candidates(&l, &awards, &roster).len(), 1, "other roster version: offered again");
        l.roster_version = "r1".into();
        assert!(candidates(&l, &awards, &roster).is_empty());
        let only_abe: HashSet<String> = ["abe".to_string()].into();
        assert!(candidates(&Ledger::default(), &awards, &only_abe).iter().all(|x| x.player == "Abe-Realm"));
    }

    #[test]
    fn removals_are_careful() {
        let now = 1_780_000_000 + 86400;
        let mut l = Ledger { accounts: Some(vec!["k1".into()]), ..Default::default() };
        l.acked.insert("1780000000-1".into(), "h".into());
        l.acked.insert("1700000000-1".into(), "h".into());
        let current = BTreeSet::new();
        // Partial read: nothing.
        assert!(plan_removals(&l, &current, &["k1".into()], false, now).is_none());
        let p = plan_removals(&l, &current, &["k1".into()], true, now).unwrap();
        assert_eq!(p.removals, ["1780000000-1"]);
        assert_eq!(p.forget, ["1700000000-1"]);
        // An account no longer read: withheld instead.
        let p = plan_removals(&l, &current, &["k2".into()], true, now).unwrap();
        assert!(p.removals.is_empty());
        assert_eq!(p.withheld.len(), 2);
        // Many at once: withheld.
        for i in 0..30 {
            l.acked.insert(format!("1780000000-{}", i + 10), "h".into());
        }
        let p = plan_removals(&l, &current, &["k1".into()], true, now).unwrap();
        assert!(p.removals.is_empty() && p.mass_vanish == 32);
    }

    #[test]
    fn batching() {
        let list: Vec<Award> = (0..5).map(|i| award(&format!("1780000000-{i}"), "Abe-Realm")).collect();
        let refs: Vec<&Award> = list.iter().collect();
        assert_eq!(batches(&refs, Award::upload_json, 2, SEND_MAX_BYTES).iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
        assert_eq!(batches(&refs, Award::upload_json, 10, 20).len(), 0, "too big alone: left out");
    }
}
