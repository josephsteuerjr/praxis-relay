//! Live model catalog: what the Codex backend says it serves today, not what a
//! constant said it served on the day someone last edited it.
//!
//! `GET https://chatgpt.com/backend-api/codex/models?client_version=X` answers
//! with every model the account can use: slug, visibility (`list` / `hide`),
//! context window, input modalities, reasoning levels, priority. The relay asks
//! once per `RELAY_MODELS_TTL` seconds (default 600), serves `/v1/models` from
//! the answer and validates chat requests against it. A request for a slug the
//! cached answer does not know forces one early refresh, so a model released an
//! hour ago is usable the moment someone asks for it.
//!
//! Degradation is explicit, never silent: when the backend cannot be asked (no
//! subscription, network, 5xx) the last good answer keeps serving; with no
//! answer ever, the static [`FALLBACK_MODELS`] list stands in. `/health` and
//! `/v1/models` report which of the three is live.
//!
//! The backend answers per account, and two subscriptions need not be offered
//! the same models, so every answer is bound to the slot whose credentials
//! fetched it. That is the lease's own slot: discovery leases with `lease_any`,
//! which may fail over to the standby (and park the active slot) on the way,
//! so "the slot that was active" would be a guess. A deliberate switch to
//! another slot invalidates the answer: the next read refreshes it at once, and
//! until a refresh succeeds the old answer serves only as the last good one --
//! `stale`, under the slot that really fetched it. Automatic failover
//! invalidates nothing: the TTL rebinds the answer, and the slot it reports
//! stays true in the meantime.
//!
//! Signing a slot out, or in again, retires the answer its old credentials
//! fetched: it is forgotten, not kept as `stale`, because those credentials
//! are no longer this relay's and the slot would now name someone else, or no
//! one. The next read asks at once, past any retry cooldown -- a cold start
//! with no subscription to ask arms one moments before the first sign-in.
//!
//! Why this exists: the static list was edited for 5.6 (three times), for
//! Spark, and would have been edited again for gpt-6-astra -- a rebuild and a
//! deploy each time for a fact the backend already publishes. Note the backend
//! gates the catalog on the client version it sees: astra carries
//! `minimal_client_version: 0.153.0`, so the version the relay presents
//! (`RELAY_CODEX_VERSION`) decides what the catalog lists.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::core::account_router::AccountRouter;
use crate::core::chat_completions::{codex_cli_version, codex_user_agent, CODEX_ORIGINATOR};
use crate::core::models::{CatalogProvenance, Model, ModelList, ServiceTier, FALLBACK_MODELS};

const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
pub const RELAY_MODELS_TTL_ENV: &str = "RELAY_MODELS_TTL";
pub const RELAY_MODEL_DISCOVERY_ENV: &str = "RELAY_MODEL_DISCOVERY";
pub const RELAY_EXTRA_MODELS_ENV: &str = "RELAY_EXTRA_MODELS";
const DEFAULT_TTL: Duration = Duration::from_secs(600);
/// After a refresh attempt (failed or not) upstream is left alone at least this long,
/// so a burst of requests for a bogus slug cannot become a burst of catalog fetches.
const RETRY_COOLDOWN: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// The live answer is ~370 KiB (each model carries its full base instructions).
const MAX_CATALOG_BODY_BYTES: usize = 8 * 1024 * 1024;

/// One model as the backend describes it, reduced to what the relay uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    pub display_name: Option<String>,
    /// `visibility == "list"`: advertised on `/v1/models`. Hidden slugs are
    /// still accepted when asked for by name -- the backend serves them.
    pub listed: bool,
    pub priority: i64,
    pub context_window: Option<u64>,
    pub max_context_window: Option<u64>,
    pub input_modalities: Vec<String>,
    pub reasoning_efforts: Vec<String>,
    pub service_tiers: Option<Vec<ServiceTier>>,
    pub additional_speed_tiers: Option<Vec<String>>,
    pub default_reasoning_effort: Option<String>,
    pub minimal_client_version: Option<String>,
    pub upgrade: Option<String>,
}

impl CatalogModel {
    /// A slug with no metadata: what the fallback list and RELAY_EXTRA_MODELS produce.
    fn bare(slug: &str, priority: i64) -> Self {
        CatalogModel {
            slug: slug.to_string(),
            display_name: None,
            listed: true,
            priority,
            context_window: None,
            max_context_window: None,
            input_modalities: Vec::new(),
            reasoning_efforts: Vec::new(),
            service_tiers: None,
            additional_speed_tiers: None,
            default_reasoning_effort: None,
            minimal_client_version: None,
            upgrade: None,
        }
    }
}

/// Where the models being served came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogSource {
    /// A backend answer younger than the TTL.
    Live,
    /// A backend answer past the TTL, or fetched through a slot a deliberate
    /// switch has since moved away from, that has not been refreshed.
    Stale,
    /// No backend answer held -- none yet, or a logout or login retired the
    /// last one: [`FALLBACK_MODELS`] (+ extras).
    Fallback,
    /// Discovery switched off by `RELAY_MODEL_DISCOVERY=off`.
    Static,
}

impl CatalogSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CatalogSource::Live => "live",
            CatalogSource::Stale => "stale",
            CatalogSource::Fallback => "fallback",
            CatalogSource::Static => "static",
        }
    }
}

/// The catalog as of one moment: models plus their provenance.
#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub models: Vec<CatalogModel>,
    pub source: CatalogSource,
    /// The slot whose lease fetched the backend answer being served; `None`
    /// when the static list is serving, which no subscription produced.
    pub slot: Option<String>,
}

impl CatalogSnapshot {
    pub fn find(&self, slug: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|m| m.slug == slug)
    }

    pub fn listed_slugs(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.listed)
            .map(|m| m.slug.clone())
            .collect()
    }
}

/// One backend answer, bound to where it came from.
struct CatalogEntry {
    models: Vec<CatalogModel>,
    /// The slot whose lease fetched `models`: the lease's own, never the active
    /// slot read before or after the fetch.
    slot: String,
    fetched: Instant,
    /// A deliberate switch has since selected another slot. Still the last good
    /// answer, but no longer `live`, and due for a refresh.
    superseded: bool,
}

struct CatalogState {
    /// None until the first successful fetch, and again once a logout or
    /// login retires the credentials that fetched it.
    entry: Option<CatalogEntry>,
    last_attempt: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Clone)]
pub struct ModelCatalog {
    inner: Arc<Mutex<CatalogState>>,
    /// The slot the latest deliberate switch selected, until the next reader
    /// applies it under `inner`. A fetch holds `inner` for up to
    /// REQUEST_TIMEOUT, and a switch must not wait behind it. Written from
    /// inside the router's commit, so "latest" is the order the switches
    /// took effect in.
    pending_switch: Arc<std::sync::Mutex<Option<String>>>,
    /// Slots whose credentials a logout removed or a login replaced since the
    /// last reader looked; recorded and applied like `pending_switch`.
    pending_credentials: Arc<std::sync::Mutex<BTreeSet<String>>>,
    ttl: Duration,
    discovery: bool,
    extra: Vec<String>,
    /// The backend endpoint; tests point it at a local fixture.
    url: Arc<str>,
}

impl ModelCatalog {
    /// Configure from the environment:
    /// * `RELAY_MODELS_TTL` -- seconds between catalog refreshes (default 600);
    /// * `RELAY_MODEL_DISCOVERY` -- `off`/`0`/`false` serves the static list only;
    /// * `RELAY_EXTRA_MODELS` -- comma-separated slugs always accepted and advertised
    ///   on top of whatever the catalog says (a slug the backend hides, or one it
    ///   lists only for a client version the relay does not claim).
    pub fn from_env() -> Self {
        let ttl = std::env::var(RELAY_MODELS_TTL_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TTL);
        let discovery = !matches!(
            std::env::var(RELAY_MODEL_DISCOVERY_ENV)
                .ok()
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref(),
            Some("off") | Some("0") | Some("false") | Some("no")
        );
        let extra = parse_extra_models(&std::env::var(RELAY_EXTRA_MODELS_ENV).unwrap_or_default());
        Self::with(ttl, discovery, extra, CODEX_MODELS_URL)
    }

    /// The static list only, never a network call: what tests and
    /// `RELAY_MODEL_DISCOVERY=off` deployments run on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn static_only() -> Self {
        Self::with(DEFAULT_TTL, false, Vec::new(), CODEX_MODELS_URL)
    }

    fn with(ttl: Duration, discovery: bool, extra: Vec<String>, url: &str) -> Self {
        ModelCatalog {
            inner: Arc::new(Mutex::new(CatalogState {
                entry: None,
                last_attempt: None,
                last_error: None,
            })),
            pending_switch: Arc::new(std::sync::Mutex::new(None)),
            pending_credentials: Arc::new(std::sync::Mutex::new(BTreeSet::new())),
            ttl,
            discovery,
            extra,
            url: Arc::from(url),
        }
    }

    /// A discovering catalog already holding an answer `slot` fetched just now.
    #[cfg(test)]
    fn preloaded(slot: &str, models: Vec<CatalogModel>, ttl: Duration) -> Self {
        let catalog = Self::with(ttl, true, Vec::new(), CODEX_MODELS_URL);
        catalog
            .inner
            .try_lock()
            .expect("a new catalog is not shared yet")
            .entry = Some(CatalogEntry {
            models,
            slot: slot.to_string(),
            fetched: Instant::now(),
            superseded: false,
        });
        catalog
    }

    /// A deliberate switch selected `slot`. An answer fetched through any other
    /// slot no longer describes the selected subscription: the next read
    /// refreshes it regardless of the TTL and the retry cooldown (an explicit
    /// choice outranks both), and until a refresh succeeds it serves only as the
    /// last good answer, `stale` and under the slot that fetched it. An answer
    /// `slot` itself fetched stays exactly as it is. With no answer yet (no
    /// attempt has succeeded), the next read likewise asks at once, past any
    /// cooldown a failed attempt armed; until one succeeds the built-in list
    /// serves as `fallback`.
    ///
    /// Never waits: the choice is recorded here and applied by the next reader
    /// (`/health` included), so a switch cannot stall behind a fetch in flight
    /// -- nor deadlock with one: the router calls this from inside its commit
    /// (the `switched` callback of `AccountRouter::switch_to`), under the
    /// state lock a fetch holding `inner` may be waiting on to lease.
    /// Recording there, not once the switch has returned, keeps overlapping
    /// switches in the order they took effect: the slot recorded last is the
    /// slot left active, never an earlier switch's that landed late.
    pub fn invalidate_for_switch(&self, slot: &str) {
        *self
            .pending_switch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(slot.to_string());
    }

    fn apply_pending_switch(&self, state: &mut CatalogState) {
        let selected = self
            .pending_switch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(selected) = selected else {
            return;
        };
        let Some(entry) = state.entry.as_mut() else {
            // No answer to demote, but a failed attempt (say, a cold start
            // through the slot just left) may have armed the retry cooldown,
            // and the switch outranks it all the same: the next read asks at once.
            state.last_attempt = None;
            return;
        };
        // Assigned, not accumulated: switching back to the slot that fetched the
        // answer, before anything refreshed it, makes it that slot's answer again.
        entry.superseded = entry.slot != selected;
        if entry.superseded {
            state.last_attempt = None;
        }
    }

    /// A logout removed `slot`'s credentials, or a login replaced them. An
    /// answer `slot` fetched goes with the credentials that fetched it: it is
    /// forgotten, not demoted to `stale` as after a switch, because they are
    /// no longer this relay's and `slot` would now name other credentials, or
    /// none. Until a refresh succeeds, the built-in list serves as `fallback`.
    /// An answer another slot fetched stays as it is. Either way the retry
    /// cooldown is lifted -- new or fewer credentials are reason to ask again
    /// -- so the next read that is due to ask (after a forgotten answer or a
    /// failed cold start, every read is) asks at once; one ask per sign-in or
    /// sign-out cannot become a burst.
    ///
    /// Never waits, like [`Self::invalidate_for_switch`]: the next reader
    /// (`/health` included) applies it. That is what lets the router report
    /// the change from under the slot's refresh lock the moment the credential
    /// files change, so a logout or login that fails or is cancelled after
    /// that still retires the answer.
    pub fn invalidate_for_credential_change(&self, slot: &str) {
        self.pending_credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(slot.to_string());
    }

    fn apply_pending_credentials(&self, state: &mut CatalogState) {
        let changed = std::mem::take(
            &mut *self
                .pending_credentials
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if changed.is_empty() {
            return;
        }
        if state
            .entry
            .as_ref()
            .is_some_and(|entry| changed.contains(&entry.slot))
        {
            state.entry = None;
        }
        state.last_attempt = None;
    }

    /// Apply what switches, logouts and logins recorded since the last reader.
    /// The order is immaterial: a credential change only ever removes its own
    /// slot's answer, and a switch only relabels whatever answer is left.
    fn apply_pending(&self, state: &mut CatalogState) {
        self.apply_pending_credentials(state);
        self.apply_pending_switch(state);
    }

    /// The current catalog, refreshed first when older than the TTL.
    pub async fn snapshot(&self, router: &AccountRouter, client: &Client) -> CatalogSnapshot {
        self.snapshot_inner(router, client, false).await
    }

    /// Is `model` something this relay will forward? Listed and hidden catalog
    /// slugs and RELAY_EXTRA_MODELS all count. A miss forces one early refresh
    /// (rate-limited) so a slug released after the last fetch is not refused
    /// for the rest of the TTL.
    pub async fn is_known(&self, model: &str, router: &AccountRouter, client: &Client) -> bool {
        if self.snapshot(router, client).await.find(model).is_some() {
            return true;
        }
        if !self.discovery {
            return false;
        }
        self.snapshot_inner(router, client, true)
            .await
            .find(model)
            .is_some()
    }

    /// The `/v1/models` document: listed models in backend priority order,
    /// each carrying the metadata the backend published for it.
    pub async fn model_list(&self, router: &AccountRouter, client: &Client) -> ModelList {
        let snapshot = self.snapshot(router, client).await;
        model_list_from(&snapshot)
    }

    /// For `/health`: which source is serving, which slot's lease fetched it
    /// (`null` for the static list) and how old it is. No slugs -- the list has
    /// its own endpoint.
    pub async fn status(&self) -> Value {
        let mut guard = self.inner.lock().await;
        self.apply_pending(&mut guard);
        let source = self.source_of(&guard);
        let entry = guard.entry.as_ref();
        json!({
            "source": source.as_str(),
            "slot": entry.map(|entry| entry.slot.as_str()),
            "models": entry.map_or(FALLBACK_MODELS.len(), |entry| entry.models.len()),
            "extra_models": self.extra.len(),
            "fetched_seconds_ago": entry.map(|entry| entry.fetched.elapsed().as_secs()),
            "ttl_seconds": self.ttl.as_secs(),
            "last_error": guard.last_error,
        })
    }

    fn source_of(&self, state: &CatalogState) -> CatalogSource {
        if !self.discovery {
            return CatalogSource::Static;
        }
        match &state.entry {
            None => CatalogSource::Fallback,
            Some(entry) if !entry.superseded && entry.fetched.elapsed() < self.ttl => {
                CatalogSource::Live
            }
            Some(_) => CatalogSource::Stale,
        }
    }

    async fn snapshot_inner(
        &self,
        router: &AccountRouter,
        client: &Client,
        force: bool,
    ) -> CatalogSnapshot {
        // The lock is held across the fetch on purpose: a burst of callers
        // becomes one upstream request, the rest read the answer.
        let mut guard = self.inner.lock().await;
        self.apply_pending(&mut guard);
        if self.discovery {
            let expired = guard
                .entry
                .as_ref()
                .is_none_or(|entry| entry.superseded || entry.fetched.elapsed() >= self.ttl);
            let cooling = guard
                .last_attempt
                .is_some_and(|at| at.elapsed() < RETRY_COOLDOWN);
            if (expired || force) && !cooling {
                guard.last_attempt = Some(Instant::now());
                match fetch_catalog(router, client, &self.url).await {
                    Ok((slot, models)) => {
                        info!(
                            "model catalog refreshed through slot {}: {} models ({} listed) for client {}",
                            slot,
                            models.len(),
                            models.iter().filter(|m| m.listed).count(),
                            codex_cli_version()
                        );
                        guard.entry = Some(CatalogEntry {
                            models,
                            slot,
                            fetched: Instant::now(),
                            superseded: false,
                        });
                        guard.last_error = None;
                    }
                    Err(error) => {
                        warn!("model catalog refresh failed: {}", error);
                        guard.last_error = Some(error.to_string());
                    }
                }
            }
        }
        let source = self.source_of(&guard);
        let (base, slot) = match &guard.entry {
            Some(entry) => (entry.models.clone(), Some(entry.slot.clone())),
            None => (fallback_models(), None),
        };
        drop(guard);
        CatalogSnapshot {
            models: with_extras(base, &self.extra),
            source,
            slot,
        }
    }
}

/// The static safety net as catalog entries, in the constant's order.
fn fallback_models() -> Vec<CatalogModel> {
    FALLBACK_MODELS
        .iter()
        .enumerate()
        .map(|(index, slug)| CatalogModel::bare(slug, index as i64))
        .collect()
}

/// Append RELAY_EXTRA_MODELS that the catalog does not already carry.
fn with_extras(mut models: Vec<CatalogModel>, extra: &[String]) -> Vec<CatalogModel> {
    let mut next_priority = models.iter().map(|m| m.priority).max().unwrap_or(0) + 1;
    for slug in extra {
        if models.iter().any(|m| &m.slug == slug) {
            continue;
        }
        models.push(CatalogModel::bare(slug, next_priority));
        next_priority += 1;
    }
    models
}

/// Trim, drop empties, drop repeats, keep order -- the whole RELAY_EXTRA_MODELS contract.
pub fn parse_extra_models(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for slug in raw.split(',') {
        let slug = slug.trim();
        if slug.is_empty() || out.iter().any(|seen| seen == slug) {
            continue;
        }
        out.push(slug.to_string());
    }
    out
}

/// The reasoning effort to send for a model whose supported levels the catalog
/// lists.
///
/// Measured 2026-09-04: gpt-6-astra answers `400 unsupported_value` to
/// `reasoning.effort: "none"` (and `"minimal"`); its levels are low..max. The
/// relay's default effort is `none`, so every astra call failed once and was
/// rescued by the conservative retry -- which drops the knobs *and* swaps the
/// 350-character instructions for the full Codex preamble: 5016 prompt tokens
/// for a "pong" against 88 through terra. Clamping here, before anything goes
/// upstream, keeps both the call count and the instructions honest.
///
/// Rules: no known levels -> pass through untouched; a supported value stays;
/// a value on the ladder but unsupported -> the nearest supported level above
/// it, else the highest supported; a value not on the ladder at all -> the
/// model's own default, else its lowest level. `None` (no effort asked
/// anywhere) stays `None` -- the backend default is the model's own.
pub fn clamp_effort(
    requested: Option<&str>,
    supported: &[String],
    model_default: Option<&str>,
) -> Option<String> {
    let requested = requested?;
    if supported.is_empty() || supported.iter().any(|s| s == requested) {
        return Some(requested.to_string());
    }
    let ladder = crate::core::chat_completions::REASONING_EFFORTS;
    let rank = |effort: &str| ladder.iter().position(|e| *e == effort);
    let mut ranked: Vec<(usize, &str)> = supported
        .iter()
        .filter_map(|s| rank(s).map(|r| (r, s.as_str())))
        .collect();
    ranked.sort_unstable();
    match rank(requested) {
        Some(want) => ranked
            .iter()
            .find(|(r, _)| *r >= want)
            .or_else(|| ranked.last())
            .map(|(_, s)| (*s).to_string()),
        None => model_default
            .filter(|d| supported.iter().any(|s| s == d))
            .map(str::to_string)
            .or_else(|| ranked.first().map(|(_, s)| (*s).to_string())),
    }
    .or_else(|| Some(requested.to_string()))
}

/// One backend answer, and the slot whose lease fetched it.
async fn fetch_catalog(
    router: &AccountRouter,
    client: &Client,
    url: &str,
) -> Result<(String, Vec<CatalogModel>)> {
    // lease_any, as limits does: a parked active slot must not blind discovery
    // while a healthy standby could answer. The answer is then the standby's,
    // which is why it is bound to the lease's slot, not to the active one.
    let account = router
        .lease_any()
        .await
        .context("no subscription available to read the model catalog")?;
    let response = client
        .get(url)
        .query(&[("client_version", codex_cli_version())])
        .bearer_auth(&account.access_token)
        .header("ChatGPT-Account-ID", &account.account_id)
        .header(reqwest::header::USER_AGENT, codex_user_agent())
        .header("originator", CODEX_ORIGINATOR)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("model catalog request failed")?;
    let status = response.status();
    if !status.is_success() {
        // The body can carry account details.  Never reflect it.
        bail!("model catalog endpoint returned {status}");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BODY_BYTES as u64)
    {
        bail!("model catalog response is unexpectedly large");
    }
    let body = response
        .bytes()
        .await
        .context("failed to read model catalog response")?;
    if body.len() > MAX_CATALOG_BODY_BYTES {
        bail!("model catalog response is unexpectedly large");
    }
    let raw: Value = serde_json::from_slice(&body).context("invalid model catalog JSON")?;
    let models = parse_catalog(&raw)?;
    Ok((account.slot, models))
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn tier_identifier(value: &Value) -> Option<String> {
    value.as_str().filter(|value| {
        !value.is_empty() && value.len() <= 64
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }).map(str::to_owned)
}

fn tier_description(value: Option<&Value>, limit: usize) -> Option<String> {
    value.and_then(Value::as_str)
        .map(|text| text.chars().filter(|ch| !ch.is_control()).take(limit).collect::<String>())
        .filter(|text| !text.is_empty())
}

fn service_tiers(value: Option<&Value>) -> Option<Vec<ServiceTier>> {
    let values = value?.as_array()?;
    let tiers: Vec<_> = values.iter().filter_map(|tier| Some(ServiceTier {
        id: tier_identifier(tier.get("id")?)?,
        name: tier_description(tier.get("name"), 128),
        description: tier_description(tier.get("description"), 2048),
    })).collect();
    (values.is_empty() || !tiers.is_empty()).then_some(tiers)
}

fn additional_speed_tiers(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    let tiers: Vec<_> = values.iter().filter_map(tier_identifier).collect();
    (values.is_empty() || !tiers.is_empty()).then_some(tiers)
}

/// Reduce the backend document to [`CatalogModel`]s, sorted by the backend's
/// own priority (ties keep document order). Entries without a slug are skipped;
/// an answer with no usable entry is an error, not an empty catalog -- an empty
/// catalog would refuse every request while looking healthy.
pub fn parse_catalog(raw: &Value) -> Result<Vec<CatalogModel>> {
    let items = raw
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("model catalog payload has no `models` array"))?;
    let mut out: Vec<CatalogModel> = Vec::new();
    for item in items {
        let Some(slug) = item
            .get("slug")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|slug| !slug.is_empty())
        else {
            continue;
        };
        if out.iter().any(|m| m.slug == slug) {
            continue;
        }
        let listed = item
            .get("visibility")
            .and_then(Value::as_str)
            .is_none_or(|v| v == "list");
        out.push(CatalogModel {
            slug: slug.to_string(),
            display_name: item
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            listed,
            priority: item
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX),
            context_window: item.get("context_window").and_then(Value::as_u64),
            max_context_window: item.get("max_context_window").and_then(Value::as_u64),
            input_modalities: string_list(item.get("input_modalities")),
            service_tiers: service_tiers(item.get("service_tiers")),
            additional_speed_tiers: additional_speed_tiers(item.get("additional_speed_tiers")),
            reasoning_efforts: item
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|level| level.get("effort").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            default_reasoning_effort: item
                .get("default_reasoning_level")
                .and_then(Value::as_str)
                .map(str::to_string),
            minimal_client_version: item
                .get("minimal_client_version")
                .and_then(Value::as_str)
                .map(str::to_string),
            upgrade: item
                .get("upgrade")
                .and_then(|u| u.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if out.is_empty() {
        bail!("model catalog payload lists no models");
    }
    out.sort_by_key(|m| m.priority);
    Ok(out)
}

/// Build the OpenAI-shaped `/v1/models` document from a snapshot: listed models
/// only, in priority order, with the backend's metadata beside the four
/// standard fields. `meta.n_ctx_train` / `context_length` are the llama.cpp
/// spellings that local-model client lanes read. `relay_catalog` carries the
/// snapshot's own provenance: its source and the slot that fetched it.
pub fn model_list_from(snapshot: &CatalogSnapshot) -> ModelList {
    let created = chrono::Utc::now().timestamp();
    ModelList {
        object: "list".to_string(),
        data: snapshot
            .models
            .iter()
            .filter(|m| m.listed)
            .map(|m| Model {
                id: m.slug.clone(),
                object: "model".to_string(),
                created,
                owned_by: "chatgpt".to_string(),
                display_name: m.display_name.clone(),
                context_window: m.context_window,
                context_length: m.context_window,
                max_context_window: m.max_context_window,
                meta: m
                    .context_window
                    .map(|window| json!({ "n_ctx_train": window })),
                input_modalities: m.input_modalities.clone(),
                reasoning_efforts: m.reasoning_efforts.clone(),
                service_tiers: m.service_tiers.clone(),
                additional_speed_tiers: m.additional_speed_tiers.clone(),
                default_reasoning_effort: m.default_reasoning_effort.clone(),
                upgrade: m.upgrade.clone(),
            })
            .collect(),
        relay_catalog: Some(CatalogProvenance {
            source: snapshot.source.as_str().to_string(),
            slot: snapshot.slot.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{model_for, staged_login, write_account, CatalogFixture};
    use super::*;

    fn fixture() -> Value {
        json!({
            "models": [
                {"slug": "gpt-reserve", "display_name": "GPT-Reserve", "visibility": "hide", "priority": 3,
                 "context_window": 272000, "input_modalities": ["text", "image"],
                 "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}]},
                {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "visibility": "list", "priority": 6,
                 "context_window": 272000, "max_context_window": 872000,
                 "input_modalities": ["text", "image"], "default_reasoning_level": "medium",
                 "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}, {"effort": "ultra"}]},
                {"slug": "gpt-6-astra", "display_name": "GPT-6-Astra", "visibility": "list", "priority": 1,
                 "context_window": 272000, "minimal_client_version": "0.153.0",
                 "input_modalities": ["text", "image"], "upgrade": null,
                 "supported_reasoning_levels": [{"effort": "low"}, {"effort": "max"}]},
                {"slug": "gpt-5.4", "visibility": "list", "priority": 16, "upgrade": {"model": "gpt-5.6-terra"}},
                {"visibility": "list", "priority": 0},
                {"slug": "   ", "visibility": "list"},
                {"slug": "gpt-5.4", "visibility": "list", "priority": 99}
            ]
        })
    }

    #[test]
    fn parses_the_backend_document_in_priority_order() {
        let models = parse_catalog(&fixture()).unwrap();
        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        // Sorted by priority; the slug-less and blank entries are skipped; the
        // duplicate gpt-5.4 keeps its first occurrence.
        assert_eq!(
            slugs,
            ["gpt-6-astra", "gpt-reserve", "gpt-5.6-sol", "gpt-5.4"]
        );
        let astra = &models[0];
        assert!(astra.listed);
        assert_eq!(astra.display_name.as_deref(), Some("GPT-6-Astra"));
        assert_eq!(astra.context_window, Some(272000));
        assert_eq!(astra.minimal_client_version.as_deref(), Some("0.153.0"));
        assert_eq!(astra.reasoning_efforts, ["low", "max"]);
        assert_eq!(astra.upgrade, None);
        assert!(!models[1].listed, "hidden stays hidden");
        assert_eq!(models[2].max_context_window, Some(872000));
        assert_eq!(
            models[2].default_reasoning_effort.as_deref(),
            Some("medium")
        );
        assert_eq!(models[3].upgrade.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(models[3].priority, 16);
    }

    #[test]
    fn speed_capabilities_keep_advertised_missing_and_empty_distinct() {
        let raw = json!({"models":[
            {"slug":"fast-capable","service_tiers":[{"id":"priority","name":"Fast", "description":"Faster processing", "private_unknown":"drop"}], "additional_speed_tiers":["fast"]},
            {"slug":"unsupported","service_tiers":[],"additional_speed_tiers":[]},
            {"slug":"unknown"},
            {"slug":"malformed","service_tiers":[{"id":"bad\nidentifier"}],"additional_speed_tiers":[false]},
            {"slug":"sanitized","service_tiers":[{"id":"priority","name":"Fast\u{0}", "description":"Faster\u{1} processing"}]}
        ]});
        let snapshot = CatalogSnapshot { models: parse_catalog(&raw).unwrap(), source: CatalogSource::Live, slot: Some("primary".to_string()) };
        let document = serde_json::to_value(model_list_from(&snapshot)).unwrap();
        let data = document["data"].as_array().unwrap();
        assert_eq!(data[0]["service_tiers"],json!([{"id":"priority","name":"Fast","description":"Faster processing"}]));
        assert_eq!(data[0]["additional_speed_tiers"],json!(["fast"]));
        assert_eq!(data[1]["service_tiers"],json!([]));
        assert_eq!(data[1]["additional_speed_tiers"],json!([]));
        for index in [2,3] {
            assert!(data[index].get("service_tiers").is_none());
            assert!(data[index].get("additional_speed_tiers").is_none());
        }
        assert_eq!(data[4]["service_tiers"][0]["name"],"Fast");
        assert_eq!(data[4]["service_tiers"][0]["description"],"Faster processing");
        assert!(model_list_from(&CatalogSnapshot { models:fallback_models(),source:CatalogSource::Fallback,slot:None }).data
            .iter().all(|model| model.service_tiers.is_none() && model.additional_speed_tiers.is_none()));
    }

    #[test]
    fn an_answer_without_models_is_an_error_not_an_empty_catalog() {
        assert!(parse_catalog(&json!({"models": []})).is_err());
        assert!(parse_catalog(&json!({"models": [{"visibility": "list"}]})).is_err());
        assert!(parse_catalog(&json!({"error": "nope"})).is_err());
        assert!(parse_catalog(&json!([])).is_err());
    }

    #[test]
    fn models_document_lists_only_visible_models_with_metadata() {
        let snapshot = CatalogSnapshot {
            models: parse_catalog(&fixture()).unwrap(),
            source: CatalogSource::Live,
            slot: Some("secondary".to_string()),
        };
        let list = model_list_from(&snapshot);
        assert_eq!(list.object, "list");
        let ids: Vec<&str> = list.data.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.4"]);
        let astra = &list.data[0];
        assert_eq!(astra.object, "model");
        assert_eq!(astra.owned_by, "chatgpt");
        assert_eq!(astra.context_window, Some(272000));
        assert_eq!(astra.context_length, Some(272000));
        assert_eq!(astra.meta.as_ref().unwrap()["n_ctx_train"], 272000);
        assert_eq!(astra.input_modalities, ["text", "image"]);
        // Serialized shape: the four OpenAI fields always, metadata only when known.
        let value = serde_json::to_value(&list).unwrap();
        assert_eq!(value["data"][0]["id"], "gpt-6-astra");
        assert_eq!(value["data"][0]["display_name"], "GPT-6-Astra");
        assert!(value["data"][2].get("display_name").is_none());
        assert!(value["data"][2].get("meta").is_none());
        assert!(value["data"][2].get("input_modalities").is_none());
        assert_eq!(value["data"][2]["upgrade"], "gpt-5.6-terra");
        // Provenance rides beside the OpenAI fields, never inside an entry.
        assert_eq!(
            value["relay_catalog"],
            json!({"source": "live", "slot": "secondary"})
        );
        assert!(value["data"][0].get("relay_catalog").is_none());
        // Hidden slugs are still findable for validation.
        assert!(snapshot.find("gpt-reserve").is_some());
        assert!(snapshot.find("gpt-5").is_none());
        assert_eq!(
            snapshot.listed_slugs(),
            ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.4"]
        );
    }

    #[test]
    fn extra_models_parse_cleanly_and_join_the_catalog_once() {
        assert_eq!(
            parse_extra_models("  gpt-7 , ,gpt-5.6-sol, gpt-7 ,gpt-8-mini"),
            vec![
                "gpt-7".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-8-mini".to_string()
            ]
        );
        assert!(parse_extra_models("").is_empty());
        assert!(parse_extra_models(" , , ").is_empty());

        let base = parse_catalog(&fixture()).unwrap();
        let joined = with_extras(base, &parse_extra_models("gpt-5.6-sol,gpt-7"));
        let slugs: Vec<&str> = joined.iter().map(|m| m.slug.as_str()).collect();
        // sol already there (once); gpt-7 appended last, listed, bare.
        assert_eq!(
            slugs,
            [
                "gpt-6-astra",
                "gpt-reserve",
                "gpt-5.6-sol",
                "gpt-5.4",
                "gpt-7"
            ]
        );
        let seven = joined.last().unwrap();
        assert!(seven.listed);
        assert!(seven.priority > 16);
        assert_eq!(seven.context_window, None);
    }

    #[test]
    fn fallback_is_the_static_list_in_its_own_order() {
        let models = with_extras(fallback_models(), &[]);
        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(slugs, FALLBACK_MODELS.to_vec());
        assert!(models.iter().all(|m| m.listed));
        assert!(slugs.contains(&"gpt-6-astra"));
    }

    #[tokio::test]
    async fn discovery_off_reports_static_and_extras() {
        let catalog = ModelCatalog::with(
            DEFAULT_TTL,
            false,
            parse_extra_models("gpt-x"),
            CODEX_MODELS_URL,
        );
        let status = catalog.status().await;
        assert_eq!(status["source"], "static");
        assert_eq!(status["extra_models"], 1);
        assert_eq!(status["models"], FALLBACK_MODELS.len());
        assert!(status["fetched_seconds_ago"].is_null());
        // The built-in list was produced by no subscription; say so explicitly.
        assert!(status.get("slot").is_some_and(Value::is_null));
    }

    #[tokio::test]
    async fn a_preloaded_catalog_reports_live_until_the_ttl_passes() {
        let catalog =
            ModelCatalog::preloaded("primary", parse_catalog(&fixture()).unwrap(), DEFAULT_TTL);
        let status = catalog.status().await;
        assert_eq!(status["source"], "live");
        assert_eq!(status["slot"], "primary");
        let expired = ModelCatalog::preloaded(
            "primary",
            parse_catalog(&fixture()).unwrap(),
            Duration::ZERO,
        );
        let status = expired.status().await;
        assert_eq!(status["source"], "stale");
        assert_eq!(status["slot"], "primary", "age never rewrites provenance");
        let never = ModelCatalog::with(DEFAULT_TTL, true, Vec::new(), CODEX_MODELS_URL);
        let status = never.status().await;
        assert_eq!(status["source"], "fallback");
        assert!(status["slot"].is_null());
    }

    #[tokio::test]
    async fn status_reports_a_switch_before_any_read_and_switching_back_restores_live() {
        let catalog =
            ModelCatalog::preloaded("primary", parse_catalog(&fixture()).unwrap(), DEFAULT_TTL);
        catalog.invalidate_for_switch("secondary");
        // `/health` alone must stop calling primary's answer the live one.
        let status = catalog.status().await;
        assert_eq!(status["source"], "stale");
        assert_eq!(status["slot"], "primary");
        // Back to the slot that fetched it before anything refreshed: it is that
        // slot's live answer again.
        catalog.invalidate_for_switch("primary");
        let status = catalog.status().await;
        assert_eq!(status["source"], "live");
        assert_eq!(status["slot"], "primary");
        // A switch to the fetching slot never demotes its answer.
        catalog.invalidate_for_switch("primary");
        assert_eq!(catalog.status().await["source"], "live");
    }

    fn two_slot_router_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        write_account(root.path(), "primary", "acct-primary");
        write_account(root.path(), "secondary", "acct-secondary");
        root
    }

    #[tokio::test]
    async fn an_answer_is_bound_to_the_slot_whose_lease_fetched_it() {
        let backend = CatalogFixture::start().await;
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();

        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Live);
        assert_eq!(snapshot.slot.as_deref(), Some("primary"));
        assert_eq!(snapshot.listed_slugs(), [model_for("acct-primary")]);
        let status = catalog.status().await;
        assert_eq!(status["source"], "live");
        assert_eq!(status["slot"], "primary");
        assert_eq!(status["models"], 1);
        let list = catalog.model_list(&router, &client).await;
        assert_eq!(
            list.relay_catalog,
            Some(CatalogProvenance {
                source: "live".to_string(),
                slot: Some("primary".to_string()),
            })
        );
        assert_eq!(backend.hits("acct-primary"), 1, "one fetch, then the cache");
        assert_eq!(backend.hits("acct-secondary"), 0);
    }

    /// The trap a naive binding falls into: the active slot is unusable, so
    /// `lease_any` parks it and the standby fetches. The answer is the standby's;
    /// labelling it with the slot that was active when the fetch began would be
    /// false, and leasing only the active slot would blind discovery instead.
    #[tokio::test]
    async fn a_failover_during_discovery_binds_the_answer_to_the_standby() {
        let backend = CatalogFixture::start().await;
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        assert_eq!(router.active_slot().await, "primary");
        std::fs::write(
            root.path().join("accounts/primary/auth.json"),
            b"not json at all",
        )
        .unwrap();
        let client = Client::new();
        let catalog = backend.catalog();

        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Live);
        assert_eq!(snapshot.slot.as_deref(), Some("secondary"));
        assert_eq!(snapshot.listed_slugs(), [model_for("acct-secondary")]);
        assert_eq!(catalog.status().await["slot"], "secondary");
        assert_eq!(backend.hits("acct-secondary"), 1);
        // lease_any's own failover is untouched: parked and switched, as before.
        assert_eq!(router.active_slot().await, "secondary");
        assert!(router
            .describe()
            .await
            .iter()
            .any(|view| view.slot == "primary" && view.cooldown_seconds_left > 0));
    }

    #[tokio::test]
    async fn a_deliberate_switch_refreshes_an_answer_from_another_slot_at_once() {
        let backend = CatalogFixture::start().await;
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();
        assert_eq!(
            catalog.snapshot(&router, &client).await.slot.as_deref(),
            Some("primary")
        );

        router
            .switch_to("secondary", |slot| catalog.invalidate_for_switch(slot))
            .await
            .unwrap();
        // Well inside both the TTL and the retry cooldown of the first fetch.
        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Live);
        assert_eq!(snapshot.slot.as_deref(), Some("secondary"));
        assert_eq!(snapshot.listed_slugs(), [model_for("acct-secondary")]);
        assert!(
            catalog
                .is_known(&model_for("acct-secondary"), &router, &client)
                .await
        );
        assert_eq!(catalog.status().await["slot"], "secondary");
        // One refresh per switch, not one per read.
        catalog.snapshot(&router, &client).await;
        assert_eq!(backend.hits("acct-secondary"), 1);
        assert_eq!(backend.hits("acct-primary"), 1);
    }

    #[tokio::test]
    async fn a_catalog_switch_preserves_vendor_parking_and_updates_fast_capabilities() {
        use crate::core::account_router::QuotaReset;

        let backend = CatalogFixture::start().await;
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();
        let first = serde_json::to_value(catalog.model_list(&router, &client).await).unwrap();
        assert_eq!(first["relay_catalog"]["slot"], "primary");
        assert_eq!(first["data"][0]["service_tiers"][0]["id"], "priority");
        assert_eq!(first["data"][0]["additional_speed_tiers"], json!(["fast"]));

        let exhausted = router.lease_active().await.unwrap();
        let reset_seconds = 2 * 60 * 60;
        router
            .switch_after_quota(&exhausted, QuotaReset {
                resets_at: None,
                resets_in_seconds: Some(reset_seconds),
            })
            .await
            .unwrap();
        router
            .switch_to("secondary", |slot| catalog.invalidate_for_switch(slot))
            .await
            .unwrap();

        let second = serde_json::to_value(catalog.model_list(&router, &client).await).unwrap();
        assert_eq!(
            second["relay_catalog"],
            json!({"source":"live","slot":"secondary"})
        );
        assert_eq!(second["data"][0]["service_tiers"], json!([]));
        assert_eq!(second["data"][0]["additional_speed_tiers"], json!([]));
        let primary = router.describe().await.into_iter()
            .find(|slot| slot.slot == "primary")
            .unwrap();
        assert!(primary.cooldown_seconds_left <= reset_seconds);
        assert!(primary.cooldown_seconds_left >= reset_seconds - 60);
    }

    #[tokio::test]
    async fn a_switch_to_the_slot_that_fetched_the_answer_keeps_it() {
        let backend = CatalogFixture::start().await;
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();
        catalog.snapshot(&router, &client).await;

        router
            .switch_to("primary", |slot| catalog.invalidate_for_switch(slot))
            .await
            .unwrap();
        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Live);
        assert_eq!(snapshot.slot.as_deref(), Some("primary"));
        assert_eq!(backend.hits("acct-primary"), 1, "nothing to refresh");
        assert_eq!(backend.hits("acct-secondary"), 0);
    }

    /// Recovery is unchanged by the switch: the last good answer keeps serving
    /// when the selected slot cannot read the catalog, honestly labelled, and
    /// the retry cooldown still guards upstream afterwards.
    #[tokio::test]
    async fn a_failed_refresh_after_a_switch_keeps_the_last_answer_and_its_true_slot() {
        let backend = CatalogFixture::start().await;
        backend.fail("acct-secondary");
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();
        catalog.snapshot(&router, &client).await;

        router
            .switch_to("secondary", |slot| catalog.invalidate_for_switch(slot))
            .await
            .unwrap();
        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(
            backend.hits("acct-secondary"),
            1,
            "the switch is acted on at once"
        );
        assert_eq!(snapshot.source, CatalogSource::Stale);
        assert_eq!(snapshot.slot.as_deref(), Some("primary"));
        assert_eq!(snapshot.listed_slugs(), [model_for("acct-primary")]);
        let list = model_list_from(&snapshot);
        assert_eq!(
            serde_json::to_value(&list).unwrap()["relay_catalog"],
            json!({"source": "stale", "slot": "primary"})
        );
        let status = catalog.status().await;
        assert_eq!(status["source"], "stale");
        assert_eq!(status["slot"], "primary");
        assert!(
            status["last_error"]
                .as_str()
                .is_some_and(|error| error.contains("500")),
            "{status}"
        );

        // Neither a plain read nor an unknown slug's forced refresh gets past
        // the cooldown the failed attempt armed.
        catalog.snapshot(&router, &client).await;
        assert!(!catalog.is_known("no-such-model", &router, &client).await);
        assert_eq!(backend.hits("acct-secondary"), 1);
        assert_eq!(backend.hits("acct-primary"), 1);
        assert_eq!(router.active_slot().await, "secondary");
    }

    /// The cold start: the very first discovery fails and arms the retry
    /// cooldown while there is no answer for a switch to supersede. A
    /// deliberate switch outranks that cooldown all the same: the next
    /// `/v1/models` read asks the selected slot at once, whichever reader --
    /// `/health` or the list itself -- applies the switch first.
    #[tokio::test]
    async fn a_switch_after_a_failed_cold_start_refreshes_at_once() {
        for health_first in [false, true] {
            let backend = CatalogFixture::start().await;
            backend.fail("acct-primary");
            let root = two_slot_router_root();
            let router = AccountRouter::load(root.path()).await.unwrap();
            let client = Client::new();
            let catalog = backend.catalog();

            let cold = catalog.snapshot(&router, &client).await;
            assert_eq!(cold.source, CatalogSource::Fallback);
            assert_eq!(cold.slot, None);
            assert!(catalog.status().await["last_error"].is_string());
            assert!(catalog.inner.lock().await.last_attempt.is_some());
            // The failed attempt armed the cooldown: a plain read asks no one.
            catalog.snapshot(&router, &client).await;
            assert_eq!(backend.hits("acct-primary"), 1);
            assert_eq!(backend.hits("acct-secondary"), 0);

            router
                .switch_to("secondary", |slot| catalog.invalidate_for_switch(slot))
                .await
                .unwrap();
            if health_first {
                // `/health` applies the switch but never fetches.
                let status = catalog.status().await;
                assert_eq!(status["source"], "fallback");
                assert!(status["slot"].is_null());
            }
            // Well inside the cooldown of the failed cold-start attempt.
            let list = catalog.model_list(&router, &client).await;
            assert_eq!(
                backend.hits("acct-secondary"),
                1,
                "the switch is acted on at once (health_first: {health_first})"
            );
            assert_eq!(
                list.relay_catalog,
                Some(CatalogProvenance {
                    source: "live".to_string(),
                    slot: Some("secondary".to_string()),
                })
            );
            assert_eq!(list.data.len(), 1);
            assert_eq!(list.data[0].id, model_for("acct-secondary"));
            assert!(catalog.status().await["last_error"].is_null());

            // One refresh per switch: the cooldown guards upstream again.
            catalog.snapshot(&router, &client).await;
            assert!(!catalog.is_known("no-such-model", &router, &client).await);
            assert_eq!(backend.hits("acct-secondary"), 1);
            assert_eq!(backend.hits("acct-primary"), 1);
        }
    }

    /// The active primary signs out and the router moves to the secondary on
    /// its own. Primary's answer goes with its credentials: `/health` stops
    /// naming it before anything is read, and the next read asks the
    /// secondary at once. When the secondary cannot answer, the built-in list
    /// stands in -- the removed subscription's list is not kept as `stale`,
    /// and its models are no longer accepted either way.
    #[tokio::test]
    async fn a_logout_forgets_the_answer_its_credentials_fetched() {
        for secondary_answers in [true, false] {
            let backend = CatalogFixture::start().await;
            if !secondary_answers {
                backend.fail("acct-secondary");
            }
            let root = two_slot_router_root();
            let router = AccountRouter::load(root.path()).await.unwrap();
            let client = Client::new();
            let catalog = backend.catalog();
            let before = catalog.snapshot(&router, &client).await;
            assert_eq!(before.source, CatalogSource::Live);
            assert_eq!(before.slot.as_deref(), Some("primary"));

            router
                .logout("primary", || {
                    catalog.invalidate_for_credential_change("primary")
                })
                .await
                .unwrap();
            assert_eq!(router.active_slot().await, "secondary");
            let status = catalog.status().await;
            assert_eq!(status["source"], "fallback", "{status}");
            assert!(status["slot"].is_null());
            assert_eq!(status["models"], FALLBACK_MODELS.len());
            assert!(status["fetched_seconds_ago"].is_null());

            // Well inside both the TTL and the retry cooldown of the first fetch.
            let list = catalog.model_list(&router, &client).await;
            assert_eq!(
                backend.hits("acct-secondary"),
                1,
                "the logout is acted on at once (secondary_answers: {secondary_answers})"
            );
            let ids: Vec<&str> = list.data.iter().map(|m| m.id.as_str()).collect();
            if secondary_answers {
                assert_eq!(
                    list.relay_catalog,
                    Some(CatalogProvenance {
                        source: "live".to_string(),
                        slot: Some("secondary".to_string()),
                    })
                );
                assert_eq!(ids, [model_for("acct-secondary")]);
            } else {
                assert_eq!(
                    list.relay_catalog,
                    Some(CatalogProvenance {
                        source: "fallback".to_string(),
                        slot: None,
                    })
                );
                assert_eq!(ids, FALLBACK_MODELS.to_vec());
            }
            assert!(
                !catalog
                    .is_known(&model_for("acct-primary"), &router, &client)
                    .await
            );
            assert_eq!(backend.hits("acct-primary"), 1);
            assert_eq!(backend.hits("acct-secondary"), 1);
        }
    }

    /// A credential change elsewhere leaves an answer alone: signing the
    /// standby in again costs the active slot's fresh answer nothing. And when
    /// the slot a switch selected signs out, the router moves back to the slot
    /// whose answer is still held; that answer is kept, and the next read
    /// refreshes it through that slot at once rather than after the cooldown
    /// the failed post-switch refresh armed.
    #[tokio::test]
    async fn a_credential_change_elsewhere_keeps_the_answer_and_a_move_back_refreshes_it() {
        let backend = CatalogFixture::start().await;
        let root = two_slot_router_root();
        let router = AccountRouter::load(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();
        catalog.snapshot(&router, &client).await;

        catalog.invalidate_for_credential_change("secondary");
        assert_eq!(catalog.status().await["source"], "live");
        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Live);
        assert_eq!(snapshot.slot.as_deref(), Some("primary"));
        assert_eq!(backend.hits("acct-primary"), 1, "nothing was due");
        assert_eq!(backend.hits("acct-secondary"), 0);

        backend.fail("acct-secondary");
        router
            .switch_to("secondary", |slot| catalog.invalidate_for_switch(slot))
            .await
            .unwrap();
        assert_eq!(
            catalog.snapshot(&router, &client).await.source,
            CatalogSource::Stale
        );
        assert_eq!(backend.hits("acct-secondary"), 1);

        router
            .logout("secondary", || {
                catalog.invalidate_for_credential_change("secondary")
            })
            .await
            .unwrap();
        assert_eq!(router.active_slot().await, "primary");
        let status = catalog.status().await;
        assert_eq!(
            status["source"], "stale",
            "primary's answer is kept: {status}"
        );
        assert_eq!(status["slot"], "primary");
        // Inside the cooldown the failed refresh through secondary armed.
        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Live);
        assert_eq!(snapshot.slot.as_deref(), Some("primary"));
        assert_eq!(snapshot.listed_slugs(), [model_for("acct-primary")]);
        assert_eq!(backend.hits("acct-primary"), 2);
        assert_eq!(backend.hits("acct-secondary"), 1);
    }

    /// A sign-in brings new credentials, so the next read asks at once: past
    /// the cooldown a cold start with no subscription armed, and past the TTL
    /// of the answer the slot's previous account fetched -- which is forgotten
    /// rather than served as `stale` under a slot that now holds someone else.
    #[tokio::test]
    async fn a_sign_in_is_asked_at_once_and_retires_the_slot_previous_answer() {
        let backend = CatalogFixture::start().await;
        let root = tempfile::tempdir().unwrap();
        let router = AccountRouter::open(root.path()).await.unwrap();
        let client = Client::new();
        let catalog = backend.catalog();
        let cold = catalog.snapshot(&router, &client).await;
        assert_eq!(cold.source, CatalogSource::Fallback);
        assert!(catalog.inner.lock().await.last_attempt.is_some());

        router.stage_next_login(staged_login("primary", "acct-first"));
        router
            .login("primary", || {
                catalog.invalidate_for_credential_change("primary")
            })
            .await
            .unwrap();
        let first = catalog.snapshot(&router, &client).await;
        assert_eq!(first.source, CatalogSource::Live);
        assert_eq!(first.slot.as_deref(), Some("primary"));
        assert_eq!(first.listed_slugs(), [model_for("acct-first")]);

        // The same slot signed in again, as another account, inside the TTL.
        router.stage_next_login(staged_login("primary", "acct-second"));
        router
            .login("primary", || {
                catalog.invalidate_for_credential_change("primary")
            })
            .await
            .unwrap();
        let status = catalog.status().await;
        assert_eq!(status["source"], "fallback", "{status}");
        assert!(status["slot"].is_null());
        let second = catalog.snapshot(&router, &client).await;
        assert_eq!(second.source, CatalogSource::Live);
        assert_eq!(second.slot.as_deref(), Some("primary"));
        assert_eq!(second.listed_slugs(), [model_for("acct-second")]);
        assert!(
            !catalog
                .is_known(&model_for("acct-first"), &router, &client)
                .await
        );
        // One ask per sign-in: the cooldown guards upstream again.
        assert_eq!(backend.hits("acct-first"), 1);
        assert_eq!(backend.hits("acct-second"), 1);
    }

    #[tokio::test]
    async fn the_builtin_lists_name_no_slot() {
        let backend = CatalogFixture::start().await;
        let root = tempfile::tempdir().unwrap();
        let router = AccountRouter::open(root.path()).await.unwrap();
        let client = Client::new();

        let snapshot = ModelCatalog::static_only().snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Static);
        assert_eq!(snapshot.slot, None);
        let value = serde_json::to_value(model_list_from(&snapshot)).unwrap();
        assert_eq!(
            value["relay_catalog"],
            json!({"source": "static", "slot": null})
        );

        // Discovery on, but no subscription to ask: the fallback, from no slot.
        let catalog = backend.catalog();
        let snapshot = catalog.snapshot(&router, &client).await;
        assert_eq!(snapshot.source, CatalogSource::Fallback);
        assert_eq!(snapshot.slot, None);
        assert_eq!(snapshot.listed_slugs(), FALLBACK_MODELS.to_vec());
        let status = catalog.status().await;
        assert_eq!(status["source"], "fallback");
        assert!(status["slot"].is_null());
        assert!(status["last_error"].is_string());
        // A switch recorded while nothing was fetched has nothing to demote.
        catalog.invalidate_for_switch("secondary");
        assert_eq!(catalog.status().await["source"], "fallback");
        assert_eq!(
            backend.hits("acct-primary") + backend.hits("acct-secondary"),
            0
        );
    }

    #[test]
    fn effort_is_clamped_to_what_the_model_takes() {
        let astra: Vec<String> = ["low", "medium", "high", "xhigh", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let sol: Vec<String> = ["low", "medium", "high", "xhigh", "max", "ultra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let none: Vec<String> = Vec::new();
        // The live failure: relay default "none" on astra -> its lowest level.
        assert_eq!(
            clamp_effort(Some("none"), &astra, Some("medium")).as_deref(),
            Some("low")
        );
        assert_eq!(
            clamp_effort(Some("minimal"), &astra, None).as_deref(),
            Some("low")
        );
        // Supported values pass untouched; too high folds to the top.
        assert_eq!(
            clamp_effort(Some("high"), &astra, None).as_deref(),
            Some("high")
        );
        assert_eq!(
            clamp_effort(Some("ultra"), &astra, None).as_deref(),
            Some("max")
        );
        assert_eq!(
            clamp_effort(Some("ultra"), &sol, None).as_deref(),
            Some("ultra")
        );
        // Junk: the model's default when it is real, else the lowest level.
        assert_eq!(
            clamp_effort(Some("turbo"), &astra, Some("medium")).as_deref(),
            Some("medium")
        );
        assert_eq!(
            clamp_effort(Some("turbo"), &astra, Some("nope")).as_deref(),
            Some("low")
        );
        // No knowledge -> no opinion (terra takes "none" today; the fallback list
        // carries no levels, so nothing is rewritten there).
        assert_eq!(
            clamp_effort(Some("none"), &none, None).as_deref(),
            Some("none")
        );
        assert_eq!(
            clamp_effort(Some("turbo"), &none, None).as_deref(),
            Some("turbo")
        );
        assert_eq!(clamp_effort(None, &astra, Some("medium")), None);
    }

    #[test]
    fn source_names_are_stable_strings() {
        assert_eq!(CatalogSource::Live.as_str(), "live");
        assert_eq!(CatalogSource::Stale.as_str(), "stale");
        assert_eq!(CatalogSource::Fallback.as_str(), "fallback");
        assert_eq!(CatalogSource::Static.as_str(), "static");
    }
}

/// A local stand-in for the backend catalog endpoint, shared by the catalog,
/// HTTP and control tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use axum::{http::HeaderMap, http::StatusCode, response::IntoResponse, routing::get, Router};
    use base64::Engine;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    /// The one model the fixture offers `account_id`, so an answer names whose
    /// credentials fetched it.
    pub(crate) fn model_for(account_id: &str) -> String {
        format!("model-of-{account_id}")
    }

    #[derive(Default)]
    struct Ledger {
        hits: BTreeMap<String, usize>,
        failing: BTreeSet<String>,
    }

    pub(crate) struct CatalogFixture {
        url: String,
        ledger: Arc<std::sync::Mutex<Ledger>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl CatalogFixture {
        pub(crate) async fn start() -> Self {
            let ledger = Arc::new(std::sync::Mutex::new(Ledger::default()));
            let served = ledger.clone();
            let app = Router::new().route(
                "/codex/models",
                get(move |headers: HeaderMap| {
                    let ledger = served.clone();
                    async move {
                        let account = headers
                            .get("chatgpt-account-id")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let failing = {
                            let mut ledger = ledger.lock().unwrap();
                            *ledger.hits.entry(account.clone()).or_default() += 1;
                            ledger.failing.contains(&account)
                        };
                        if failing {
                            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                        }
                        let service_tiers = if account == "acct-primary" {
                            json!([{"id":"priority","name":"Fast"}])
                        } else {
                            json!([])
                        };
                        let speed_tiers = if account == "acct-primary" {
                            json!(["fast"])
                        } else {
                            json!([])
                        };
                        axum::Json(json!({"models": [{
                            "slug": model_for(&account), "visibility": "list", "priority": 1,
                            "service_tiers": service_tiers, "additional_speed_tiers": speed_tiers
                        }]}))
                        .into_response()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/codex/models", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            CatalogFixture { url, ledger, task }
        }

        /// A discovering catalog with the default TTL that reads this fixture.
        pub(crate) fn catalog(&self) -> ModelCatalog {
            ModelCatalog::with(DEFAULT_TTL, true, Vec::new(), &self.url)
        }

        /// Catalog requests that arrived with `account_id`'s credentials.
        pub(crate) fn hits(&self, account_id: &str) -> usize {
            self.ledger
                .lock()
                .unwrap()
                .hits
                .get(account_id)
                .copied()
                .unwrap_or(0)
        }

        /// From now on `account_id` is answered with a 500.
        pub(crate) fn fail(&self, account_id: &str) {
            self.ledger
                .lock()
                .unwrap()
                .failing
                .insert(account_id.to_string());
        }
    }

    impl Drop for CatalogFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// A synthetic ChatGPT profile in `slot`, the shape the router tests write.
    pub(crate) fn write_account(root: &Path, slot: &str, account_id: &str) {
        write_auth(&root.join("accounts").join(slot), slot, account_id);
    }

    /// What the browser sign-in leaves staged for `slot` as `account_id`, for
    /// `AccountRouter::stage_next_login`.
    pub(crate) fn staged_login(slot: &str, account_id: &str) -> tempfile::TempDir {
        let staged = tempfile::tempdir().unwrap();
        write_auth(staged.path(), slot, account_id);
        staged
    }

    /// `home/auth.json` holding `account_id`'s synthetic credentials.
    fn write_auth(home: &Path, slot: &str, account_id: &str) {
        std::fs::create_dir_all(home).unwrap();
        let claims = json!({
            "email": format!("{slot}@example.test"),
            "https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let auth = json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": format!("e30.{encoded}.c2ln"),
                "access_token": format!("access-{slot}"),
                "refresh_token": format!("refresh-{slot}"),
                "account_id": account_id
            },
            "last_refresh": chrono::Utc::now().to_rfc3339()
        });
        std::fs::write(home.join("auth.json"), auth.to_string()).unwrap();
    }
}
