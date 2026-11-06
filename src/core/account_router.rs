//! Two-slot ChatGPT subscription routing.
//!
//! The router deliberately has a very small policy surface: the currently
//! active slot is sticky and persisted, and only a confirmed subscription
//! exhaustion signal may move it to the other configured slot.

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::login::lib::{AuthMode, CodexAuth};

const STATE_SCHEMA: &str = "relay.account-router.v1";
pub const ACCOUNT_SLOTS: [&str; 2] = ["primary", "secondary"];
const DEFAULT_COOLDOWN_SECONDS: u64 = 15 * 60;

#[derive(Clone)]
pub struct AccountRouter {
    accounts: Arc<Vec<AccountProfile>>,
    state: Arc<Mutex<PersistentState>>,
    state_file: Arc<PathBuf>,
    auth_root: Arc<PathBuf>,
    cooldown: Duration,
    #[cfg(test)]
    lease_gate: Arc<std::sync::Mutex<Option<Arc<TestGate>>>>,
    #[cfg(test)]
    commit_gate: Arc<std::sync::Mutex<Option<Arc<TestCommitGate>>>>,
    /// Where the next deliberate switch stops once committed -- the state
    /// lock released, its lease not yet handed back to the caller.
    #[cfg(test)]
    handback_gate: Arc<std::sync::Mutex<Option<Arc<TestGate>>>>,
    /// What the next `login` gets instead of running the browser OAuth
    /// helper -- a staged profile, or an OAuth failure -- so a sign-in can be
    /// driven end to end in tests.
    #[cfg(test)]
    staged_login: Arc<std::sync::Mutex<Option<std::io::Result<tempfile::TempDir>>>>,
}

/// A linearization guard shared with the downstream receiver. Receiver drop
/// takes the write side; account-state persistence takes the read side. Thus a
/// completed cancellation can never be followed by an account-state commit.
#[derive(Clone, Default)]
pub(crate) struct DownstreamCancellation {
    cancelled: Arc<std::sync::RwLock<bool>>,
    #[cfg(test)]
    cancel_attempts: Arc<AtomicUsize>,
}

impl DownstreamCancellation {
    pub(crate) fn cancel(&self) {
        #[cfg(test)]
        self.cancel_attempts.fetch_add(1, AtomicOrdering::SeqCst);
        let mut cancelled = self
            .cancelled
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cancelled = true;
    }

    fn commit_permit(&self) -> Option<std::sync::RwLockReadGuard<'_, bool>> {
        let permit = self
            .cancelled
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *permit {
            None
        } else {
            Some(permit)
        }
    }

    #[cfg(test)]
    fn cancel_attempts(&self) -> usize {
        self.cancel_attempts.load(AtomicOrdering::SeqCst)
    }
}

/// One-shot synchronization point used by cancellation regressions. Semaphores
/// retain permits, unlike Notify, so a fast boundary cannot be missed.
#[cfg(test)]
pub(crate) struct TestGate {
    reached: Semaphore,
    release: Semaphore,
}

#[cfg(test)]
impl TestGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            reached: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    pub(crate) async fn wait_reached(&self) {
        self.reached.acquire().await.unwrap().forget();
    }

    pub(crate) fn release(&self) {
        self.release.add_permits(1);
    }

    async fn stop(&self) {
        self.reached.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }
}

/// Commit-only boundary: `stop` is deliberately synchronous because it runs
/// while holding the cancellation permit, which is a std RwLock read guard and
/// must never cross an `.await` in the production commit future.
#[cfg(test)]
pub(crate) struct TestCommitGate {
    reached: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl TestCommitGate {
    pub(crate) fn new() -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                reached: reached_tx,
                release: std::sync::Mutex::new(release_rx),
            }),
            reached_rx,
            release_tx,
        )
    }

    fn stop(&self) {
        self.reached.send(()).unwrap();
        self.release
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv()
            .unwrap();
    }
}

struct AccountProfile {
    slot: String,
    auth_home: PathBuf,
    refresh_lock: Mutex<()>,
}

impl AccountProfile {
    fn configured(&self) -> bool {
        self.auth_home.join("auth.json").is_file()
    }
}

/// One internally consistent credential snapshot.  It intentionally has no
/// `Debug` implementation so bearer tokens cannot be logged by accident.
pub struct AccountLease {
    pub slot: String,
    pub access_token: String,
    pub account_id: String,
}

/// One slot as seen from outside.  Carries no credential material by design.
#[derive(Clone, Debug, Serialize)]
pub struct SlotView {
    pub slot: String,
    pub configured: bool,
    pub active: bool,
    pub cooldown_seconds_left: i64,
}

#[derive(Clone, Deserialize, Serialize)]
struct PersistentState {
    schema: String,
    active: String,
    #[serde(default)]
    exhausted_until: BTreeMap<String, i64>,
}

impl AccountRouter {
    pub async fn load(auth_root: &Path) -> Result<Self> {
        let router = Self::open(auth_root).await?;
        router
            .lease_any()
            .await
            .context("no usable ChatGPT account profile")?;
        Ok(router)
    }

    /// Start the control surface even when every account is logged out.
    pub async fn open(auth_root: &Path) -> Result<Self> {
        let accounts_root = auth_root.join("accounts");
        let mut accounts = Vec::new();
        for slot in ACCOUNT_SLOTS {
            let profile = accounts_root.join(slot);
            // Preserve the old single-account layout without duplicating its tokens.
            let auth_home = if slot == "primary" && !profile.join("auth.json").is_file() {
                auth_root.to_path_buf()
            } else {
                profile
            };
            accounts.push(AccountProfile {
                slot: slot.to_string(),
                auth_home,
                refresh_lock: Mutex::new(()),
            });
        }

        let state_file = std::env::var_os("RELAY_ACCOUNT_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| auth_root.join("router-state.json"));
        let first_slot = accounts
            .iter()
            .find(|a| a.configured())
            .unwrap_or(&accounts[0])
            .slot
            .clone();
        let mut state = if state_file.exists() {
            let bytes = fs::read(&state_file).context("failed to read account router state")?;
            let state: PersistentState =
                serde_json::from_slice(&bytes).context("invalid account router state")?;
            if state.schema != STATE_SCHEMA {
                bail!("unsupported account router state schema");
            }
            state
        } else {
            PersistentState {
                schema: STATE_SCHEMA.to_string(),
                active: first_slot.clone(),
                exhausted_until: BTreeMap::new(),
            }
        };
        if !accounts
            .iter()
            .any(|account| account.slot == state.active && account.configured())
        {
            warn!("persisted account slot is unavailable; selecting the first configured slot");
            state.active = first_slot;
        }

        let cooldown_seconds = std::env::var("RELAY_ACCOUNT_COOLDOWN_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_COOLDOWN_SECONDS)
            .clamp(60, 7 * 24 * 60 * 60);
        let router = Self {
            accounts: Arc::new(accounts),
            state: Arc::new(Mutex::new(state.clone())),
            state_file: Arc::new(state_file),
            auth_root: Arc::new(auth_root.to_path_buf()),
            cooldown: Duration::from_secs(cooldown_seconds),
            #[cfg(test)]
            lease_gate: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            commit_gate: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            handback_gate: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            staged_login: Arc::new(std::sync::Mutex::new(None)),
        };
        router.persist(&state)?;

        info!(
            "account router ready: active={}, configured={}",
            router.active_slot().await,
            router.account_count()
        );
        Ok(router)
    }

    pub fn account_count(&self) -> usize {
        self.accounts.iter().filter(|a| a.configured()).count()
    }

    pub async fn active_slot(&self) -> String {
        self.state.lock().await.active.clone()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn lease_active(&self) -> Result<AccountLease> {
        let slot = self.active_slot().await;
        self.lease_slot(&slot).await
    }

    /// Аренда с фолбэком (R2+R4, 17.08.2026).
    ///
    /// ⚠ ЖИВОЙ СЛУЧАЙ 13.08: файл активного слота перестал читаться, каждый запрос
    /// упирался в ту же аренду, и Praxis молчала полтора часа при ЗДОРОВОМ втором
    /// слоте. Здесь отказ активного не валит запрос: слот паркуется на кулдаун
    /// (тот же `exhausted_until`, что у квоты: истекает сам, снимается осознанным
    /// `switch_to`, виден в `/v1/account` как cooldown), а аренда уходит в резерв.
    ///
    /// Три пола, каждый закрывает известный тупик из отчёта Кодекса:
    /// * **последний живой слот не паркуется никогда** — иначе R2+R4 в связке умели
    ///   запарковать оба слота и уронить процесс в цикл рестартов;
    /// * паркинг ВСЕГДА с TTL — вечной метки `needs_login` здесь нет по построению;
    /// * ошибка несёт обе причины (активный и резерв), а не последнюю.
    pub async fn lease_any(&self) -> Result<AccountLease> {
        self.lease_any_guarded(None).await
    }

    pub(crate) async fn lease_any_for_downstream(
        &self,
        cancellation: &DownstreamCancellation,
    ) -> Result<AccountLease> {
        self.lease_any_guarded(Some(cancellation)).await
    }

    async fn lease_any_guarded(
        &self,
        cancellation: Option<&DownstreamCancellation>,
    ) -> Result<AccountLease> {
        let active = self.active_slot().await;
        let active_err = match self.lease_slot(&active).await {
            Ok(lease) => return Ok(lease),
            Err(error) => error,
        };
        let now = Utc::now().timestamp();
        let candidate = {
            let state = self.state.lock().await;
            self.accounts
                .iter()
                .find(|account| {
                    account.configured()
                        && account.slot != active
                        && state
                            .exhausted_until
                            .get(&account.slot)
                            .copied()
                            .unwrap_or(0)
                            <= now
                })
                .map(|account| account.slot.clone())
        };
        let Some(candidate) = candidate else {
            return Err(active_err.context("active slot failed and no standby is available"));
        };
        let standby = match self.lease_slot(&candidate).await {
            Ok(lease) => lease,
            Err(standby_err) => {
                return Err(standby_err.context(format!(
                    "active slot {} failed ({:#}) and standby {} failed too",
                    active, active_err, candidate
                )));
            }
        };
        let mut state = self.state.lock().await;
        let _commit_permit = cancellation.and_then(DownstreamCancellation::commit_permit);
        if cancellation.is_some() && _commit_permit.is_none() {
            bail!("downstream cancelled before account-state commit");
        }
        #[cfg(test)]
        self.stop_at_commit_gate();
        if state.active == active {
            let mut next = state.clone();
            next.active = standby.slot.clone();
            next.exhausted_until
                .insert(active.clone(), now + self.cooldown.as_secs() as i64);
            self.persist(&next)?;
            *state = next;
            warn!(
                "active slot {} is unusable ({:#}); parked for {}s, switched to {}",
                active,
                active_err,
                self.cooldown.as_secs(),
                standby.slot
            );
        }
        Ok(standby)
    }

    /// Atomically marks `exhausted` unavailable for a bounded cooldown and
    /// makes the other valid account the global active slot.  Concurrent
    /// requests that saw the same exhaustion converge on the first switch.
    pub async fn switch_after_quota(&self, exhausted: &AccountLease) -> Result<AccountLease> {
        self.switch_after_quota_guarded(exhausted, None).await
    }

    pub(crate) async fn switch_after_quota_for_downstream(
        &self,
        exhausted: &AccountLease,
        cancellation: &DownstreamCancellation,
    ) -> Result<AccountLease> {
        self.switch_after_quota_guarded(exhausted, Some(cancellation))
            .await
    }

    async fn switch_after_quota_guarded(
        &self,
        exhausted: &AccountLease,
        cancellation: Option<&DownstreamCancellation>,
    ) -> Result<AccountLease> {
        let now = Utc::now().timestamp();
        let candidate = {
            let state = self.state.lock().await;
            if state.active != exhausted.slot {
                let active = state.active.clone();
                drop(state);
                return self.lease_slot(&active).await;
            }
            self.accounts
                .iter()
                .find(|account| {
                    account.configured()
                        && account.slot != exhausted.slot
                        && state
                            .exhausted_until
                            .get(&account.slot)
                            .copied()
                            .unwrap_or(0)
                            <= now
                })
                .map(|account| account.slot.clone())
                .ok_or_else(|| anyhow!("no standby subscription is currently available"))?
        };

        // Validate and refresh the standby before changing global state.
        let standby = self
            .lease_slot(&candidate)
            .await
            .context("standby ChatGPT account is unusable")?;
        if standby.account_id == exhausted.account_id {
            bail!("standby profile resolves to the exhausted ChatGPT account");
        }

        let mut state = self.state.lock().await;
        if state.active != exhausted.slot {
            let active = state.active.clone();
            drop(state);
            return self.lease_slot(&active).await;
        }
        let _commit_permit = cancellation.and_then(DownstreamCancellation::commit_permit);
        if cancellation.is_some() && _commit_permit.is_none() {
            bail!("downstream cancelled before account-state commit");
        }
        #[cfg(test)]
        self.stop_at_commit_gate();
        let mut next = state.clone();
        next.active = standby.slot.clone();
        next.exhausted_until
            .insert(exhausted.slot.clone(), now + self.cooldown.as_secs() as i64);
        self.persist(&next)?;
        *state = next;
        info!(
            "subscription quota exhausted: switched active slot {} -> {}",
            exhausted.slot, standby.slot
        );
        Ok(standby)
    }

    /// Every configured slot, which one is active, and how long a slot stays
    /// parked after its quota ran out.  Read-only: this is what an operator —
    /// or Praxis herself — needs before deciding anything.
    pub async fn describe(&self) -> Vec<SlotView> {
        let now = Utc::now().timestamp();
        let state = self.state.lock().await;
        self.accounts
            .iter()
            .map(|account| {
                let until = state
                    .exhausted_until
                    .get(&account.slot)
                    .copied()
                    .unwrap_or(0);
                SlotView {
                    slot: account.slot.clone(),
                    configured: account.configured(),
                    active: account.slot == state.active && account.configured(),
                    cooldown_seconds_left: (until - now).max(0),
                }
            })
            .collect()
    }

    /// A deliberate move to a named slot.
    ///
    /// `switch_after_quota` answers "the current subscription is spent"; this
    /// answers "I want the other one now" — the two must not be the same door.
    /// The standby is validated and refreshed BEFORE global state moves, exactly
    /// as on the quota path, so a broken profile cannot take the live one down.
    ///
    /// A deliberate switch also clears that slot's cooldown: the timer was our
    /// guess about when the subscription might be usable again, and an explicit
    /// decision outranks a guess.  Genuine exhaustion re-arms it on the next 429.
    ///
    /// `switched` runs once, with the selected slot, synchronously and under
    /// the state lock (so it must not block), the moment the move is
    /// committed: persisted and live. That lock serializes commits, so
    /// overlapping switches report in the order they took effect and the
    /// last report names the slot left active; a caller told only once this
    /// returns could hear an earlier switch last. A switch to the active slot
    /// reports too; one that fails, having moved nothing, never runs it.
    pub async fn switch_to(&self, slot: &str, switched: impl FnOnce(&str)) -> Result<AccountLease> {
        let wanted = slot.trim();
        if !self
            .accounts
            .iter()
            .any(|account| account.slot == wanted && account.configured())
        {
            let configured: Vec<&str> = self
                .accounts
                .iter()
                .map(|account| account.slot.as_str())
                .collect();
            bail!(
                "account slot {:?} is not configured (configured: {})",
                wanted,
                configured.join(", ")
            );
        }
        let target = self
            .lease_slot(wanted)
            .await
            .context("requested ChatGPT account is unusable")?;

        let mut state = self.state.lock().await;
        let previous = state.active.clone();
        let mut next = state.clone();
        next.active = target.slot.clone();
        next.exhausted_until.remove(&target.slot);
        self.persist(&next)?;
        *state = next;
        // Still under the lock: no later switch can commit, let alone
        // report, before this report is in.
        switched(&target.slot);
        info!(
            "subscription switched deliberately: {} -> {}",
            previous, target.slot
        );
        drop(state);
        #[cfg(test)]
        self.stop_at_gate(&self.handback_gate).await;
        Ok(target)
    }

    pub fn auth_home(&self, slot: &str) -> Result<&Path> {
        self.accounts
            .iter()
            .find(|a| a.slot == slot)
            .map(|a| a.auth_home.as_path())
            .ok_or_else(|| anyhow!("unknown slot; use primary or secondary"))
    }

    /// Share the refresh lock with login/logout so refresh cannot resurrect a
    /// removed credential file. Existing requests may finish their leased turn.
    ///
    /// `credentials_changed` runs once, synchronously and under that lock (so
    /// it must not block), the moment the first credential file is gone:
    /// before the legacy cleanup and the state write, either of which can
    /// still fail, and before the next await, where the call can be
    /// cancelled. A sign-out with nothing to remove runs it on success
    /// instead; one that fails before removing anything never runs it.
    pub async fn logout(&self, slot: &str, credentials_changed: impl FnOnce()) -> Result<bool> {
        let account = self
            .accounts
            .iter()
            .find(|a| a.slot == slot)
            .ok_or_else(|| anyhow!("unknown slot; use primary or secondary"))?;
        let _guard = account.refresh_lock.lock().await;
        let mut credentials_changed = Some(credentials_changed);
        let mut remove = |codex_home: &Path| -> std::io::Result<bool> {
            let removed = crate::login::lib::logout(codex_home)?;
            if removed {
                if let Some(report) = credentials_changed.take() {
                    report();
                }
            }
            Ok(removed)
        };
        let mut removed = remove(&account.auth_home)?;
        if slot == "primary" {
            // Remove both historical representations of this slot. Otherwise a
            // restart could silently resurrect an ignored legacy auth.json.
            removed |= remove(&self.auth_root)?;
            removed |= remove(&self.auth_root.join("accounts/primary"))?;
        }
        let mut state = self.state.lock().await;
        let mut next = state.clone();
        next.exhausted_until.remove(slot);
        if next.active == slot {
            if let Some(other) = self
                .accounts
                .iter()
                .find(|a| a.slot != slot && a.configured())
            {
                next.active = other.slot.clone();
            }
        }
        self.persist(&next)?;
        *state = next;
        // Nothing was there to remove, and the slot is signed out all the same.
        if let Some(report) = credentials_changed.take() {
            report();
        }
        Ok(removed)
    }

    /// `credentials_changed` runs once, synchronously and under the slot's
    /// refresh lock (so it must not block), the moment the new credentials are
    /// installed: before the state write, which can still fail, and before the
    /// next await, where the call can be cancelled. A sign-in that fails or is
    /// cancelled before the install -- in OAuth, say -- never runs it.
    pub async fn login(&self, slot: &str, credentials_changed: impl FnOnce()) -> Result<()> {
        let account = self
            .accounts
            .iter()
            .find(|a| a.slot == slot)
            .ok_or_else(|| anyhow!("unknown slot; use primary or secondary"))?;
        // OAuth is staged away from live credentials. A failed/cancelled login
        // leaves the previous account usable; only the final replace is locked.
        let staged = self.stage_login(&account.auth_home).await?;
        let _guard = account.refresh_lock.lock().await;
        crate::login::lib::install_staged_login(staged.path(), &account.auth_home)?;
        // The slot holds the new account from here on, whatever happens next.
        credentials_changed();
        let mut state = self.state.lock().await;
        let mut next = state.clone();
        next.exhausted_until.remove(slot);
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    /// The browser OAuth helper's staged profile -- or, in tests, what
    /// `stage_next_login` or `fail_next_login` handed over.
    async fn stage_login(&self, auth_home: &Path) -> std::io::Result<tempfile::TempDir> {
        #[cfg(test)]
        {
            let staged = self
                .staged_login
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(staged) = staged {
                return staged;
            }
        }
        crate::login::lib::staged_chatgpt_login(auth_home).await
    }

    #[cfg(test)]
    pub(crate) fn stage_next_login(&self, staged: tempfile::TempDir) {
        *self
            .staged_login
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Ok(staged));
    }

    /// The next `login` fails in OAuth, as a closed browser would, before any
    /// credential is touched.
    #[cfg(test)]
    pub(crate) fn fail_next_login(&self, error: std::io::Error) {
        *self
            .staged_login
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Err(error));
    }

    pub(crate) async fn lease_slot(&self, slot: &str) -> Result<AccountLease> {
        #[cfg(test)]
        self.stop_at_gate(&self.lease_gate).await;
        let account = self
            .accounts
            .iter()
            .find(|account| account.slot == slot)
            .ok_or_else(|| anyhow!("account slot is not configured"))?;
        // CodexAuth may refresh and rewrite auth.json.  Serialize that operation
        // per account so concurrent requests never race a token refresh.
        let _refresh_guard = account.refresh_lock.lock().await;
        let auth = CodexAuth::from_codex_home(&account.auth_home)
            .context("failed to load ChatGPT authentication")?
            .ok_or_else(|| anyhow!("ChatGPT authentication is not configured"))?;
        if auth.mode != AuthMode::ChatGPT {
            bail!("ChatGPT subscription authentication is required");
        }
        let tokens = auth
            .get_token_data()
            .await
            .context("failed to refresh ChatGPT authentication")?;
        if tokens.access_token.trim().is_empty() {
            bail!("ChatGPT access token is empty");
        }
        let account_id = tokens
            .account_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("ChatGPT account id is missing"))?;
        Ok(AccountLease {
            slot: account.slot.clone(),
            access_token: tokens.access_token,
            account_id,
        })
    }

    #[cfg(test)]
    async fn stop_at_gate(&self, gate_slot: &std::sync::Mutex<Option<Arc<TestGate>>>) {
        let gate = gate_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(gate) = gate {
            gate.stop().await;
        }
    }

    #[cfg(test)]
    fn stop_at_commit_gate(&self) {
        let gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(gate) = gate {
            gate.stop();
        }
    }

    #[cfg(test)]
    pub(crate) fn gate_next_lease(&self, gate: Arc<TestGate>) {
        *self
            .lease_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gate);
    }

    #[cfg(test)]
    pub(crate) fn gate_next_commit(&self, gate: Arc<TestCommitGate>) {
        *self
            .commit_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gate);
    }

    /// The next deliberate switch stops once it has committed and released
    /// the state lock, before handing its lease back: the window in which a
    /// report made by the caller could be overtaken by a later switch.
    #[cfg(test)]
    pub(crate) fn gate_next_handback(&self, gate: Arc<TestGate>) {
        *self
            .handback_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gate);
    }

    fn persist(&self, state: &PersistentState) -> Result<()> {
        let parent = self
            .state_file
            .parent()
            .ok_or_else(|| anyhow!("account router state path has no parent"))?;
        fs::create_dir_all(parent).context("failed to create account router state directory")?;
        let mut temp = NamedTempFile::new_in(parent)
            .context("failed to create temporary account router state")?;
        serde_json::to_writer_pretty(temp.as_file_mut(), state)
            .context("failed to serialize account router state")?;
        temp.as_file_mut()
            .write_all(b"\n")
            .context("failed to terminate account router state")?;
        temp.as_file_mut()
            .sync_all()
            .context("failed to sync account router state")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .context("failed to protect account router state")?;
        }
        temp.persist(self.state_file.as_ref())
            .map_err(|error| error.error)
            .context("failed to atomically replace account router state")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;
    use tempfile::tempdir;

    fn write_account(root: &Path, slot: &str, account_id: &str) {
        let home = root.join("accounts").join(slot);
        fs::create_dir_all(&home).unwrap();
        let payload = json!({
            "email": format!("{slot}@example.test"),
            "https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let jwt = format!("e30.{encoded}.c2ln");
        let auth = json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt,
                "access_token": format!("access-{slot}"),
                "refresh_token": format!("refresh-{slot}"),
                "account_id": account_id
            },
            "last_refresh": Utc::now().to_rfc3339()
        });
        fs::write(
            home.join("auth.json"),
            serde_json::to_vec_pretty(&auth).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn empty_relay_accepts_credentials_without_restart_and_logout_removes_them() {
        let root = tempdir().unwrap();
        let router = AccountRouter::open(root.path()).await.unwrap();
        assert_eq!(router.account_count(), 0);
        assert!(router.lease_any().await.is_err());
        write_account(root.path(), "secondary", "acct-secondary");
        assert_eq!(
            router
                .switch_to("secondary", |_| {})
                .await
                .unwrap()
                .account_id,
            "acct-secondary"
        );
        assert_eq!(router.account_count(), 1);
        router.logout("secondary", || {}).await.unwrap();
        assert_eq!(router.account_count(), 0);
        assert!(router.lease_any().await.is_err());
        assert_eq!(
            AccountRouter::open(root.path())
                .await
                .unwrap()
                .account_count(),
            0
        );
    }

    #[tokio::test]
    async fn a_second_account_can_be_added_and_selected_while_serving() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        write_account(root.path(), "secondary", "acct-secondary");
        assert_eq!(router.account_count(), 2);
        router.switch_to("secondary", |_| {}).await.unwrap();
        router.logout("secondary", || {}).await.unwrap();
        assert_eq!(router.lease_any().await.unwrap().account_id, "acct-primary");
    }

    #[tokio::test]
    async fn logout_cannot_resurrect_shadow_legacy_credentials_after_restart() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        fs::copy(
            root.path().join("accounts/primary/auth.json"),
            root.path().join("auth.json"),
        )
        .unwrap();
        let router = AccountRouter::load(root.path()).await.unwrap();
        router.logout("primary", || {}).await.unwrap();
        assert_eq!(
            AccountRouter::open(root.path())
                .await
                .unwrap()
                .account_count(),
            0
        );
        assert!(!root.path().join("auth.json").exists());
    }

    #[tokio::test]
    async fn logout_rejects_paths_and_preserves_other_slots() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        let original = fs::read(root.path().join("accounts/primary/auth.json")).unwrap();
        let router = AccountRouter::load(root.path()).await.unwrap();
        assert!(router.logout("../../.codex", || {}).await.is_err());
        assert_eq!(
            fs::read(root.path().join("accounts/primary/auth.json")).unwrap(),
            original
        );
    }

    /// A `credentials_changed` callback that counts its calls.
    fn counting(calls: &Arc<AtomicUsize>) -> impl FnOnce() + Send + 'static {
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    /// Yield to the task under test until it has reported `n` changes in all.
    async fn reported(calls: &AtomicUsize, n: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(AtomicOrdering::SeqCst) < n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the credential change is reported before the state lock");
    }

    /// A directory where `path` names a file: removing or replacing it fails.
    fn occupy(path: &Path) {
        let _ = fs::remove_file(path);
        fs::create_dir_all(path.join("occupied")).unwrap();
    }

    /// The account primary's credentials name right now.
    async fn primary_account(router: &AccountRouter) -> String {
        router.lease_slot("primary").await.unwrap().account_id
    }

    /// Primary's credentials are gone the moment its auth.json is, and the
    /// caller hears so then: neither a legacy cleanup nor a state write that
    /// fails afterwards, nor a cancellation at the state lock, swallows it.
    #[tokio::test]
    async fn logout_reports_the_removal_even_when_a_later_step_fails_or_is_cancelled() {
        for later in ["legacy cleanup", "state write", "cancellation"] {
            let root = tempdir().unwrap();
            write_account(root.path(), "primary", "acct-primary");
            let router = AccountRouter::load(root.path()).await.unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let ended = if later == "cancellation" {
                let held = router.state.lock().await;
                let task = tokio::spawn({
                    let router = router.clone();
                    let report = counting(&calls);
                    async move { router.logout("primary", report).await }
                });
                reported(&calls, 1).await;
                task.abort();
                let ended = task.await.unwrap_err();
                assert!(ended.is_cancelled());
                drop(held);
                ended.to_string()
            } else {
                occupy(&root.path().join(match later {
                    "legacy cleanup" => "auth.json",
                    _ => "router-state.json",
                }));
                let error = router
                    .logout("primary", counting(&calls))
                    .await
                    .unwrap_err();
                format!("{error:#}")
            };
            assert_eq!(calls.load(AtomicOrdering::SeqCst), 1, "{later}: {ended}");
            assert!(
                !root.path().join("accounts/primary/auth.json").exists(),
                "{later}"
            );
        }
    }

    /// A sign-out that fails before removing anything reports nothing. One
    /// that finds nothing to remove reports once it succeeds: the slot is
    /// signed out all the same.
    #[tokio::test]
    async fn logout_reports_nothing_before_a_removal_and_an_empty_slot_on_success() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(router.logout("tertiary", counting(&calls)).await.is_err());
        occupy(&root.path().join("accounts/secondary/auth.json"));
        assert!(router.logout("secondary", counting(&calls)).await.is_err());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);

        fs::remove_dir_all(root.path().join("accounts/secondary/auth.json")).unwrap();
        assert!(!router.logout("secondary", counting(&calls)).await.unwrap());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert!(root.path().join("accounts/primary/auth.json").is_file());
    }

    /// A sign-in changes the slot's credentials at one point: the install of
    /// the staged profile. The caller hears so then -- neither a state write
    /// that fails afterwards nor a cancellation at the state lock swallows it
    /// -- and never before: an OAuth failure, or a cancellation while the
    /// install still waits for the refresh lock, leaves the old account in
    /// place and reports nothing.
    #[tokio::test]
    async fn login_reports_an_installed_replacement_and_nothing_before_it() {
        use crate::core::catalog::test_support::staged_login;
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-first");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let sign_in = |report| {
            let router = router.clone();
            tokio::spawn(async move { router.login("primary", report).await })
        };

        router.fail_next_login(std::io::Error::other("the browser was closed"));
        let error = router.login("primary", counting(&calls)).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("browser was closed"),
            "{error:#}"
        );
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(primary_account(&router).await, "acct-first");

        // OAuth is over once the staged profile is taken; the install then
        // waits for the refresh lock held here, and the sign-in is cancelled.
        let primary = router.accounts.iter().find(|a| a.slot == "primary");
        let refresh = primary.unwrap().refresh_lock.lock().await;
        router.stage_next_login(staged_login("primary", "acct-second"));
        let task = sign_in(counting(&calls));
        tokio::time::timeout(Duration::from_secs(1), async {
            while router.staged_login.lock().unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the sign-in takes its staged profile");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(refresh);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(primary_account(&router).await, "acct-first");

        // Installed, then cancelled while the state lock is held here.
        let held = router.state.lock().await;
        router.stage_next_login(staged_login("primary", "acct-second"));
        let task = sign_in(counting(&calls));
        reported(&calls, 1).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(held);
        assert_eq!(primary_account(&router).await, "acct-second");

        // Installed, then the state write fails.
        occupy(&root.path().join("router-state.json"));
        router.stage_next_login(staged_login("primary", "acct-third"));
        let error = router.login("primary", counting(&calls)).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("account router state"),
            "{error:#}"
        );
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(primary_account(&router).await, "acct-third");
    }

    #[tokio::test]
    async fn quota_switch_is_global_sticky_and_persistent() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let exhausted = router.lease_active().await.unwrap();
        let standby = router.switch_after_quota(&exhausted).await.unwrap();
        assert_eq!(standby.slot, "secondary");
        assert_eq!(router.lease_active().await.unwrap().slot, "secondary");

        let reloaded = AccountRouter::load(root.path()).await.unwrap();
        assert_eq!(reloaded.lease_active().await.unwrap().slot, "secondary");
    }

    #[tokio::test]
    async fn a_deliberate_switch_moves_and_persists_the_active_slot() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        assert_eq!(router.active_slot().await, "primary");

        let moved = router.switch_to("secondary", |_| {}).await.unwrap();
        assert_eq!(moved.slot, "secondary");
        assert_eq!(router.active_slot().await, "secondary");

        let reloaded = AccountRouter::load(root.path()).await.unwrap();
        assert_eq!(reloaded.active_slot().await, "secondary");
    }

    #[tokio::test]
    async fn switching_to_the_active_slot_changes_nothing() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let same = router.switch_to("primary", |_| {}).await.unwrap();
        assert_eq!(same.slot, "primary");
        assert_eq!(router.active_slot().await, "primary");
    }

    #[tokio::test]
    async fn an_unknown_slot_is_named_rather_than_silently_ignored() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let error = match router.switch_to("tertiary", |_| {}).await {
            Ok(_) => panic!("an unconfigured slot was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("not configured"), "{error}");
        assert!(error.contains("primary"), "{error}");
    }

    #[tokio::test]
    async fn a_deliberate_switch_clears_the_slot_cooldown() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let exhausted = router.lease_active().await.unwrap();
        router.switch_after_quota(&exhausted).await.unwrap();
        assert_eq!(router.active_slot().await, "secondary");
        assert!(
            router
                .describe()
                .await
                .iter()
                .any(|view| view.slot == "primary" && view.cooldown_seconds_left > 0),
            "quota exhaustion must park the spent slot"
        );

        router.switch_to("primary", |_| {}).await.unwrap();
        let parked = router
            .describe()
            .await
            .into_iter()
            .find(|view| view.slot == "primary")
            .unwrap();
        assert!(parked.active);
        assert_eq!(parked.cooldown_seconds_left, 0);
    }

    /// What each `switched` callback saw: the slot it was told, the active
    /// slot persisted at that moment, and whether the state lock was held.
    type SwitchReports = Arc<std::sync::Mutex<Vec<(String, String, bool)>>>;

    fn recording(router: &AccountRouter, reports: &SwitchReports) -> impl FnOnce(&str) {
        let router = router.clone();
        let reports = reports.clone();
        move |slot: &str| {
            let persisted: serde_json::Value =
                serde_json::from_slice(&fs::read(router.state_file.as_ref()).unwrap()).unwrap();
            let locked = router.state.try_lock().is_err();
            reports.lock().unwrap().push((
                slot.to_string(),
                persisted["active"].as_str().unwrap_or_default().to_string(),
                locked,
            ));
        }
    }

    /// A deliberate switch -- to the active slot too -- reports the slot it
    /// selected from inside its commit: already persisted, and with the
    /// state lock still held, so no other switch can commit in between. One
    /// that moved nothing -- an unconfigured or unusable slot, a refused
    /// state write -- reports nothing.
    #[tokio::test]
    async fn a_switch_reports_its_target_inside_the_commit_and_only_once_it_moved() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let reports = SwitchReports::default();
        for slot in ["secondary", "secondary"] {
            router
                .switch_to(slot, recording(&router, &reports))
                .await
                .unwrap();
        }
        let inside = |slot: &str| (slot.to_string(), slot.to_string(), true);
        assert_eq!(
            *reports.lock().unwrap(),
            [inside("secondary"), inside("secondary")]
        );

        assert!(router
            .switch_to("tertiary", recording(&router, &reports))
            .await
            .is_err());
        let primary = root.path().join("accounts/primary/auth.json");
        let healthy = fs::read(&primary).unwrap();
        fs::write(&primary, b"not json at all").unwrap();
        assert!(router
            .switch_to("primary", recording(&router, &reports))
            .await
            .is_err());
        fs::write(&primary, healthy).unwrap();
        occupy(&root.path().join("router-state.json"));
        assert!(router
            .switch_to("primary", recording(&router, &reports))
            .await
            .is_err());
        assert_eq!(reports.lock().unwrap().len(), 2, "nothing moved");
        assert_eq!(router.active_slot().await, "secondary");
    }

    /// ЖИВОЙ СЛУЧАЙ 13.08.2026, ради которого написан lease_any: файл активного слота
    /// перестал читаться, каждый запрос умирал об одну и ту же аренду, процесс падал в
    /// цикл рестартов — при ЗДОРОВОМ втором слоте. Здесь охраняется противоположное:
    /// сломанный активный паркуется с TTL, аренда уходит в резерв, реле живёт.
    #[tokio::test]
    async fn a_broken_active_slot_fails_over_and_parks() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        assert_eq!(router.active_slot().await, "primary");

        fs::write(
            root.path()
                .join("accounts")
                .join("primary")
                .join("auth.json"),
            b"not json at all",
        )
        .unwrap();

        let lease = router
            .lease_any()
            .await
            .expect("резерв обязан спасти аренду");
        assert_eq!(lease.slot, "secondary");
        assert_eq!(router.active_slot().await, "secondary");
        let parked = router
            .describe()
            .await
            .into_iter()
            .find(|view| view.slot == "primary")
            .unwrap();
        assert!(!parked.active);
        assert!(
            parked.cooldown_seconds_left > 0,
            "сломанный слот обязан быть виден запаркованным, а не здоровым"
        );
    }

    /// Тупик из отчёта Кодекса: R2+R4 в связке умели запарковать ОБА слота и уронить
    /// процесс навсегда. Пол: последний живой слот не паркуется никогда — отказ уходит
    /// наружу ошибкой, а состояние не трогается.
    #[tokio::test]
    async fn the_last_slot_is_never_parked() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        let router = AccountRouter::load(root.path()).await.unwrap();

        fs::write(
            root.path()
                .join("accounts")
                .join("primary")
                .join("auth.json"),
            b"not json at all",
        )
        .unwrap();

        // AccountLease намеренно без Debug (токены не должны уметь печататься),
        // поэтому не expect_err, а match.
        let error = match router.lease_any().await {
            Ok(_) => panic!("единственный сломанный слот не может дать аренду"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no standby"), "{error:#}");
        let view = router
            .describe()
            .await
            .into_iter()
            .find(|view| view.slot == "primary")
            .unwrap();
        assert_eq!(
            view.cooldown_seconds_left, 0,
            "последний слот запаркован — процессу некуда жить"
        );
        assert_eq!(router.active_slot().await, "primary");
    }

    /// Старт переживает сломанный АКТИВНЫЙ профиль: 13.08 load() падал и ронял процесс,
    /// хотя рядом лежал здоровый второй слот.
    #[tokio::test]
    async fn startup_survives_a_broken_active_profile() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        // Ломаем primary ДО первой загрузки: state ещё не существует, active = primary.
        fs::write(
            root.path()
                .join("accounts")
                .join("primary")
                .join("auth.json"),
            b"not json at all",
        )
        .unwrap();
        let router = AccountRouter::load(root.path())
            .await
            .expect("реле обязано подняться на здоровом резерве");
        assert_eq!(router.active_slot().await, "secondary");
    }

    #[tokio::test]
    async fn cancellation_wins_at_account_state_commit_boundary() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let exhausted = router.lease_active().await.unwrap();
        let cancellation = DownstreamCancellation::default();

        // This is a completed cancellation, not tx.closed() cancelling an outer
        // future. The guarded router operation itself must reject its commit.
        cancellation.cancel();
        let error = match router
            .switch_after_quota_for_downstream(&exhausted, &cancellation)
            .await
        {
            Ok(_) => panic!("a commit was allowed after cancellation completed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert_eq!(router.active_slot().await, "primary");
        assert!(router
            .describe()
            .await
            .iter()
            .all(|slot| slot.cooldown_seconds_left == 0));

        // The persistent state must be unchanged as well as the in-memory copy.
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("router-state.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["active"], "primary");
        assert_eq!(persisted["exhausted_until"], json!({}));
    }

    #[tokio::test]
    async fn commit_permit_wins_before_cancellation_and_makes_cancellation_wait() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let exhausted = router.lease_active().await.unwrap();
        let cancellation = DownstreamCancellation::default();
        let (gate, commit_reached, release_commit) = TestCommitGate::new();
        router.gate_next_commit(gate);

        let router_for_commit = router.clone();
        let cancellation_for_commit = cancellation.clone();
        let commit_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(
                router_for_commit
                    .switch_after_quota_for_downstream(&exhausted, &cancellation_for_commit),
            )
        });
        commit_reached
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("commit must acquire its permit and reach the deterministic boundary");

        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let cancellation_for_thread = cancellation.clone();
        let cancel_thread = std::thread::spawn(move || {
            cancellation_for_thread.cancel();
            cancelled_tx.send(()).unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while cancellation.cancel_attempts() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation must attempt to take the write side of the permit");
        assert!(
            cancelled_rx.try_recv().is_err(),
            "cancellation completed while the commit still held its permit"
        );
        let persisted_before_release: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("router-state.json")).unwrap())
                .unwrap();
        assert_eq!(persisted_before_release["active"], "primary");

        release_commit.send(()).unwrap();
        let standby = commit_thread.join().unwrap().unwrap();
        assert_eq!(standby.slot, "secondary");
        cancelled_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("cancellation must complete immediately after the permit is released");
        cancel_thread.join().unwrap();
        assert_eq!(router.active_slot().await, "secondary");
        assert!(router
            .describe()
            .await
            .iter()
            .any(|slot| slot.slot == "primary" && slot.cooldown_seconds_left > 0));
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("router-state.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["active"], "secondary");
        assert!(persisted["exhausted_until"]["primary"].as_i64().is_some());
    }

    #[tokio::test]
    async fn describe_names_every_slot_and_which_one_is_live() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let views = router.describe().await;
        assert_eq!(views.len(), 2);
        assert_eq!(
            views.iter().filter(|view| view.active).count(),
            1,
            "exactly one slot is live at a time"
        );
    }

    #[tokio::test]
    async fn duplicate_subscription_is_refused() {
        let root = tempdir().unwrap();
        write_account(root.path(), "primary", "same-account");
        write_account(root.path(), "secondary", "same-account");
        let router = AccountRouter::load(root.path()).await.unwrap();
        let exhausted = router.lease_active().await.unwrap();
        let error = match router.switch_after_quota(&exhausted).await {
            Ok(_) => panic!("duplicate subscription was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exhausted ChatGPT account"));
        assert_eq!(router.active_slot().await, "primary");
    }
}
