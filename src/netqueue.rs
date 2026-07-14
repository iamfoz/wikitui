//! The single background networking substrate (PRD §5.8 intro, §6.5 NF-NET-1..8).
//!
//! Every non-interactive request wikitui makes — stale-while-revalidate
//! revalidation (FR-OFF-2), link prefetch (FR-PF-1), trending prefetch
//! (FR-PF-2) — rides one *strictly serial, lowest-priority* background queue
//! feeding a single worker task. This module owns the queue mechanics and the
//! respectful-client guarantees that make that traffic safe to send at all;
//! it deliberately knows nothing about Wikipedia. The actual HTTP work is a
//! caller-supplied [`Executor`] (wired to `api::WikiClient` in `main`), so the
//! whole state machine here — priority ordering, single-flight dedup, budgets,
//! the circuit breaker, backoff, the foreground gate — is testable with a fake
//! executor and an injected clock, no network and no wall clock.
//!
//! Design decisions and their PRD anchors:
//! - **NF-NET-1** foreground > revalidation > prefetch; background is one
//!   serial worker (never two requests in flight), and it *yields to
//!   foreground* through [`ForegroundGate`]: the worker will not *start* a new
//!   background request while any foreground request is in flight. It cannot
//!   preempt a request already on the wire — that is documented, not a bug —
//!   but because foreground fetches run on their own task on the multi-thread
//!   runtime, they are never blocked *by* the worker.
//! - **NF-NET-4** a [`CircuitBreaker`] suspends *all* background traffic
//!   (revalidation included) after `k` consecutive 429/5xx, for a documented
//!   cooldown; `Retry-After` is honored on 429/503; other network errors get
//!   exponential backoff with deterministic, seeded [`Backoff`] jitter.
//! - **NF-NET-5** single-flight: a `(kind, lang, title)` already queued or in
//!   flight coalesces — a second enqueue is dropped.
//! - **NF-NET-7** budgets (FR-PF-5) are enforced *here, at the queue*, never at
//!   call sites: a prefetch job whose byte/request budget is exhausted is
//!   logged `skipped-budget` and dropped without touching the network.
//!   Revalidation is not prefetch, so it is exempt from the *budget* (but not
//!   from the gate, the breaker, or serialization).
//!
//! Budget counters are **in-memory** and reset on restart — an accepted v1.0
//! tradeoff (FR-PF-5 allows it if noted): a rolling one-hour request window and
//! 24-hour byte window live in [`Budget`], not in `$XDG_STATE`, so a restart
//! forgives the day. Persisting them is a documented later option.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

/// Monotonic seconds source for the rolling budget windows and the breaker
/// cooldown. A trait so tests inject a controllable clock instead of sleeping
/// real wall time (the codebase's "thread a clock, never read the wall clock
/// directly in logic" convention — cf. `cache`'s age arithmetic taking
/// `age_secs` as a parameter).
pub trait Clock: Send + Sync {
    fn now_secs(&self) -> u64;
}

/// Production clock: monotonic seconds since the substrate was built. Relative
/// windows (last hour / last 24h / cooldown) only ever need elapsed time, so a
/// monotonic base is correct and immune to wall-clock jumps.
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}

/// PRD FR-PF-5's `prefetch.metered`. Metered-*detection* (NetworkManager
/// D-Bus) is a documented later seam (SP-11): with no detector wired, `Never`
/// and `Reduced` both run at the configured budgets today, and `Always`
/// suspends prefetch entirely (treats the link as metered). Once a detector
/// exists, `Reduced` is where it would shrink the budget on a metered link
/// while `Never` would ignore the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metered {
    Never,
    Reduced,
    Always,
}

impl Metered {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "never" => Some(Self::Never),
            "reduced" => Some(Self::Reduced),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Reduced => "reduced",
            Self::Always => "always",
        }
    }
}

/// Tunables for the substrate, resolved from `[prefetch]` config in `main`.
#[derive(Debug, Clone)]
pub struct SubstrateConfig {
    /// FR-PF-5 per-day prefetch byte budget (default 20 MB).
    pub daily_byte_budget: u64,
    /// FR-PF-5 per-hour background *request* budget (default 100).
    pub hourly_request_budget: u32,
    pub metered: Metered,
    /// FR-PF-1 top-N article bodies to prefetch from a page's links.
    pub top_n: usize,
    /// FR-PF-1 ranking weights (`w1·lead + w2·log(views) + w3·affinity`).
    pub weights: RankWeights,
    /// NF-NET-4 circuit-breaker trip threshold (`k`, default 3).
    pub breaker_threshold: u32,
    /// NF-NET-4 breaker cooldown once tripped (default 300 s).
    pub breaker_cooldown_secs: u64,
    /// NF-NET-3 minimum pause on a lag/429 with no usable `Retry-After`.
    pub retry_after_default_secs: u64,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    /// Seed for the deterministic backoff jitter (production seeds from the
    /// wall clock; tests pin it).
    pub jitter_seed: u64,
}

impl Default for SubstrateConfig {
    fn default() -> Self {
        Self {
            daily_byte_budget: 20 * 1024 * 1024,
            hourly_request_budget: 100,
            metered: Metered::Reduced,
            top_n: 5,
            weights: RankWeights::default(),
            breaker_threshold: 3,
            breaker_cooldown_secs: 300,
            retry_after_default_secs: 5,
            backoff_base_ms: 500,
            backoff_max_ms: 30_000,
            jitter_seed: 0x5EED_1234_ABCD_0001,
        }
    }
}

/// FR-PF-1 ranking weights, config-overridable. `affinity` is retained but 0
/// by default — the interest model (FR-PF-3) is a v1.x chunk; this is its
/// seam.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankWeights {
    pub lead: f64,
    pub pageviews: f64,
    pub affinity: f64,
}

impl Default for RankWeights {
    fn default() -> Self {
        Self {
            lead: 1.0,
            pageviews: 1.0,
            affinity: 0.0,
        }
    }
}

// -- Jitter & backoff (deterministic, seeded) -----------------------------

/// A tiny SplitMix64 PRNG. The crate has no `rand` dependency and the brief
/// forbids un-seeded randomness in logic that tests must pin, so backoff
/// jitter draws from this seeded generator instead — same seed, same
/// sequence, every run.
#[derive(Debug, Clone)]
pub struct Jitter {
    state: u64,
}

impl Jitter {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A fraction in `[0, 1)`.
    fn next_frac(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Exponential backoff with full jitter (NF-NET-4). `delay(attempt)` returns a
/// duration in `[0, min(base·2^attempt, max))` — the AWS "full jitter"
/// strategy, which spreads retries instead of synchronizing them. Jitter is
/// deterministic given the seed so tests can assert the exact sequence.
#[derive(Debug, Clone)]
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
    jitter: Jitter,
}

impl Backoff {
    pub fn new(base_ms: u64, max_ms: u64, seed: u64) -> Self {
        Self {
            base_ms: base_ms.max(1),
            max_ms: max_ms.max(1),
            jitter: Jitter::new(seed),
        }
    }

    /// The capped exponential ceiling for `attempt` (no jitter) — the upper
    /// bound `delay` draws under.
    fn ceiling_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.min(16);
        self.base_ms.saturating_mul(1u64 << shift).min(self.max_ms)
    }

    pub fn delay(&mut self, attempt: u32) -> Duration {
        let ceiling = self.ceiling_ms(attempt);
        let jittered = (self.jitter.next_frac() * ceiling as f64) as u64;
        Duration::from_millis(jittered)
    }
}

// -- Budget (rolling windows, in-memory) ----------------------------------

/// FR-PF-5 / NF-NET-7 prefetch budgets, enforced at the queue. A rolling
/// one-hour request window and 24-hour byte window, both in memory (reset on
/// restart — the documented v1.0 tradeoff). The check-before / record-after
/// shape means the byte cap may be overshot by exactly one article (bytes are
/// unknown until fetched); the *next* job is then blocked — the standard,
/// intentional single-overshoot.
#[derive(Debug)]
pub struct Budget {
    daily_byte_cap: u64,
    hourly_req_cap: u32,
    metered: Metered,
    reqs: VecDeque<u64>,
    bytes: VecDeque<(u64, u64)>,
}

const HOUR_SECS: u64 = 3600;
const DAY_SECS: u64 = 86_400;

impl Budget {
    pub fn new(daily_byte_cap: u64, hourly_req_cap: u32, metered: Metered) -> Self {
        Self {
            daily_byte_cap,
            hourly_req_cap,
            metered,
            reqs: VecDeque::new(),
            bytes: VecDeque::new(),
        }
    }

    fn prune(&mut self, now: u64) {
        while let Some(&t) = self.reqs.front() {
            if now.saturating_sub(t) >= HOUR_SECS {
                self.reqs.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(t, _)) = self.bytes.front() {
            if now.saturating_sub(t) >= DAY_SECS {
                self.bytes.pop_front();
            } else {
                break;
            }
        }
    }

    fn bytes_used(&self) -> u64 {
        self.bytes.iter().map(|&(_, n)| n).sum()
    }

    /// Whether a prefetch request may be issued now. `Always`-metered suspends
    /// prefetch outright; otherwise both the hourly request window and the
    /// daily byte window must have headroom.
    pub fn allows(&mut self, now: u64) -> bool {
        if self.metered == Metered::Always {
            return false;
        }
        self.prune(now);
        (self.reqs.len() as u32) < self.hourly_req_cap && self.bytes_used() < self.daily_byte_cap
    }

    pub fn record_request(&mut self, now: u64) {
        self.reqs.push_back(now);
    }

    pub fn record_bytes(&mut self, now: u64, n: u64) {
        if n > 0 {
            self.bytes.push_back((now, n));
        }
    }

    pub fn snapshot(&mut self, now: u64) -> BudgetSnapshot {
        self.prune(now);
        BudgetSnapshot {
            requests_used: self.reqs.len() as u32,
            requests_cap: self.hourly_req_cap,
            bytes_used: self.bytes_used(),
            bytes_cap: self.daily_byte_cap,
            metered: self.metered,
            suspended: self.metered == Metered::Always,
        }
    }
}

/// A read-only view of budget state for the `:prefetch-log` panel (FR-PF-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub requests_used: u32,
    pub requests_cap: u32,
    pub bytes_used: u64,
    pub bytes_cap: u64,
    pub metered: Metered,
    pub suspended: bool,
}

// -- Circuit breaker ------------------------------------------------------

/// NF-NET-4 circuit breaker: after `threshold` consecutive 429/5xx it opens
/// and stays open for `cooldown_secs`, suspending *all* background traffic. A
/// success (or any completed request that wasn't a 429/5xx) resets the
/// consecutive count. After the cooldown elapses `is_open` returns false,
/// letting one probe through; if that probe also fails, `on_failure` re-opens
/// it (the consecutive count is already at/over threshold).
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown_secs: u64,
    consecutive: u32,
    opened_at: Option<u64>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown_secs: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown_secs,
            consecutive: 0,
            opened_at: None,
        }
    }

    pub fn on_success(&mut self) {
        self.consecutive = 0;
        self.opened_at = None;
    }

    pub fn on_failure(&mut self, now: u64) {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= self.threshold {
            self.opened_at = Some(now);
        }
    }

    pub fn is_open(&self, now: u64) -> bool {
        match self.opened_at {
            Some(t) => now.saturating_sub(t) < self.cooldown_secs,
            None => false,
        }
    }

    /// Seconds until the cooldown lets a probe through, or 0 if already open
    /// past cooldown / not open.
    pub fn remaining_cooldown(&self, now: u64) -> u64 {
        match self.opened_at {
            Some(t) => self.cooldown_secs.saturating_sub(now.saturating_sub(t)),
            None => 0,
        }
    }
}

// -- Foreground gate ------------------------------------------------------

/// NF-NET-1's "yield to foreground". Foreground fetches take a
/// [`ForegroundGuard`]; while any is alive the count is nonzero and the worker
/// parks in [`ForegroundGate::wait_until_idle`] rather than starting a new
/// background request. Built on a `watch` channel so a foreground burst that
/// starts and finishes between two worker polls can never be missed (watch
/// holds the latest value and versions changes).
#[derive(Debug)]
pub struct ForegroundGate {
    tx: watch::Sender<u32>,
}

impl Default for ForegroundGate {
    fn default() -> Self {
        Self {
            tx: watch::channel(0).0,
        }
    }
}

impl ForegroundGate {
    fn enter(&self) -> ForegroundGuard {
        self.tx.send_modify(|n| *n += 1);
        ForegroundGuard {
            tx: self.tx.clone(),
        }
    }

    async fn wait_until_idle(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() == 0 {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Held for the duration of a foreground request; decrements the gate on drop,
/// which wakes the worker if it was the last one.
pub struct ForegroundGuard {
    tx: watch::Sender<u32>,
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        self.tx.send_modify(|n| *n = n.saturating_sub(1));
    }
}

// -- Jobs -----------------------------------------------------------------

/// One candidate outgoing link for FR-PF-1 ranking. `lead_position` is the
/// link's index in document order (0 = first link in the lead → strongest
/// lead score); `is_cursor` marks the link under the reader's cursor, which is
/// always kept as a candidate regardless of rank.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkCandidate {
    pub title: String,
    pub lead_position: usize,
    pub is_cursor: bool,
}

/// A unit of background work. Priority is derived from the variant:
/// `Revalidate` is revalidation priority (outranks all prefetch), everything
/// else is prefetch priority. `RankLinks`/`Featured` are *seed* jobs — one
/// batched metadata request each (NEVER per-article fanout, §6.2 rule 7) whose
/// result enqueues the actual `PrefetchArticle` bodies.
#[derive(Debug, Clone)]
pub enum Job {
    /// FR-OFF-2 stale-while-revalidate, migrated onto the substrate so it
    /// obeys the same gate/serial/breaker discipline as prefetch.
    Revalidate {
        tab_id: u64,
        lang: String,
        title: String,
        cached_revid: u64,
    },
    /// FR-PF-1/FR-PF-2 fetch one article body into L2 with its reason string.
    PrefetchArticle {
        lang: String,
        title: String,
        reason: String,
        /// The `:prefetch-log` entry id assigned at enqueue (FR-PF-4).
        log_id: u64,
    },
    /// FR-PF-1 seed: batch-fetch pageviews for a page's links, rank, and
    /// enqueue the top-N bodies.
    RankLinks {
        lang: String,
        article_title: String,
        candidates: Vec<LinkCandidate>,
    },
    /// FR-PF-2 seed: fetch the day's featured feed and enqueue TFA + top-10
    /// most-read bodies. `date` is the `yyyy-mm-dd` bucket (injected in main
    /// from `chrono`) that also drives the once-per-day cache.
    Featured { lang: String, date: String },
}

impl Job {
    fn is_prefetch(&self) -> bool {
        !matches!(self, Job::Revalidate { .. })
    }

    fn dedup_key(&self) -> String {
        match self {
            Job::Revalidate { lang, title, .. } => format!("rv:{lang}:{title}"),
            Job::PrefetchArticle { lang, title, .. } => format!("pf:{lang}:{title}"),
            Job::RankLinks {
                lang,
                article_title,
                ..
            } => format!("rl:{lang}:{article_title}"),
            Job::Featured { lang, date } => format!("ft:{lang}:{date}"),
        }
    }
}

/// The result the [`Executor`] hands back for one job.
#[derive(Debug, Default)]
pub struct ExecResult {
    pub outcome: Outcome,
    /// Follow-up jobs to enqueue (a seed job's ranked bodies). The worker
    /// enqueues these through the same dedup/budget/logging path.
    pub follow_ups: Vec<Job>,
}

/// How a job ended — drives breaker, budget accounting, and the log status.
#[derive(Debug, Default, Clone, PartialEq)]
pub enum Outcome {
    /// A request happened and succeeded; `bytes` fed the byte budget.
    Done { bytes: u64 },
    /// No request happened (already cached, or nothing to do). Neither budget
    /// nor breaker is touched.
    Skipped { note: String },
    /// A request happened and failed with a plain network/parse error (not a
    /// 429/5xx). Backoff applies; the breaker is *not* tripped.
    #[default]
    Failed,
    /// A 429 (or explicit lag) with an optional `Retry-After`. Trips the
    /// breaker and pauses the worker (NF-NET-3/4).
    RateLimited { retry_after: Option<Duration> },
    /// A 5xx. Trips the breaker; backoff applies.
    ServerError,
}

/// The HTTP-doing half, supplied by `main` (wired to `api::WikiClient`). Kept
/// as a trait so the whole substrate is testable with a fake. The returned
/// future is `Send` so the worker can be `tokio::spawn`ed on the multi-thread
/// runtime.
pub trait Executor: Send + Sync + 'static {
    fn execute(&self, job: Job) -> impl Future<Output = ExecResult> + Send;
}

// -- Log ------------------------------------------------------------------

/// FR-PF-4 transparency: the status of one logged prefetch action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStatus {
    Queued,
    Done,
    Failed,
    SkippedBudget,
    RateLimited,
}

impl LogStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::SkippedBudget => "skipped-budget",
            Self::RateLimited => "rate-limited",
        }
    }
}

/// One row of the `:prefetch-log` panel (FR-PF-4): what was prefetched, *why*
/// (the reason string), how it went, and how many bytes it cost.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: u64,
    pub title: String,
    pub reason: String,
    pub status: LogStatus,
    pub bytes: u64,
}

/// A bounded ring of recent prefetch actions.
#[derive(Debug, Default)]
pub struct PrefetchLog {
    entries: VecDeque<LogEntry>,
    next_id: u64,
}

const LOG_CAPACITY: usize = 200;

impl PrefetchLog {
    fn push_queued(&mut self, title: String, reason: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(LogEntry {
            id,
            title,
            reason,
            status: LogStatus::Queued,
            bytes: 0,
        });
        while self.entries.len() > LOG_CAPACITY {
            self.entries.pop_front();
        }
        id
    }

    fn update(&mut self, id: u64, status: LogStatus, bytes: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.status = status;
            e.bytes = bytes;
        }
    }

    /// Newest first, for the panel.
    pub fn recent(&self) -> Vec<LogEntry> {
        self.entries.iter().rev().cloned().collect()
    }
}

// -- Shared state & handle ------------------------------------------------

struct Queue {
    revalidation: VecDeque<Job>,
    prefetch: VecDeque<Job>,
    /// Dedup / single-flight: a key stays reserved from enqueue until the
    /// worker `complete`s it (i.e. across execution), so a duplicate enqueued
    /// mid-flight coalesces (NF-NET-5).
    inflight: HashSet<String>,
}

impl Queue {
    fn new() -> Self {
        Self {
            revalidation: VecDeque::new(),
            prefetch: VecDeque::new(),
            inflight: HashSet::new(),
        }
    }

    fn enqueue(&mut self, job: Job) -> bool {
        let key = job.dedup_key();
        if !self.inflight.insert(key) {
            return false;
        }
        if job.is_prefetch() {
            self.prefetch.push_back(job);
        } else {
            self.revalidation.push_back(job);
        }
        true
    }

    /// Revalidation outranks prefetch; FIFO within a priority.
    fn pop(&mut self) -> Option<Job> {
        self.revalidation
            .pop_front()
            .or_else(|| self.prefetch.pop_front())
    }

    /// Put a popped job back at the front of its lane without disturbing its
    /// (still-reserved) dedup key — used when the breaker defers it.
    fn requeue_front(&mut self, job: Job) {
        if job.is_prefetch() {
            self.prefetch.push_front(job);
        } else {
            self.revalidation.push_front(job);
        }
    }

    fn complete(&mut self, key: &str) {
        self.inflight.remove(key);
    }

    fn pending(&self) -> usize {
        self.revalidation.len() + self.prefetch.len()
    }
}

struct Shared {
    queue: Mutex<Queue>,
    budget: Mutex<Budget>,
    breaker: Mutex<CircuitBreaker>,
    log: Mutex<PrefetchLog>,
    gate: ForegroundGate,
    enabled: AtomicBool,
    /// Woken on every enqueue and on completion, so a parked worker resumes.
    notify: Notify,
    config: SubstrateConfig,
    /// The one clock the worker *and* the budget/breaker snapshots read, so
    /// the `:prefetch-log` panel's budget view agrees with the worker's
    /// accounting. Injectable for deterministic tests.
    clock: Arc<dyn Clock>,
}

/// The cloneable handle every part of the app holds: `main` to enqueue,
/// `App` to read the log/budget for the panel, the worker to drain. It is an
/// `Arc` inside, so cloning is cheap and all clones share one queue.
#[derive(Clone)]
pub struct SubstrateHandle {
    inner: Arc<Shared>,
}

impl SubstrateHandle {
    pub fn new(config: SubstrateConfig) -> Self {
        Self::new_with_clock(config, Arc::new(SystemClock::default()))
    }

    /// Construct with an injected clock (tests). Production uses [`new`].
    pub fn new_with_clock(config: SubstrateConfig, clock: Arc<dyn Clock>) -> Self {
        let budget = Budget::new(
            config.daily_byte_budget,
            config.hourly_request_budget,
            config.metered,
        );
        let breaker = CircuitBreaker::new(config.breaker_threshold, config.breaker_cooldown_secs);
        Self {
            inner: Arc::new(Shared {
                queue: Mutex::new(Queue::new()),
                budget: Mutex::new(budget),
                breaker: Mutex::new(breaker),
                log: Mutex::new(PrefetchLog::default()),
                gate: ForegroundGate::default(),
                enabled: AtomicBool::new(true),
                notify: Notify::new(),
                config,
                clock,
            }),
        }
    }

    /// FR-PF-6 kill switch. When off, prefetch enqueues become no-ops
    /// (revalidation still flows — it is not prefetch).
    pub fn set_enabled(&self, on: bool) {
        self.inner.enabled.store(on, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::SeqCst)
    }

    pub fn top_n(&self) -> usize {
        self.inner.config.top_n
    }

    pub fn weights(&self) -> RankWeights {
        self.inner.config.weights
    }

    /// Take a foreground guard around an interactive fetch (NF-NET-1). The
    /// worker will not start a new background request while it is alive.
    pub fn foreground_guard(&self) -> ForegroundGuard {
        self.inner.gate.enter()
    }

    /// Enqueue a revalidation (FR-OFF-2). Never gated by the kill switch.
    /// Returns whether it was newly queued (false = coalesced duplicate).
    pub fn enqueue_revalidation(
        &self,
        tab_id: u64,
        lang: String,
        title: String,
        cached_revid: u64,
    ) -> bool {
        let job = Job::Revalidate {
            tab_id,
            lang,
            title,
            cached_revid,
        };
        let added = self.inner.queue.lock().unwrap().enqueue(job);
        if added {
            self.inner.notify.notify_one();
        }
        added
    }

    /// Enqueue a prefetch-priority job (article body or a seed). A no-op when
    /// the kill switch is off (FR-PF-6). For `PrefetchArticle`, a `Queued` log
    /// row is created here and the job's `log_id` is filled in — logging is
    /// centralized at the queue, not scattered through the executor.
    pub fn enqueue_prefetch(&self, mut job: Job) -> bool {
        if !self.is_enabled() {
            return false;
        }
        if let Job::PrefetchArticle {
            title,
            reason,
            log_id,
            ..
        } = &mut job
        {
            let id = self
                .inner
                .log
                .lock()
                .unwrap()
                .push_queued(title.clone(), reason.clone());
            *log_id = id;
        }
        let added = self.inner.queue.lock().unwrap().enqueue(job);
        if added {
            self.inner.notify.notify_one();
        }
        added
    }

    /// Convenience for a ranked article body (FR-PF-1/2). Test-only: production
    /// enqueues `PrefetchArticle` jobs as follow-ups of a ranking/feed seed.
    #[cfg(test)]
    pub fn enqueue_prefetch_article(&self, lang: String, title: String, reason: String) -> bool {
        self.enqueue_prefetch(Job::PrefetchArticle {
            lang,
            title,
            reason,
            log_id: 0,
        })
    }

    pub fn pending(&self) -> usize {
        self.inner.queue.lock().unwrap().pending()
    }

    pub fn log_recent(&self) -> Vec<LogEntry> {
        self.inner.log.lock().unwrap().recent()
    }

    /// The current budget state (FR-PF-4 panel), read against the substrate's
    /// own clock so it matches the worker's accounting.
    pub fn budget_snapshot(&self) -> BudgetSnapshot {
        let now = self.inner.clock.now_secs();
        self.inner.budget.lock().unwrap().snapshot(now)
    }

    /// Drive the single serial worker until the runtime shuts down. Spawn one
    /// of these (only one — serialization is the whole point).
    pub async fn run<E: Executor>(self, executor: E) {
        let clock = self.inner.clock.clone();
        let mut backoff = Backoff::new(
            self.inner.config.backoff_base_ms,
            self.inner.config.backoff_max_ms,
            self.inner.config.jitter_seed,
        );
        let mut fail_attempt: u32 = 0;
        loop {
            // Block until there is work.
            let job = loop {
                if let Some(job) = self.inner.queue.lock().unwrap().pop() {
                    break job;
                }
                self.inner.notify.notified().await;
            };

            // NF-NET-1: never start a background request while foreground is
            // in flight.
            self.inner.gate.wait_until_idle().await;

            let now = clock.now_secs();

            // NF-NET-4: the breaker suspends *all* background traffic.
            let open_wait = {
                let breaker = self.inner.breaker.lock().unwrap();
                if breaker.is_open(now) {
                    Some(breaker.remaining_cooldown(now).max(1))
                } else {
                    None
                }
            };
            if let Some(secs) = open_wait {
                self.inner.queue.lock().unwrap().requeue_front(job);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }

            let key = job.dedup_key();

            // NF-NET-7: prefetch budgets are enforced here, at the queue.
            if job.is_prefetch() {
                let allowed = self.inner.budget.lock().unwrap().allows(now);
                if !allowed {
                    if let Job::PrefetchArticle { log_id, .. } = &job {
                        self.inner
                            .log
                            .lock()
                            .unwrap()
                            .update(*log_id, LogStatus::SkippedBudget, 0);
                    }
                    self.inner.queue.lock().unwrap().complete(&key);
                    continue;
                }
            }

            let is_prefetch = job.is_prefetch();
            let log_id = match &job {
                Job::PrefetchArticle { log_id, .. } => Some(*log_id),
                _ => None,
            };

            let result = executor.execute(job).await;
            let now2 = clock.now_secs();

            // A request was actually attempted unless the executor skipped.
            let attempted = !matches!(result.outcome, Outcome::Skipped { .. });
            if is_prefetch && attempted {
                self.inner.budget.lock().unwrap().record_request(now2);
            }

            let mut sleep_for: Option<Duration> = None;
            match &result.outcome {
                Outcome::Done { bytes } => {
                    if is_prefetch {
                        self.inner.budget.lock().unwrap().record_bytes(now2, *bytes);
                    }
                    self.inner.breaker.lock().unwrap().on_success();
                    fail_attempt = 0;
                    if let Some(id) = log_id {
                        self.inner
                            .log
                            .lock()
                            .unwrap()
                            .update(id, LogStatus::Done, *bytes);
                    }
                }
                Outcome::Skipped { .. } => {
                    // No request, no budget/breaker change. Drop the queued log
                    // row's "queued" state to done-with-0 so it doesn't dangle.
                    if let Some(id) = log_id {
                        self.inner
                            .log
                            .lock()
                            .unwrap()
                            .update(id, LogStatus::Done, 0);
                    }
                }
                Outcome::RateLimited { retry_after } => {
                    self.inner.breaker.lock().unwrap().on_failure(now2);
                    let secs = retry_after.unwrap_or(Duration::from_secs(
                        self.inner.config.retry_after_default_secs,
                    ));
                    let floor = Duration::from_secs(self.inner.config.retry_after_default_secs);
                    sleep_for = Some(secs.max(floor));
                    if let Some(id) = log_id {
                        self.inner
                            .log
                            .lock()
                            .unwrap()
                            .update(id, LogStatus::RateLimited, 0);
                    }
                }
                Outcome::ServerError => {
                    self.inner.breaker.lock().unwrap().on_failure(now2);
                    sleep_for = Some(backoff.delay(fail_attempt));
                    fail_attempt = fail_attempt.saturating_add(1);
                    if let Some(id) = log_id {
                        self.inner
                            .log
                            .lock()
                            .unwrap()
                            .update(id, LogStatus::Failed, 0);
                    }
                }
                Outcome::Failed => {
                    // Plain network error: backoff, but do not trip the breaker
                    // (that is reserved for 429/5xx, NF-NET-4).
                    sleep_for = Some(backoff.delay(fail_attempt));
                    fail_attempt = fail_attempt.saturating_add(1);
                    if let Some(id) = log_id {
                        self.inner
                            .log
                            .lock()
                            .unwrap()
                            .update(id, LogStatus::Failed, 0);
                    }
                }
            }

            for follow in result.follow_ups {
                self.enqueue_prefetch(follow);
            }
            self.inner.queue.lock().unwrap().complete(&key);
            self.inner.notify.notify_one();

            if let Some(dur) = sleep_for {
                tokio::time::sleep(dur).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    struct TestClock {
        secs: AtomicU64,
    }
    impl TestClock {
        fn new(start: u64) -> Arc<Self> {
            Arc::new(Self {
                secs: AtomicU64::new(start),
            })
        }
    }
    impl Clock for TestClock {
        fn now_secs(&self) -> u64 {
            self.secs.load(Ordering::SeqCst)
        }
    }

    // -- Pure-type tests --------------------------------------------------

    #[test]
    fn queue_serves_revalidation_before_prefetch() {
        let mut q = Queue::new();
        assert!(q.enqueue(Job::PrefetchArticle {
            lang: "en".into(),
            title: "A".into(),
            reason: "r".into(),
            log_id: 0,
        }));
        assert!(q.enqueue(Job::Revalidate {
            tab_id: 1,
            lang: "en".into(),
            title: "B".into(),
            cached_revid: 5,
        }));
        // Revalidation outranks the earlier-enqueued prefetch.
        assert!(matches!(q.pop(), Some(Job::Revalidate { .. })));
        assert!(matches!(q.pop(), Some(Job::PrefetchArticle { .. })));
        assert!(q.pop().is_none());
    }

    #[test]
    fn queue_dedups_identical_jobs_until_completed() {
        let mut q = Queue::new();
        let job = || Job::PrefetchArticle {
            lang: "en".into(),
            title: "Alan Turing".into(),
            reason: "r".into(),
            log_id: 0,
        };
        assert!(q.enqueue(job()));
        assert!(!q.enqueue(job()), "duplicate coalesces (NF-NET-5)");
        let popped = q.pop().unwrap();
        // Still reserved while 'in flight'.
        assert!(!q.enqueue(job()), "still coalesces mid-flight");
        q.complete(&popped.dedup_key());
        assert!(q.enqueue(job()), "re-queueable once completed");
    }

    #[test]
    fn budget_enforces_request_and_byte_windows_and_rolls_off() {
        let mut b = Budget::new(1000, 2, Metered::Reduced);
        assert!(b.allows(0));
        b.record_request(0);
        assert!(b.allows(0));
        b.record_request(0);
        assert!(!b.allows(0), "hourly request cap reached");
        // Requests older than an hour roll off.
        assert!(b.allows(3600));

        let mut b = Budget::new(100, 100, Metered::Reduced);
        assert!(b.allows(0));
        b.record_bytes(0, 200);
        assert!(!b.allows(0), "daily byte cap exceeded (single overshoot)");
        assert!(b.allows(DAY_SECS), "bytes roll off after 24h");
    }

    #[test]
    fn metered_always_suspends_prefetch() {
        let mut b = Budget::new(u64::MAX, u32::MAX, Metered::Always);
        assert!(!b.allows(0));
        let mut b = Budget::new(1, 1, Metered::Never);
        assert!(b.allows(0));
    }

    #[test]
    fn breaker_trips_after_k_failures_and_cools_down() {
        let mut cb = CircuitBreaker::new(3, 300);
        assert!(!cb.is_open(0));
        cb.on_failure(0);
        cb.on_failure(0);
        assert!(!cb.is_open(0), "under threshold");
        cb.on_failure(0);
        assert!(cb.is_open(0), "k=3 consecutive trips it");
        assert!(cb.is_open(299), "still open within cooldown");
        assert!(!cb.is_open(300), "open past cooldown lets a probe through");
        assert_eq!(cb.remaining_cooldown(100), 200);
    }

    #[test]
    fn breaker_success_resets_consecutive_count() {
        let mut cb = CircuitBreaker::new(3, 300);
        cb.on_failure(0);
        cb.on_failure(0);
        cb.on_success();
        cb.on_failure(0);
        cb.on_failure(0);
        assert!(
            !cb.is_open(0),
            "success reset the count so 2 more don't trip"
        );
    }

    #[test]
    fn backoff_is_deterministic_and_bounded() {
        let mut a = Backoff::new(100, 10_000, 42);
        let mut b = Backoff::new(100, 10_000, 42);
        for attempt in 0..8 {
            let da = a.delay(attempt);
            let db = b.delay(attempt);
            assert_eq!(da, db, "same seed => same sequence");
            let ceiling = 100u64.saturating_mul(1 << attempt.min(16)).min(10_000);
            assert!(
                da.as_millis() as u64 <= ceiling,
                "full jitter stays under the exponential ceiling"
            );
        }
        // A different seed diverges (guards against a constant/degenerate PRNG).
        let mut c = Backoff::new(100, 10_000, 43);
        let differs = (0..8).any(|i| c.delay(i) != Backoff::new(100, 10_000, 42).delay(i));
        assert!(differs);
    }

    // -- Async worker tests (fake executor) -------------------------------

    /// Records call order/concurrency and returns a scripted outcome.
    struct FakeExecutor {
        calls: Arc<Mutex<Vec<String>>>,
        concurrent: Arc<AtomicUsize>,
        max_concurrent: Arc<AtomicUsize>,
        outcome: Outcome,
        started: Arc<Notify>,
    }
    impl FakeExecutor {
        fn new(outcome: Outcome) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                concurrent: Arc::new(AtomicUsize::new(0)),
                max_concurrent: Arc::new(AtomicUsize::new(0)),
                outcome,
                started: Arc::new(Notify::new()),
            }
        }
    }
    impl Executor for FakeExecutor {
        fn execute(&self, job: Job) -> impl Future<Output = ExecResult> + Send {
            let calls = self.calls.clone();
            let concurrent = self.concurrent.clone();
            let max_concurrent = self.max_concurrent.clone();
            let started = self.started.clone();
            let outcome = self.outcome.clone();
            async move {
                let n = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(n, Ordering::SeqCst);
                calls.lock().unwrap().push(job.dedup_key());
                started.notify_one();
                tokio::task::yield_now().await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
                ExecResult {
                    outcome,
                    follow_ups: Vec::new(),
                }
            }
        }
    }

    fn test_config() -> SubstrateConfig {
        SubstrateConfig {
            breaker_threshold: 2,
            breaker_cooldown_secs: 60,
            retry_after_default_secs: 5,
            ..SubstrateConfig::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn worker_runs_background_strictly_serially() {
        let handle = SubstrateHandle::new_with_clock(test_config(), TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::Done { bytes: 10 });
        let max = exec.max_concurrent.clone();
        let calls = exec.calls.clone();
        tokio::spawn(handle.clone().run(exec));

        for i in 0..6 {
            handle.enqueue_prefetch_article("en".into(), format!("T{i}"), "r".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(calls.lock().unwrap().len(), 6, "all drained");
        assert_eq!(max.load(Ordering::SeqCst), 1, "never two in flight");
    }

    #[tokio::test(start_paused = true)]
    async fn worker_yields_to_foreground_then_resumes() {
        let handle = SubstrateHandle::new_with_clock(test_config(), TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::Done { bytes: 1 });
        let calls = exec.calls.clone();

        let guard = handle.foreground_guard();
        tokio::spawn(handle.clone().run(exec));
        handle.enqueue_prefetch_article("en".into(), "Gated".into(), "r".into());

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            0,
            "blocked while foreground active"
        );

        drop(guard);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "resumes once foreground clears"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn worker_runs_revalidation_before_prefetch() {
        let handle = SubstrateHandle::new_with_clock(test_config(), TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::Done { bytes: 1 });
        let calls = exec.calls.clone();

        // Hold foreground so both jobs are queued before the worker drains.
        let guard = handle.foreground_guard();
        tokio::spawn(handle.clone().run(exec));
        handle.enqueue_prefetch_article("en".into(), "P".into(), "r".into());
        handle.enqueue_revalidation(1, "en".into(), "R".into(), 7);
        drop(guard);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let order = calls.lock().unwrap().clone();
        assert_eq!(order, vec!["rv:en:R".to_string(), "pf:en:P".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn worker_coalesces_duplicate_prefetches() {
        let handle = SubstrateHandle::new_with_clock(test_config(), TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::Done { bytes: 1 });
        let calls = exec.calls.clone();

        let guard = handle.foreground_guard();
        tokio::spawn(handle.clone().run(exec));
        assert!(handle.enqueue_prefetch_article("en".into(), "Dup".into(), "r".into()));
        assert!(!handle.enqueue_prefetch_article("en".into(), "Dup".into(), "r2".into()));
        drop(guard);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(calls.lock().unwrap().len(), 1, "single-flight");
    }

    #[tokio::test(start_paused = true)]
    async fn kill_switch_blocks_prefetch_but_not_revalidation() {
        let handle = SubstrateHandle::new_with_clock(test_config(), TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::Done { bytes: 1 });
        let calls = exec.calls.clone();
        tokio::spawn(handle.clone().run(exec));

        handle.set_enabled(false);
        assert!(!handle.enqueue_prefetch_article("en".into(), "Nope".into(), "r".into()));
        assert!(handle.enqueue_revalidation(1, "en".into(), "Yes".into(), 3));

        tokio::time::sleep(Duration::from_millis(50)).await;
        let order = calls.lock().unwrap().clone();
        assert_eq!(
            order,
            vec!["rv:en:Yes".to_string()],
            "only revalidation ran"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn budget_exhaustion_skips_prefetch_and_logs_it() {
        let cfg = SubstrateConfig {
            hourly_request_budget: 1,
            ..test_config()
        };
        let handle = SubstrateHandle::new_with_clock(cfg, TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::Done { bytes: 10 });
        let calls = exec.calls.clone();

        let guard = handle.foreground_guard();
        tokio::spawn(handle.clone().run(exec));
        handle.enqueue_prefetch_article("en".into(), "First".into(), "r".into());
        handle.enqueue_prefetch_article("en".into(), "Second".into(), "r".into());
        drop(guard);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "second blocked by request budget"
        );
        let log = handle.log_recent();
        let skipped = log
            .iter()
            .find(|e| e.title == "Second")
            .expect("second is logged");
        assert_eq!(skipped.status, LogStatus::SkippedBudget);
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_trips_and_suspends_further_background() {
        let cfg = SubstrateConfig {
            breaker_threshold: 2,
            breaker_cooldown_secs: 3600,
            ..test_config()
        };
        let handle = SubstrateHandle::new_with_clock(cfg, TestClock::new(0));
        let exec = FakeExecutor::new(Outcome::ServerError);
        let calls = exec.calls.clone();

        let guard = handle.foreground_guard();
        tokio::spawn(handle.clone().run(exec));
        for i in 0..5 {
            handle.enqueue_prefetch_article("en".into(), format!("S{i}"), "r".into());
        }
        drop(guard);

        tokio::time::sleep(Duration::from_millis(500)).await;
        // Two 5xx trip the breaker (threshold 2); the rest are suspended
        // during the (very long) cooldown.
        assert_eq!(calls.lock().unwrap().len(), 2, "suspended after k failures");
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_honors_retry_after_before_next() {
        let handle = SubstrateHandle::new_with_clock(test_config(), TestClock::new(0));
        // First job rate-limited with a 30s Retry-After, second succeeds.
        struct Scripted {
            n: Arc<AtomicUsize>,
            // tokio's virtual clock (advances under `start_paused`, unlike
            // `std::time::Instant`), so the recorded gap reflects the sleep.
            times: Arc<Mutex<Vec<tokio::time::Instant>>>,
        }
        impl Executor for Scripted {
            fn execute(&self, _job: Job) -> impl Future<Output = ExecResult> + Send {
                let n = self.n.clone();
                let times = self.times.clone();
                async move {
                    times.lock().unwrap().push(tokio::time::Instant::now());
                    let i = n.fetch_add(1, Ordering::SeqCst);
                    let outcome = if i == 0 {
                        Outcome::RateLimited {
                            retry_after: Some(Duration::from_secs(30)),
                        }
                    } else {
                        Outcome::Done { bytes: 1 }
                    };
                    ExecResult {
                        outcome,
                        follow_ups: Vec::new(),
                    }
                }
            }
        }
        let times = Arc::new(Mutex::new(Vec::new()));
        let exec = Scripted {
            n: Arc::new(AtomicUsize::new(0)),
            times: times.clone(),
        };
        let guard = handle.foreground_guard();
        tokio::spawn(handle.clone().run(exec));
        handle.enqueue_prefetch_article("en".into(), "A".into(), "r".into());
        handle.enqueue_prefetch_article("en".into(), "B".into(), "r".into());
        drop(guard);

        tokio::time::sleep(Duration::from_secs(60)).await;
        let times = times.lock().unwrap().clone();
        assert_eq!(times.len(), 2, "both eventually ran");
        let gap = times[1].duration_since(times[0]);
        assert!(
            gap >= Duration::from_secs(30),
            "second waited out the Retry-After, gap was {gap:?}"
        );
    }
}
