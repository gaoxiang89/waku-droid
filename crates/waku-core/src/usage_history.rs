//! Historical token and cost usage for the settings Usage page, scanned from
//! the provider CLIs' own on-disk session transcripts (Claude Code's
//! `~/.claude/projects`, Codex's `~/.codex/sessions`, and Antigravity's
//! `~/.gemini/antigravity*/brain`) the way T3 Code and `ccusage` do it, so usage
//! covers turns driven outside Waku too. Costs are priced against LiteLLM's
//! model rate table, fetched at most daily and cached beside the app database.
//!
//! Everything here blocks on the filesystem and (for the rate table) the
//! network, and must run on the background executor. Render reads only the
//! [`UsageHistory`] snapshot the app entity stores. Transcripts are
//! append-only, so parsed records are memoised per file by `(size, mtime)` in
//! a [`ScanCache`] the caller owns; warm rescans only reparse changed files.

use std::collections::{HashMap, HashSet};
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Datelike as _, Local, NaiveDate, TimeZone as _, Utc};
use serde_json::Value;

pub use waku_protocol::usage_history::{
    CostQuality, DaySlice, MONTHLY_WINDOW, ModelSlice, MonthSlice, PricingStatus, ProjectSlice,
    ProviderDay, ProviderSlice, TokenTotals, UsageHistory, UsageProvider, UsageWindow,
    WINDOW_CHOICES,
};

/// Files whose mtime predates the window start by more than this are skipped
/// without opening. The slack covers a session whose last write lands just
/// before local midnight on the window's first day.
const MTIME_SLACK: Duration = Duration::from_secs(36 * 3600);

const LITELLM_RATES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const RATES_CACHE_FILE: &str = "usage-model-rates.json";
/// Rates move rarely; a day-old table keeps the page working offline.
const RATES_TTL: Duration = Duration::from_secs(24 * 3600);

/// One usage event parsed out of a transcript line.
#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub provider: UsageProvider,
    pub timestamp_ms: i64,
    pub model: String,
    pub session_id: String,
    /// The working directory the session ran in, as recorded by the CLI —
    /// the per-project view's grouping key. Empty when the transcript does
    /// not say.
    pub project: String,
    pub totals: TokenTotals,
    pub reported_cost_usd: Option<f64>,
    /// Key for cross-file de-duplication, or `None` when the record is
    /// inherently unique and needs no dedup.
    pub dedupe_key: Option<String>,
}

/// Cheap substring gate applied before JSON parsing. Transcripts are mostly
/// tool output; only a minority of lines carry usage, and skipping the rest
/// before `serde_json` sees them is worth an order of magnitude.
fn might_carry_usage(line: &str, provider: UsageProvider) -> bool {
    match provider {
        UsageProvider::Claude => line.contains("\"usage\""),
        UsageProvider::Codex => line.contains("\"token_count\""),
        UsageProvider::Antigravity => {
            line.contains("\"input_tokens\"") || line.contains("\"output_tokens\"")
        }
    }
}

/// Positive finite number as a token count; anything else is zero.
fn int(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.trunc() as u64)
        .unwrap_or(0)
}

fn parse_timestamp_ms(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|date| date.timestamp_millis())
}

/// Parses one line of a Claude Code transcript.
///
/// The CLI writes one record per assistant *content block*, and every one of
/// those records repeats the same complete `usage` object for the parent
/// message. Summing them overcounts severely, so callers must drop repeats by
/// `dedupe_key` and keep the first.
fn parse_claude_line(line: &str) -> Option<UsageRecord> {
    let record: Value = serde_json::from_str(line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = record.get("message")?.as_object()?;
    let usage = message.get("usage")?.as_object()?;
    let timestamp_ms = parse_timestamp_ms(record.get("timestamp"))?;
    let model = message.get("model").and_then(Value::as_str)?;
    if model.is_empty() {
        return None;
    }

    let message_id = message.get("id").and_then(Value::as_str);
    let request_id = record.get("requestId").and_then(Value::as_str);
    // Matches ccusage: prefer the message/request pair, fall back to whichever
    // half exists. Records with neither cannot be de-duplicated.
    let dedupe_key = (message_id.is_some() || request_id.is_some()).then(|| {
        format!(
            "{}:{}",
            message_id.unwrap_or_default(),
            request_id.unwrap_or_default()
        )
    });

    Some(UsageRecord {
        provider: UsageProvider::Claude,
        timestamp_ms,
        model: model.to_owned(),
        session_id: record
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        project: record
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        totals: TokenTotals {
            uncached_input: int(usage.get("input_tokens")),
            cached_input: int(usage.get("cache_read_input_tokens")),
            cache_creation: int(usage.get("cache_creation_input_tokens")),
            output: int(usage.get("output_tokens")),
            // Anthropic folds thinking tokens into output without a breakout.
            reasoning: 0,
        },
        reported_cost_usd: record
            .get("costUSD")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite()),
        dedupe_key,
    })
}

/// Parses one line of an Antigravity `transcript.jsonl` log.
///
/// Only `PLANNER_RESPONSE` entries with non-zero token counts represent billed
/// model turns. Unlike Codex, Antigravity reports `input_tokens` and
/// `cache_read_tokens` as disjoint counts.
fn parse_antigravity_line(line: &str, session_id: &str, project: &str) -> Option<UsageRecord> {
    let record: Value = serde_json::from_str(line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("PLANNER_RESPONSE") {
        return None;
    }
    let uncached_input = int(record.get("input_tokens"));
    let cached_input = int(record.get("cache_read_tokens"));
    let output = int(record.get("output_tokens"));
    if uncached_input == 0 && cached_input == 0 && output == 0 {
        return None;
    }
    let timestamp_ms = parse_timestamp_ms(record.get("created_at"))?;
    let model = record
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown");
    let step_index = record.get("step_index").and_then(|value| {
        value
            .as_u64()
            .map(|index| index.to_string())
            .or_else(|| {
                value
                    .as_i64()
                    .filter(|index| *index >= 0)
                    .map(|index| index.to_string())
            })
            .or_else(|| {
                value
                    .as_str()
                    .filter(|index| !index.is_empty())
                    .map(str::to_owned)
            })
    });
    let dedupe_key = step_index.map(|step| format!("{session_id}:{step}"));

    Some(UsageRecord {
        provider: UsageProvider::Antigravity,
        timestamp_ms,
        model: model.to_owned(),
        session_id: session_id.to_owned(),
        project: project.to_owned(),
        totals: TokenTotals {
            uncached_input,
            cached_input,
            cache_creation: 0,
            output,
            reasoning: 0,
        },
        reported_cost_usd: None,
        dedupe_key,
    })
}

/// Rolling state for a single Codex rollout file. `token_count` events carry
/// no model, so the model is carried forward from the most recent
/// `turn_context`; sessions that switch models mid-run attribute correctly
/// from the switch onward.
#[derive(Clone, Copy)]
enum CodexForkPrefix {
    NotForked,
    /// The first meta marks a fork, but no copied ancestor meta has confirmed
    /// that this physical rollout actually contains a replay prefix.
    AwaitingReplay {
        fork_ms: Option<i64>,
    },
    /// Leading usage after a copied ancestor meta is re-stamped parent history.
    Suppressing {
        anchor_ms: i64,
    },
    Complete,
}

struct CodexScanState {
    model: String,
    session_id: String,
    /// Carried from `session_meta`/`turn_context`, which both record the
    /// working directory.
    cwd: String,
    last_usage_signature: Option<String>,
    saw_session_meta: bool,
    fork_prefix: CodexForkPrefix,
}

impl CodexScanState {
    fn new() -> Self {
        Self {
            model: String::new(),
            session_id: String::new(),
            cwd: String::new(),
            last_usage_signature: None,
            saw_session_meta: false,
            fork_prefix: CodexForkPrefix::NotForked,
        }
    }
}

/// Copied history is written synchronously, while the child's first usage
/// follows a real model turn. This is only a fallback for rollout formats that
/// do not repeat the child's own `session_meta` at the structural boundary.
const FORK_COPY_MAX_GAP_MS: i64 = 1_000;

fn codex_session_id(payload: &serde_json::Map<String, Value>) -> Option<&str> {
    payload
        .get("id")
        .or_else(|| payload.get("session_id"))
        .and_then(Value::as_str)
}

fn is_forked_codex_session(payload: &serde_json::Map<String, Value>) -> bool {
    payload
        .get("forked_from_id")
        .and_then(Value::as_str)
        .is_some()
        || payload
            .get("source")
            .and_then(|source| source.get("subagent"))
            .and_then(|subagent| subagent.get("thread_spawn"))
            .and_then(|spawn| spawn.get("parent_thread_id"))
            .and_then(Value::as_str)
            .is_some()
}

/// Feeds one line of a Codex rollout into `state`, returning a record when the
/// line was a usage event. Deltas come from `last_token_usage`; summing those
/// across a session reconciles with the session's final `total_token_usage`
/// provided consecutive duplicate events are dropped, which this does.
fn parse_codex_line(line: &str, state: &mut CodexScanState) -> Option<UsageRecord> {
    let record: Value = serde_json::from_str(line).ok()?;
    let payload = record.get("payload")?.as_object()?;

    match record.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            let id = codex_session_id(payload);
            if !state.saw_session_meta {
                // The first meta names the physical rollout. Forks replay
                // ancestor metas after it, and those must never replace the
                // child's identity or working directory.
                state.saw_session_meta = true;
                if let Some(id) = id {
                    state.session_id = id.to_owned();
                }
                if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                    state.cwd = cwd.to_owned();
                }
                state.fork_prefix = if is_forked_codex_session(payload) {
                    CodexForkPrefix::AwaitingReplay {
                        fork_ms: parse_timestamp_ms(record.get("timestamp")),
                    }
                } else {
                    CodexForkPrefix::NotForked
                };
                return None;
            }

            match state.fork_prefix {
                CodexForkPrefix::AwaitingReplay { fork_ms } => {
                    if id == Some(state.session_id.as_str()) {
                        // Some Codex writers repeat the child's meta when copied
                        // history ends. This is the authoritative boundary.
                        state.fork_prefix = CodexForkPrefix::Complete;
                    } else if id.is_some() {
                        // A different meta confirms that ancestor history really
                        // follows. A fork marker alone is not enough: subagents can
                        // start with no copied prefix and may report usage quickly.
                        if let Some(anchor_ms) =
                            parse_timestamp_ms(record.get("timestamp")).or(fork_ms)
                        {
                            state.fork_prefix = CodexForkPrefix::Suppressing { anchor_ms };
                        }
                    }
                }
                CodexForkPrefix::Suppressing { anchor_ms } => {
                    if id == Some(state.session_id.as_str()) {
                        state.fork_prefix = CodexForkPrefix::Complete;
                    } else if id.is_some() {
                        state.fork_prefix = CodexForkPrefix::Suppressing {
                            anchor_ms: parse_timestamp_ms(record.get("timestamp"))
                                .unwrap_or(anchor_ms),
                        };
                    }
                }
                CodexForkPrefix::NotForked | CodexForkPrefix::Complete => {}
            }
            return None;
        }
        Some("turn_context") => {
            if let Some(model) = payload.get("model").and_then(Value::as_str) {
                state.model = model.to_owned();
            }
            if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                state.cwd = cwd.to_owned();
            }
            return None;
        }
        _ => {}
    }

    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let last = payload.get("info")?.get("last_token_usage")?.as_object()?;

    // Only an event that is otherwise eligible may consume the duplicate
    // signature. A token_count arriving before its turn_context (no model yet)
    // must not poison it, or the re-emitted copy after the model is known
    // would be skipped as a duplicate and those tokens never counted.
    let timestamp_ms = parse_timestamp_ms(record.get("timestamp"))?;
    if state.model.is_empty() {
        return None;
    }

    // Codex re-emits an unchanged token_count on some stream boundaries.
    // Summing those would double count, so identical consecutive payloads are
    // skipped.
    let signature = serde_json::to_string(last).ok()?;
    if state.last_usage_signature.as_deref() == Some(signature.as_str()) {
        return None;
    }
    state.last_usage_signature = Some(signature);

    if let CodexForkPrefix::Suppressing { anchor_ms } = state.fork_prefix {
        if timestamp_ms.saturating_sub(anchor_ms) < FORK_COPY_MAX_GAP_MS {
            state.fork_prefix = CodexForkPrefix::Suppressing {
                anchor_ms: timestamp_ms,
            };
            return None;
        }
        state.fork_prefix = CodexForkPrefix::Complete;
    }

    let input = int(last.get("input_tokens"));
    let cached_input = int(last.get("cached_input_tokens"));
    let cache_creation = int(last.get("cache_write_input_tokens"));
    let output = int(last.get("output_tokens"));
    let totals = TokenTotals {
        // Codex reports `input_tokens` inclusive of the cached portion.
        uncached_input: input.saturating_sub(cached_input + cache_creation),
        cached_input,
        cache_creation,
        output,
        // Reported inside output_tokens, surfaced separately for the mix.
        reasoning: int(last.get("reasoning_output_tokens")).min(output),
    };
    if totals.total() == 0 {
        return None;
    }
    if matches!(state.fork_prefix, CodexForkPrefix::AwaitingReplay { .. }) {
        // Once this physical rollout emits real usage, a replay prefix cannot
        // begin later in the file.
        state.fork_prefix = CodexForkPrefix::Complete;
    }

    Some(UsageRecord {
        provider: UsageProvider::Codex,
        timestamp_ms,
        model: state.model.clone(),
        session_id: state.session_id.clone(),
        project: state.cwd.clone(),
        totals,
        // Codex does not report cost in the rollout.
        reported_cost_usd: None,
        // Fork copies were suppressed while parsing the physical rollout, so
        // surviving events are unique and need no global deduplication.
        dedupe_key: None,
    })
}

/* ------------------------------------------------------------------------- */
/* Pricing                                                                   */
/* ------------------------------------------------------------------------- */

/// USD per token at the base tier. Tiered variants (long-context, flex,
/// batch) are deliberately ignored: transcripts don't record which tier
/// served a request, so anything else would be a guess dressed as precision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelRate {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
}

#[derive(Clone, Debug)]
pub struct RateTable {
    pub rates: HashMap<String, ModelRate>,
    pub status: PricingStatus,
}

impl RateTable {
    pub fn unavailable() -> Self {
        Self {
            rates: HashMap::new(),
            status: PricingStatus::Unavailable,
        }
    }
}

/// Models never priced regardless of the table: `<synthetic>` marks locally
/// generated messages that were never billed, and bare family names are
/// ambiguous across generations, so they report as unpriced instead of
/// guessing one.
const UNPRICEABLE_MODELS: [&str; 6] = [
    "<synthetic>",
    "synthetic",
    "opus",
    "sonnet",
    "haiku",
    "fable",
];

/// Canonicalises a model name for lookup: strips a `provider/` prefix
/// (LiteLLM publishes both `claude-opus-5` and `anthropic/claude-opus-5`) and
/// lowercases, since transcripts are inconsistent about casing.
fn normalize_model_name(model: &str) -> String {
    let trimmed = model.trim().to_ascii_lowercase();
    match trimmed.rfind('/') {
        Some(slash) => trimmed[slash + 1..].to_owned(),
        None => trimmed,
    }
}

/// Antigravity encodes reasoning effort on the model id (`gemini-3.8-flash-low`)
/// while LiteLLM indexes the base model name. Longest suffixes come first so
/// `-xhigh` is never misread as `-high`.
const EFFORT_SUFFIXES: [&str; 5] = ["-xhigh", "-medium", "-high", "-low", "-max"];

fn lookup_rate<'a>(table: &'a RateTable, model: &str) -> Option<&'a ModelRate> {
    let normalized = normalize_model_name(model);
    if normalized.is_empty() || UNPRICEABLE_MODELS.contains(&normalized.as_str()) {
        return None;
    }
    if let Some(rate) = table.rates.get(&normalized) {
        return Some(rate);
    }
    for suffix in EFFORT_SUFFIXES {
        if let Some(base) = normalized.strip_suffix(suffix) {
            if !base.is_empty() && !UNPRICEABLE_MODELS.contains(&base) {
                if let Some(rate) = table.rates.get(base) {
                    return Some(rate);
                }
            }
            break;
        }
    }
    None
}

/// Projects the LiteLLM document into a rate table. Entries without both an
/// input and an output rate are dropped: a half-priced model would silently
/// under-report cost, which is worse than reporting it as unpriced.
fn parse_rate_table(document: &Value) -> HashMap<String, ModelRate> {
    let mut table = HashMap::new();
    let Some(entries) = document.as_object() else {
        return table;
    };
    let finite = |value: Option<&Value>| value.and_then(Value::as_f64).filter(|v| v.is_finite());
    for (name, entry) in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(input) = finite(entry.get("input_cost_per_token")) else {
            continue;
        };
        let Some(output) = finite(entry.get("output_cost_per_token")) else {
            continue;
        };
        table.insert(
            normalize_model_name(name),
            ModelRate {
                input,
                output,
                // Anthropic bills cache reads at a discount and writes at a
                // premium. When a model omits them, cached input is priced as
                // plain input rather than as free.
                cache_read: finite(entry.get("cache_read_input_token_cost")).unwrap_or(input),
                cache_creation: finite(entry.get("cache_creation_input_token_cost"))
                    .unwrap_or(input),
            },
        );
    }
    table
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn read_rates_cache(path: &Path) -> Option<(i64, HashMap<String, ModelRate>)> {
    let document: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let fetched_at_ms = document.get("fetched_at_ms")?.as_i64()?;
    let mut rates = HashMap::new();
    for (name, entry) in document.get("rates")?.as_object()? {
        let values = entry.as_array()?;
        let field = |index: usize| values.get(index).and_then(Value::as_f64);
        rates.insert(
            name.clone(),
            ModelRate {
                input: field(0)?,
                output: field(1)?,
                cache_read: field(2)?,
                cache_creation: field(3)?,
            },
        );
    }
    Some((fetched_at_ms, rates))
}

fn write_rates_cache(path: &Path, fetched_at_ms: i64, rates: &HashMap<String, ModelRate>) {
    let entries: serde_json::Map<String, Value> = rates
        .iter()
        .map(|(name, rate)| {
            (
                name.clone(),
                serde_json::json!([
                    rate.input,
                    rate.output,
                    rate.cache_read,
                    rate.cache_creation
                ]),
            )
        })
        .collect();
    let document = serde_json::json!({ "fetched_at_ms": fetched_at_ms, "rates": entries });
    // Best effort: a cache that fails to write only costs the next scan a
    // refetch.
    let _ = std::fs::write(path, document.to_string());
}

/// Load the LiteLLM rate table, preferring the on-disk snapshot while it is
/// within TTL, refetching otherwise, and degrading to the stale snapshot or an
/// empty table rather than failing the scan. Blocking: disk plus up to one
/// HTTPS round trip. Never call from the UI thread.
pub fn load_rate_table(cache_dir: &Path) -> RateTable {
    let cache_path = cache_dir.join(RATES_CACHE_FILE);
    let disk = read_rates_cache(&cache_path).filter(|(_, rates)| !rates.is_empty());
    let now_ms = unix_time_ms();
    if let Some((fetched_at_ms, rates)) = &disk
        && now_ms.saturating_sub(*fetched_at_ms) < RATES_TTL.as_millis() as i64
    {
        return RateTable {
            rates: rates.clone(),
            status: PricingStatus::Cached,
        };
    }

    let fetched =
        crate::usage::http_get(LITELLM_RATES_URL, &["Accept: application/json".to_owned()])
            .ok()
            .filter(|(status, _)| *status == 200)
            .and_then(|(_, body)| serde_json::from_str::<Value>(&body).ok())
            .map(|document| parse_rate_table(&document))
            .filter(|rates| !rates.is_empty());

    match (fetched, disk) {
        (Some(rates), _) => {
            write_rates_cache(&cache_path, now_ms, &rates);
            RateTable {
                rates,
                status: PricingStatus::Fresh,
            }
        }
        (None, Some((_, rates))) => RateTable {
            rates,
            status: PricingStatus::Cached,
        },
        (None, None) => RateTable::unavailable(),
    }
}

/* ------------------------------------------------------------------------- */
/* Scanning                                                                  */
/* ------------------------------------------------------------------------- */

/// Parsed records for one transcript, memoised by `(size, mtime, provider)`.
/// Records are stored unfiltered by window; the aggregation applies the day
/// filter, so one cache serves every window length.
pub struct FileCacheEntry {
    size: u64,
    mtime_ms: i64,
    provider: UsageProvider,
    records: Vec<UsageRecord>,
}

pub type ScanCache = HashMap<PathBuf, FileCacheEntry>;

const ANTIGRAVITY_DATA_ROOTS: [&str; 3] = [
    ".gemini/antigravity-cli",
    ".gemini/antigravity-acp",
    ".gemini/antigravity",
];

fn antigravity_data_root(path: &Path) -> Option<PathBuf> {
    if path.file_name().and_then(|name| name.to_str()) == Some("sidecar_data") {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
    } else {
        Some(path.to_path_buf())
    }
}

fn antigravity_brain_roots_from(
    exec_data_dir: Option<&std::ffi::OsStr>,
    home_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push_data_root = |data_root: PathBuf| {
        let brain = data_root.join("brain");
        if !roots.contains(&brain) {
            roots.push(brain);
        }
    };
    if let Some(exec_dir) = exec_data_dir.filter(|value| !value.is_empty())
        && let Some(root) = antigravity_data_root(Path::new(exec_dir))
    {
        push_data_root(root);
    }
    if let Some(home) = home_dir {
        for relative in ANTIGRAVITY_DATA_ROOTS {
            push_data_root(home.join(relative));
        }
    }
    roots
}

/// Candidate transcript roots scanned for one provider, in priority order.
fn provider_roots(provider: UsageProvider) -> Vec<PathBuf> {
    match provider {
        UsageProvider::Claude => match crate::command_env::env_var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) if !dir.is_empty() => vec![PathBuf::from(dir).join("projects")],
            _ => dirs::home_dir()
                .into_iter()
                .map(|home| home.join(".claude/projects"))
                .collect(),
        },
        UsageProvider::Codex => match crate::command_env::env_var_os("CODEX_HOME") {
            Some(dir) if !dir.is_empty() => vec![PathBuf::from(dir).join("sessions")],
            _ => dirs::home_dir()
                .into_iter()
                .map(|home| home.join(".codex/sessions"))
                .collect(),
        },
        UsageProvider::Antigravity => {
            let exec_data_dir = crate::command_env::env_var_os("ANTIGRAVITY_EXECUTABLE_DATA_DIR");
            let home = dirs::home_dir();
            antigravity_brain_roots_from(exec_data_dir.as_deref(), home.as_deref())
        }
    }
}

/// Lists transcripts under `root` modified at or after `since_ms`.
/// Errors on individual entries are swallowed: session files rotate and get
/// removed while the walk is in flight, and a partial listing beats failing
/// the page. Returns the number of files skipped by the mtime prefilter.
fn list_transcript_files(
    root: &Path,
    provider: UsageProvider,
    since_ms: i64,
    found: &mut Vec<(PathBuf, u64, i64)>,
) -> usize {
    if provider == UsageProvider::Antigravity {
        return list_antigravity_transcript_files(root, since_ms, found);
    }
    list_recursive_jsonl_files(root, since_ms, found)
}

fn list_recursive_jsonl_files(
    root: &Path,
    since_ms: i64,
    found: &mut Vec<(PathBuf, u64, i64)>,
) -> usize {
    let mut skipped = 0;
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            skipped += list_recursive_jsonl_files(&path, since_ms, found);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        if mtime_ms < since_ms {
            skipped += 1;
            continue;
        }
        found.push((path, metadata.len(), mtime_ms));
    }
    skipped
}

/// Antigravity stores both `transcript.jsonl` and `transcript_full.jsonl`
/// under `<brain_root>/<conv_id>/.system_generated/logs/`; only the compact
/// `transcript.jsonl` is scanned so steps are never double-counted.
fn list_antigravity_transcript_files(
    brain_root: &Path,
    since_ms: i64,
    found: &mut Vec<(PathBuf, u64, i64)>,
) -> usize {
    let mut skipped = 0;
    let Ok(entries) = std::fs::read_dir(brain_root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry
            .path()
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        if mtime_ms < since_ms {
            skipped += 1;
            continue;
        }
        found.push((path, metadata.len(), mtime_ms));
    }
    skipped
}

fn antigravity_conversation_id(transcript_path: &Path) -> Option<String> {
    transcript_path
        .parent()?
        .parent()?
        .parent()?
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn normalize_workspace_path_string(mut text: String) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' {
        let upper = (bytes[0] as char).to_ascii_uppercase();
        let mut normalized = String::with_capacity(text.len());
        normalized.push(upper);
        normalized.push_str(&text[1..]);
        text = normalized;
    }
    text
}

fn antigravity_uri_to_workspace_path(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    let path = parsed.to_file_path().ok()?;
    let text = normalize_workspace_path_string(path.to_string_lossy().into_owned());
    (!text.is_empty()).then_some(text)
}

fn antigravity_workspace_from_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("file://") {
        return antigravity_uri_to_workspace_path(trimmed);
    }
    Some(normalize_workspace_path_string(trimmed.to_owned()))
}

#[cfg(test)]
static ANTIGRAVITY_WORKSPACE_MAP_LOADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Loads `conversation_id -> workspace_path` once per Antigravity brain root,
/// preferring `<brain_root>/../conversation_summaries.db` and falling back to
/// `<brain_root>/../history.jsonl` for unmapped conversations.
fn load_antigravity_workspace_map(brain_root: &Path) -> HashMap<String, String> {
    #[cfg(test)]
    ANTIGRAVITY_WORKSPACE_MAP_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut map = HashMap::new();
    let Some(data_root) = brain_root.parent() else {
        return map;
    };

    let db_path = data_root.join("conversation_summaries.db");
    if db_path.is_file()
        && let Ok(conn) = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    {
        let _ = conn.busy_timeout(Duration::from_millis(100));
        if let Ok(mut stmt) = conn.prepare(
            "SELECT conversation_id, workspace_uris FROM conversation_summaries WHERE workspace_uris != ''",
        ) && let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                let (conv_id, uris_json) = row;
                if conv_id.is_empty() {
                    continue;
                }
                let Ok(uris) = serde_json::from_str::<Vec<String>>(&uris_json) else {
                    continue;
                };
                if let Some(workspace) = uris
                    .iter()
                    .find_map(|uri| antigravity_uri_to_workspace_path(uri))
                {
                    map.entry(conv_id).or_insert(workspace);
                }
            }
        }
    }

    let history_path = data_root.join("history.jsonl");
    if let Ok(file) = std::fs::File::open(&history_path) {
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if !line.contains("conversationId") || !line.contains("workspace") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(conv_id) = value
                .get("conversationId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            if map.contains_key(conv_id) {
                continue;
            }
            if let Some(workspace) = value
                .get("workspace")
                .and_then(Value::as_str)
                .and_then(antigravity_workspace_from_value)
            {
                map.insert(conv_id.to_owned(), workspace);
            }
        }
    }

    map
}

/// Streams one Antigravity `transcript.jsonl` and returns its deduplicated
/// usage records, or `None` on read error.
fn read_antigravity_records(
    path: &Path,
    session_id: &str,
    project: &str,
) -> Option<Vec<UsageRecord>> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut records = Vec::new();
    let mut seen_in_file = HashSet::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
        if !might_carry_usage(&line, UsageProvider::Antigravity) {
            continue;
        }
        if let Some(record) = parse_antigravity_line(&line, session_id, project) {
            if let Some(key) = &record.dedupe_key
                && !seen_in_file.insert(key.clone())
            {
                continue;
            }
            records.push(record);
        }
    }
    Some(records)
}

/// Streams one transcript and returns the usage records it contains, already
/// de-duplicated within the file, or `None` when it could not be read. The
/// distinction matters to the cache: an empty transcript is a stable fact
/// worth memoising, while a transient read failure memoised under the same
/// `(size, mtime)` would silently drop the file's usage until it changes.
fn read_transcript_records(path: &Path, provider: UsageProvider) -> Option<Vec<UsageRecord>> {
    if provider == UsageProvider::Antigravity {
        let session_id = antigravity_conversation_id(path).unwrap_or_default();
        return read_antigravity_records(path, &session_id, "");
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut records = Vec::new();
    let mut codex_state = CodexScanState::new();
    let mut seen_in_file = HashSet::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
        match provider {
            UsageProvider::Codex => {
                // Codex carries the active model on `turn_context` lines that
                // hold no usage of their own, so those still pass through the
                // reducer to keep model attribution correct.
                if !might_carry_usage(&line, provider)
                    && !line.contains("\"turn_context\"")
                    && !line.contains("\"session_meta\"")
                {
                    continue;
                }
                if let Some(record) = parse_codex_line(&line, &mut codex_state) {
                    records.push(record);
                }
            }
            UsageProvider::Claude => {
                if !might_carry_usage(&line, provider) {
                    continue;
                }
                if let Some(record) = parse_claude_line(&line) {
                    // Every assistant content block repeats the parent
                    // message's usage; the first record wins. Cross-file
                    // repeats (resumed or forked sessions) are handled by the
                    // aggregator's global pass.
                    if let Some(key) = &record.dedupe_key {
                        if !seen_in_file.insert(key.clone()) {
                            continue;
                        }
                    }
                    records.push(record);
                }
            }
            UsageProvider::Antigravity => unreachable!("handled above"),
        }
    }
    Some(records)
}

/* ------------------------------------------------------------------------- */
/* Aggregation                                                               */
/* ------------------------------------------------------------------------- */

#[derive(Clone, Copy, PartialEq)]
enum CostSource {
    Reported,
    Priced,
    Unpriced,
}

#[derive(Default)]
struct Bucket {
    totals: TokenTotals,
    cost_usd: f64,
    cache_savings_usd: f64,
    records: u64,
    unpriced_records: u64,
    reported_records: u64,
}

/// Per-project accumulation, keyed by the resolved project path.
#[derive(Default)]
struct ProjectAccumulator {
    cost_usd: f64,
    total_tokens: u64,
    by_provider: [ProviderDay; 3],
    sessions: HashSet<(UsageProvider, String)>,
    /// Cost per model, for the row's "top models" caption.
    models: HashMap<String, f64>,
    last_day: Option<NaiveDate>,
}

/// Folds records into `(day, provider, model)` buckets with global cross-file
/// de-duplication, then derives the page's view.
struct Aggregator {
    since_day: NaiveDate,
    until_day: NaiveDate,
    /// Known project roots, longest path first. A record whose working
    /// directory sits under a root is attributed to that root, so launches
    /// from a repo's subdirectories and worktrees inside it merge into one
    /// project row instead of splintering by cwd.
    project_roots: Vec<PathBuf>,
    buckets: HashMap<(NaiveDate, UsageProvider, String), Bucket>,
    seen: HashSet<String>,
    sessions: HashSet<(UsageProvider, String)>,
    /// Sessions per calendar month (keyed by the month's first day). A
    /// session spanning two months counts in both, which is what "sessions
    /// that month" means.
    month_sessions: HashSet<(NaiveDate, UsageProvider, String)>,
    projects: HashMap<String, ProjectAccumulator>,
}

impl Aggregator {
    fn new(since_day: NaiveDate, until_day: NaiveDate, project_roots: &[PathBuf]) -> Self {
        let mut project_roots = project_roots.to_vec();
        project_roots.sort_by_key(|root| std::cmp::Reverse(root.as_os_str().len()));
        Self {
            since_day,
            until_day,
            project_roots,
            buckets: HashMap::new(),
            seen: HashSet::new(),
            sessions: HashSet::new(),
            month_sessions: HashSet::new(),
            projects: HashMap::new(),
        }
    }

    /// The project a working directory belongs to: the longest known root
    /// containing it, else the directory itself.
    fn resolve_project(&self, cwd: &str) -> String {
        if cwd.is_empty() {
            return String::new();
        }
        let path = Path::new(cwd);
        self.project_roots
            .iter()
            .find(|root| path.starts_with(root))
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| cwd.to_owned())
    }

    fn add(&mut self, record: &UsageRecord, rates: &RateTable) {
        if let Some(key) = &record.dedupe_key {
            if !self.seen.insert(key.clone()) {
                return;
            }
        }
        let Some(timestamp) = Utc.timestamp_millis_opt(record.timestamp_ms).single() else {
            return;
        };
        let day = timestamp.with_timezone(&Local).date_naive();
        if day < self.since_day || day > self.until_day {
            return;
        }

        let (cost_usd, source) = match record.reported_cost_usd {
            Some(cost) => (cost, CostSource::Reported),
            None => match lookup_rate(rates, &record.model) {
                Some(rate) => (
                    record.totals.uncached_input as f64 * rate.input
                        + record.totals.cached_input as f64 * rate.cache_read
                        + record.totals.cache_creation as f64 * rate.cache_creation
                        + record.totals.output as f64 * rate.output,
                    CostSource::Priced,
                ),
                None => (0.0, CostSource::Unpriced),
            },
        };
        // What the cached input would have cost at full input rates, minus
        // what it actually cost. Drives the "cache savings" figure.
        let cache_savings_usd = lookup_rate(rates, &record.model)
            .map(|rate| record.totals.cached_input as f64 * (rate.input - rate.cache_read))
            .unwrap_or(0.0);

        let bucket = self
            .buckets
            .entry((day, record.provider, record.model.clone()))
            .or_default();
        bucket.totals.add(&record.totals);
        bucket.cost_usd += cost_usd;
        bucket.cache_savings_usd += cache_savings_usd;
        bucket.records += 1;
        match source {
            CostSource::Unpriced => bucket.unpriced_records += 1,
            CostSource::Reported => bucket.reported_records += 1,
            CostSource::Priced => {}
        }
        if !record.session_id.is_empty() {
            self.sessions
                .insert((record.provider, record.session_id.clone()));
            self.month_sessions.insert((
                first_of_month(day),
                record.provider,
                record.session_id.clone(),
            ));
        }

        let tokens = record.totals.total();
        let project_key = self.resolve_project(&record.project);
        let project = self.projects.entry(project_key).or_default();
        project.cost_usd += cost_usd;
        project.total_tokens += tokens;
        project.by_provider[record.provider.index()].cost_usd += cost_usd;
        project.by_provider[record.provider.index()].total_tokens += tokens;
        if !record.session_id.is_empty() {
            project
                .sessions
                .insert((record.provider, record.session_id.clone()));
        }
        *project.models.entry(record.model.clone()).or_default() += cost_usd;
        project.last_day = Some(project.last_day.map_or(day, |last| last.max(day)));
    }
}

fn account_transcript_root(provider: UsageProvider, config_dir: &Path) -> PathBuf {
    match provider {
        UsageProvider::Claude => config_dir.join("projects"),
        UsageProvider::Codex => config_dir.join("sessions"),
        UsageProvider::Antigravity => antigravity_data_root(config_dir)
            .unwrap_or_else(|| config_dir.to_path_buf())
            .join("brain"),
    }
}

fn collect_scan_roots(
    default_roots: impl IntoIterator<Item = (UsageProvider, PathBuf)>,
    account_dirs: &[(UsageProvider, PathBuf)],
) -> Vec<(UsageProvider, PathBuf)> {
    let mut roots_to_scan = Vec::new();
    let mut seen_roots = HashSet::new();
    for (provider, root) in default_roots {
        let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if seen_roots.insert((provider, canonical)) {
            roots_to_scan.push((provider, root));
        }
    }
    for (provider, config_dir) in account_dirs {
        let root = account_transcript_root(*provider, config_dir);
        let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if seen_roots.insert((*provider, canonical)) {
            roots_to_scan.push((*provider, root));
        }
    }
    roots_to_scan
}

/// Scan every provider's transcripts for `window` ending today and derive
/// the page snapshot. `project_roots` are the app's known project paths, used
/// to attribute working directories to projects. Blocking; run on the
/// background executor with the caller-owned `cache` and a loaded rate table.
pub fn scan(
    cache: &mut ScanCache,
    rates: &RateTable,
    window: UsageWindow,
    project_roots: &[PathBuf],
) -> UsageHistory {
    scan_with_account_dirs(cache, rates, window, project_roots, &[])
}

/// Like [`scan`], additionally scanning custom account configuration roots
/// configured in `DaemonSettings::provider_accounts`.
pub fn scan_with_account_dirs(
    cache: &mut ScanCache,
    rates: &RateTable,
    window: UsageWindow,
    project_roots: &[PathBuf],
    account_dirs: &[(UsageProvider, PathBuf)],
) -> UsageHistory {
    let started = Instant::now();
    let (since_day, until_day) = window.bounds(Local::now().date_naive());
    // Local midnight on the window's first day; a DST gap falls back to the
    // day boundary in UTC, which the mtime slack absorbs anyway.
    let window_start_ms = since_day
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
        .map(|midnight| midnight.timestamp_millis())
        .unwrap_or_else(|| unix_time_ms() - (until_day - since_day).num_days().max(1) * 86_400_000);
    let mtime_cutoff_ms = window_start_ms - MTIME_SLACK.as_millis() as i64;

    let mut aggregator = Aggregator::new(since_day, until_day, project_roots);
    let mut scanned_files = 0;
    let mut skipped_files = 0;
    let mut errors = Vec::new();

    let default_roots = UsageProvider::ALL.into_iter().flat_map(|provider| {
        provider_roots(provider)
            .into_iter()
            .map(move |root| (provider, root))
    });
    let roots_to_scan = collect_scan_roots(default_roots, account_dirs);

    for (provider, root) in roots_to_scan {
        if !root.is_dir() {
            // Provider or account never used on this machine; zero usage, not an error.
            continue;
        }
        let mut files = Vec::new();
        skipped_files += list_transcript_files(&root, provider, mtime_cutoff_ms, &mut files);
        if files.is_empty() && std::fs::read_dir(&root).is_err() {
            errors.push(format!(
                "{} transcripts at {} could not be read.",
                provider.label(),
                root.display()
            ));
            continue;
        }
        let mut workspace_map: Option<HashMap<String, String>> = None;
        for (path, size, mtime_ms) in files {
            scanned_files += 1;
            // Provider is part of the identity: if both providers were ever
            // pointed at one directory, a hit parsed by the other parser must
            // not be reused.
            let cache_hit = cache.get(&path).is_some_and(|entry| {
                entry.size == size && entry.mtime_ms == mtime_ms && entry.provider == provider
            });
            if cache_hit {
                if provider == UsageProvider::Antigravity
                    && let Some(entry) = cache.get_mut(&path)
                    && entry.records.iter().any(|record| record.project.is_empty())
                {
                    let conv_id = antigravity_conversation_id(&path).unwrap_or_default();
                    let map = workspace_map
                        .get_or_insert_with(|| load_antigravity_workspace_map(&root));
                    if let Some(antigravity_project) = map.get(&conv_id).filter(|p| !p.is_empty()) {
                        for record in &mut entry.records {
                            if record.project.is_empty() {
                                record.project = antigravity_project.clone();
                            }
                        }
                    }
                }
            } else {
                let parsed = match provider {
                    UsageProvider::Antigravity => {
                        let conv_id = antigravity_conversation_id(&path).unwrap_or_default();
                        let map = workspace_map
                            .get_or_insert_with(|| load_antigravity_workspace_map(&root));
                        let antigravity_project = map.get(&conv_id).cloned().unwrap_or_default();
                        read_antigravity_records(&path, &conv_id, &antigravity_project)
                    }
                    UsageProvider::Claude | UsageProvider::Codex => {
                        read_transcript_records(&path, provider)
                    }
                };
                // A read failure is not an empty transcript: caching it
                // under this (size, mtime) would silently drop the file's
                // usage until it changes.
                let Some(records) = parsed else {
                    continue;
                };
                cache.insert(
                    path.clone(),
                    FileCacheEntry {
                        size,
                        mtime_ms,
                        provider,
                        records,
                    },
                );
            }
            if let Some(entry) = cache.get(&path) {
                for record in &entry.records {
                    aggregator.add(record, rates);
                }
            }
        }
    }

    derive_history(
        aggregator,
        window,
        since_day,
        until_day,
        rates.status,
        scanned_files,
        skipped_files,
        errors,
        started.elapsed(),
    )
}

#[allow(clippy::too_many_arguments)]
fn derive_history(
    aggregator: Aggregator,
    window: UsageWindow,
    since_day: NaiveDate,
    until_day: NaiveDate,
    pricing: PricingStatus,
    scanned_files: usize,
    skipped_files: usize,
    errors: Vec<String>,
    scan_duration: Duration,
) -> UsageHistory {
    let mut totals = TokenTotals::default();
    let mut cost_usd = 0.0;
    let mut cache_savings_usd = 0.0;
    let mut records = 0;
    let mut unpriced_records = 0;
    let mut reported_records = 0;
    let mut providers: HashMap<UsageProvider, (f64, u64)> = HashMap::new();
    let mut models: HashMap<(UsageProvider, String), (f64, u64)> = HashMap::new();
    let mut daily: HashMap<NaiveDate, DaySlice> = HashMap::new();
    let mut month_models: HashMap<(NaiveDate, String), f64> = HashMap::new();

    for ((day, provider, model), bucket) in &aggregator.buckets {
        let tokens = bucket.totals.total();
        totals.add(&bucket.totals);
        cost_usd += bucket.cost_usd;
        cache_savings_usd += bucket.cache_savings_usd;
        records += bucket.records;
        unpriced_records += bucket.unpriced_records;
        reported_records += bucket.reported_records;

        let provider_entry = providers.entry(*provider).or_default();
        provider_entry.0 += bucket.cost_usd;
        provider_entry.1 += tokens;

        let model_entry = models.entry((*provider, model.clone())).or_default();
        model_entry.0 += bucket.cost_usd;
        model_entry.1 += tokens;

        let day_entry = daily.entry(*day).or_insert_with(|| DaySlice {
            day: *day,
            cost_usd: 0.0,
            total_tokens: 0,
            by_provider: [ProviderDay::default(); 3],
        });
        day_entry.cost_usd += bucket.cost_usd;
        day_entry.total_tokens += tokens;
        day_entry.by_provider[provider.index()].cost_usd += bucket.cost_usd;
        day_entry.by_provider[provider.index()].total_tokens += tokens;

        *month_models
            .entry((first_of_month(*day), model.clone()))
            .or_default() += bucket.cost_usd;
    }

    let total_tokens = totals.total();
    let share = |part: f64, whole: f64| if whole == 0.0 { 0.0 } else { part / whole };

    let mut provider_slices: Vec<ProviderSlice> = providers
        .into_iter()
        .map(
            |(provider, (provider_cost, provider_tokens))| ProviderSlice {
                provider,
                cost_usd: provider_cost,
                total_tokens: provider_tokens,
                cost_share: share(provider_cost, cost_usd),
                token_share: share(provider_tokens as f64, total_tokens as f64),
            },
        )
        .collect();
    provider_slices.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));

    let mut model_slices: Vec<ModelSlice> = models
        .into_iter()
        .map(
            |((provider, model), (model_cost, model_tokens))| ModelSlice {
                provider,
                model,
                cost_usd: model_cost,
                total_tokens: model_tokens,
                cost_share: share(model_cost, cost_usd),
            },
        )
        .collect();
    model_slices.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.total_tokens.cmp(&a.total_tokens))
    });

    let mut day_slices: Vec<DaySlice> = daily.into_values().collect();
    day_slices.sort_by_key(|slice| slice.day);

    // Months fold over the day slices; only sessions and the model caption
    // need their own record-level accumulators.
    let mut months: HashMap<NaiveDate, MonthSlice> = HashMap::new();
    for day in &day_slices {
        let month = months
            .entry(first_of_month(day.day))
            .or_insert_with(|| MonthSlice {
                first_day: first_of_month(day.day),
                cost_usd: 0.0,
                total_tokens: 0,
                by_provider: [ProviderDay::default(); 3],
                sessions: 0,
                active_days: 0,
                top_models: Vec::new(),
            });
        month.cost_usd += day.cost_usd;
        month.total_tokens += day.total_tokens;
        for index in 0..month.by_provider.len() {
            month.by_provider[index].cost_usd += day.by_provider[index].cost_usd;
            month.by_provider[index].total_tokens += day.by_provider[index].total_tokens;
        }
        if day.total_tokens > 0 {
            month.active_days += 1;
        }
    }
    for (first_day, _, _) in &aggregator.month_sessions {
        if let Some(month) = months.get_mut(first_day) {
            month.sessions += 1;
        }
    }
    for ((first_day, model), model_cost) in month_models {
        if let Some(month) = months.get_mut(&first_day) {
            month.top_models.push((model, model_cost));
        }
    }
    let mut month_slices: Vec<MonthSlice> = months.into_values().collect();
    month_slices.sort_by_key(|slice| slice.first_day);
    for month in &mut month_slices {
        month.top_models.sort_by(|a, b| b.1.total_cmp(&a.1));
    }

    let mut project_slices: Vec<ProjectSlice> = aggregator
        .projects
        .into_iter()
        .map(|(path, project)| {
            let mut top_models: Vec<(String, f64)> = project.models.into_iter().collect();
            top_models.sort_by(|a, b| b.1.total_cmp(&a.1));
            ProjectSlice {
                path,
                cost_usd: project.cost_usd,
                total_tokens: project.total_tokens,
                by_provider: project.by_provider,
                sessions: project.sessions.len() as u64,
                cost_share: share(project.cost_usd, cost_usd),
                last_day: project.last_day,
                top_models,
            }
        })
        .collect();
    project_slices.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.total_tokens.cmp(&a.total_tokens))
    });

    let record_share = |part: u64| share(part as f64, records as f64);
    UsageHistory {
        window,
        since_day,
        until_day,
        totals,
        total_tokens,
        cost_usd,
        records,
        sessions: aggregator.sessions.len() as u64,
        providers: provider_slices,
        models: model_slices,
        daily: day_slices,
        months: month_slices,
        projects: project_slices,
        quality: CostQuality {
            provider_reported_share: record_share(reported_records),
            model_priced_share: record_share(records - reported_records - unpriced_records),
            unpriced_share: record_share(unpriced_records),
            cache_savings_usd,
        },
        pricing,
        scanned_files,
        skipped_files,
        errors,
        scan_duration,
    }
}

/// Inclusive day list between the window bounds, oldest first — the chart's
/// x-axis, including days with no activity.
pub fn enumerate_days(since_day: NaiveDate, until_day: NaiveDate) -> Vec<NaiveDate> {
    let mut days = Vec::new();
    let mut cursor = since_day;
    while cursor <= until_day {
        days.push(cursor);
        cursor = cursor + chrono::Days::new(1);
    }
    days
}

/// The first day of `day`'s calendar month.
pub fn first_of_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap_or(day)
}

/// Inclusive first-of-month list between the bounds' months, oldest first —
/// the statement view's rows, including months with no activity.
pub fn enumerate_months(since_day: NaiveDate, until_day: NaiveDate) -> Vec<NaiveDate> {
    let mut months = Vec::new();
    let mut cursor = first_of_month(since_day);
    let last = first_of_month(until_day);
    while cursor <= last {
        months.push(cursor);
        let Some(next) = cursor.checked_add_months(chrono::Months::new(1)) else {
            break;
        };
        cursor = next;
    }
    months
}

/// Number of days in `first_day`'s month.
pub fn days_in_month(first_day: NaiveDate) -> u32 {
    first_day
        .checked_add_months(chrono::Months::new(1))
        .and_then(|next| next.pred_opt())
        .map(|last| last.day())
        .unwrap_or(31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_table(entries: &[(&str, ModelRate)]) -> RateTable {
        RateTable {
            rates: entries
                .iter()
                .map(|(name, rate)| ((*name).to_owned(), *rate))
                .collect(),
            status: PricingStatus::Fresh,
        }
    }

    const FLAT_RATE: ModelRate = ModelRate {
        input: 1e-6,
        output: 2e-6,
        cache_read: 1e-7,
        cache_creation: 1.25e-6,
    };

    fn codex_meta(
        timestamp: &str,
        id: &str,
        cwd: &str,
        forked_from_id: Option<&str>,
        parent_thread_id: Option<&str>,
    ) -> String {
        let mut payload = serde_json::json!({ "id": id, "cwd": cwd });
        if let Some(parent) = forked_from_id {
            payload["forked_from_id"] = Value::String(parent.to_owned());
        }
        if let Some(parent) = parent_thread_id {
            payload["source"] = serde_json::json!({
                "subagent": { "thread_spawn": { "parent_thread_id": parent } }
            });
        }
        serde_json::json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": payload,
        })
        .to_string()
    }

    fn codex_context(timestamp: &str, cwd: &str) -> String {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "turn_context",
            "payload": { "model": "gpt-5.3-codex", "cwd": cwd },
        })
        .to_string()
    }

    fn codex_count(timestamp: &str, input: u64, output: u64) -> String {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": 0,
                        "cache_write_input_tokens": 0,
                        "output_tokens": output,
                        "reasoning_output_tokens": 0,
                    }
                }
            },
        })
        .to_string()
    }

    #[test]
    fn claude_lines_parse_usage_and_dedupe_key() {
        // Shape captured from a live transcript on 2026-08-08.
        let line = r#"{"type":"assistant","timestamp":"2026-08-08T15:18:37.487Z",
            "requestId":"req_1","sessionId":"session-1","costUSD":null,
            "cwd":"/Users/me/dev/waku",
            "message":{"id":"msg_1","model":"claude-fable-5",
            "usage":{"input_tokens":2,"cache_creation_input_tokens":50700,
            "cache_read_input_tokens":0,"output_tokens":1238}}}"#
            .replace('\n', " ");
        let record = parse_claude_line(&line).expect("the line carries usage");
        assert_eq!(record.provider, UsageProvider::Claude);
        assert_eq!(record.model, "claude-fable-5");
        assert_eq!(record.session_id, "session-1");
        assert_eq!(record.project, "/Users/me/dev/waku");
        assert_eq!(record.dedupe_key.as_deref(), Some("msg_1:req_1"));
        assert_eq!(record.reported_cost_usd, None);
        assert_eq!(record.totals.uncached_input, 2);
        assert_eq!(record.totals.cache_creation, 50700);
        assert_eq!(record.totals.output, 1238);
        assert_eq!(record.totals.total(), 2 + 50700 + 1238);

        // Non-assistant lines and lines without usage parse to nothing.
        assert!(parse_claude_line(r#"{"type":"user","message":{}}"#).is_none());
    }

    #[test]
    fn codex_lines_carry_model_forward_and_skip_duplicates() {
        let mut state = CodexScanState::new();
        let meta = r#"{"timestamp":"2026-08-06T16:31:19.166Z","type":"session_meta",
            "payload":{"id":"codex-session","cwd":"/Users/me/dev/waku"}}"#
            .replace('\n', " ");
        let context = r#"{"timestamp":"2026-08-06T16:31:20.000Z","type":"turn_context",
            "payload":{"model":"gpt-5.3-codex"}}"#
            .replace('\n', " ");
        let count = r#"{"timestamp":"2026-08-06T16:31:25.000Z","type":"event_msg",
            "payload":{"type":"token_count","info":{"last_token_usage":{
            "input_tokens":21047,"cached_input_tokens":1000,"cache_write_input_tokens":47,
            "output_tokens":280,"reasoning_output_tokens":165}}}}"#
            .replace('\n', " ");

        // A token_count before any turn_context has no model and is dropped
        // without consuming the duplicate signature.
        assert!(parse_codex_line(&count, &mut state).is_none());
        assert!(parse_codex_line(&meta, &mut state).is_none());
        assert!(parse_codex_line(&context, &mut state).is_none());

        let record = parse_codex_line(&count, &mut state).expect("model is known now");
        assert_eq!(record.model, "gpt-5.3-codex");
        assert_eq!(record.session_id, "codex-session");
        assert_eq!(record.project, "/Users/me/dev/waku");
        // input_tokens includes the cached portion.
        assert_eq!(record.totals.uncached_input, 21047 - 1000 - 47);
        assert_eq!(record.totals.cached_input, 1000);
        assert_eq!(record.totals.cache_creation, 47);
        assert_eq!(record.totals.reasoning, 165);

        // The identical re-emitted event is a duplicate, not more usage.
        assert!(parse_codex_line(&count, &mut state).is_none());
    }

    #[test]
    fn codex_fork_uses_session_metas_as_the_copy_boundary() {
        let mut state = CodexScanState::new();
        parse_codex_line(
            &codex_meta(
                "2026-08-06T16:31:19.000Z",
                "child",
                "/child",
                Some("parent"),
                None,
            ),
            &mut state,
        );
        // Copied history is re-stamped to the fork instant. Its ancestor meta
        // confirms that the following usage is replay, while leaving the
        // physical child's identity and cwd intact.
        parse_codex_line(
            &codex_meta("2026-08-06T16:31:19.001Z", "parent", "/parent", None, None),
            &mut state,
        );
        parse_codex_line(
            &codex_context("2026-08-06T16:31:19.002Z", "/parent"),
            &mut state,
        );
        assert!(
            parse_codex_line(
                &codex_count("2026-08-06T16:31:19.003Z", 100, 10),
                &mut state,
            )
            .is_none()
        );

        // This writer repeats the child's meta at the exact structural
        // boundary. Genuine child usage must count even inside the fallback's
        // one-second window.
        parse_codex_line(
            &codex_meta(
                "2026-08-06T16:31:19.004Z",
                "child",
                "/child",
                Some("parent"),
                None,
            ),
            &mut state,
        );
        parse_codex_line(
            &codex_context("2026-08-06T16:31:19.050Z", "/child"),
            &mut state,
        );
        let child = parse_codex_line(
            &codex_count("2026-08-06T16:31:19.100Z", 300, 30),
            &mut state,
        )
        .expect("usage after the child's meta is genuine");
        assert_eq!(child.session_id, "child");
        assert_eq!(child.project, "/child");
        assert_eq!(child.totals.total(), 330);
    }

    #[test]
    fn codex_subagent_without_an_ancestor_meta_counts_fast_usage() {
        let mut state = CodexScanState::new();
        parse_codex_line(
            &codex_meta(
                "2026-08-06T16:31:19.000Z",
                "child",
                "/child",
                None,
                Some("parent"),
            ),
            &mut state,
        );
        parse_codex_line(
            &codex_context("2026-08-06T16:31:19.050Z", "/child"),
            &mut state,
        );
        let child = parse_codex_line(
            &codex_count("2026-08-06T16:31:19.100Z", 100, 10),
            &mut state,
        )
        .expect("a fork marker alone does not prove history was copied");
        assert_eq!(child.session_id, "child");
    }

    #[test]
    fn codex_fork_falls_back_to_the_gap_when_no_child_meta_repeats() {
        let mut state = CodexScanState::new();
        parse_codex_line(
            &codex_meta(
                "2026-08-06T16:31:19.000Z",
                "child",
                "/child",
                Some("parent"),
                None,
            ),
            &mut state,
        );
        parse_codex_line(
            &codex_meta("2026-08-06T16:31:19.001Z", "parent", "/parent", None, None),
            &mut state,
        );
        parse_codex_line(
            &codex_context("2026-08-06T16:31:19.001Z", "/parent"),
            &mut state,
        );
        assert!(
            parse_codex_line(
                &codex_count("2026-08-06T16:31:19.002Z", 100, 10),
                &mut state,
            )
            .is_none()
        );
        assert!(
            parse_codex_line(
                &codex_count("2026-08-06T16:31:19.040Z", 200, 20),
                &mut state,
            )
            .is_none()
        );

        let child = parse_codex_line(
            &codex_count("2026-08-06T16:31:20.040Z", 300, 30),
            &mut state,
        )
        .expect("a full-second gap ends copy suppression");
        assert_eq!(child.session_id, "child");
        assert_eq!(child.totals.total(), 330);
    }

    #[test]
    fn aggregation_dedupes_across_files_and_prices_records() {
        let rates = rate_table(&[("claude-fable-5", FLAT_RATE)]);
        let record = UsageRecord {
            provider: UsageProvider::Claude,
            timestamp_ms: Local::now().timestamp_millis(),
            model: "claude-fable-5".to_owned(),
            session_id: "session-1".to_owned(),
            project: "/Users/me/dev/waku/crates/ui".to_owned(),
            totals: TokenTotals {
                uncached_input: 1_000,
                cached_input: 10_000,
                cache_creation: 2_000,
                output: 500,
                reasoning: 0,
            },
            reported_cost_usd: None,
            dedupe_key: Some("msg:req".to_owned()),
        };
        let today = Local::now().date_naive();
        let roots = [PathBuf::from("/Users/me/dev/waku")];
        let mut aggregator = Aggregator::new(today - chrono::Days::new(29), today, &roots);
        aggregator.add(&record, &rates);
        // The same message copied into a forked session's transcript.
        aggregator.add(&record, &rates);

        let history = derive_history(
            aggregator,
            UsageWindow::TrailingDays(30),
            today - chrono::Days::new(29),
            today,
            PricingStatus::Fresh,
            1,
            0,
            Vec::new(),
            Duration::ZERO,
        );
        assert_eq!(history.records, 1);
        assert_eq!(history.sessions, 1);
        assert_eq!(history.total_tokens, 13_500);
        let expected_cost = 1_000.0 * 1e-6 + 10_000.0 * 1e-7 + 2_000.0 * 1.25e-6 + 500.0 * 2e-6;
        assert!((history.cost_usd - expected_cost).abs() < 1e-9);
        // Savings: cached input at full rate minus the cache-read rate.
        let expected_savings = 10_000.0 * (1e-6 - 1e-7);
        assert!((history.quality.cache_savings_usd - expected_savings).abs() < 1e-9);
        assert_eq!(history.daily.len(), 1);
        assert_eq!(history.providers.len(), 1);
        assert!((history.quality.model_priced_share - 1.0).abs() < f64::EPSILON);

        // The subdirectory cwd resolved to its containing project root, and
        // the month fold carries the same totals as the single active day.
        assert_eq!(history.projects.len(), 1);
        let project = &history.projects[0];
        assert_eq!(project.path, "/Users/me/dev/waku");
        assert_eq!(project.sessions, 1);
        assert_eq!(project.total_tokens, 13_500);
        assert!((project.cost_share - 1.0).abs() < f64::EPSILON);
        assert_eq!(project.last_day, Some(today));
        assert_eq!(project.top_models[0].0, "claude-fable-5");
        assert_eq!(history.months.len(), 1);
        let month = &history.months[0];
        assert_eq!(month.first_day, first_of_month(today));
        assert_eq!(month.total_tokens, 13_500);
        assert_eq!(month.sessions, 1);
        assert_eq!(month.active_days, 1);
        assert_eq!(month.top_models[0].0, "claude-fable-5");
    }

    #[test]
    fn months_and_projects_split_across_boundaries() {
        let rates = rate_table(&[("claude-fable-5", FLAT_RATE)]);
        let today = Local::now().date_naive();
        // Two months back so both records always land inside the window even
        // on the first of a month.
        let since = first_of_month(today)
            .checked_sub_months(chrono::Months::new(2))
            .unwrap();
        let roots: [PathBuf; 0] = [];
        let mut aggregator = Aggregator::new(since, today, &roots);
        let record = |days_ago: i64, session: &str, project: &str| UsageRecord {
            provider: UsageProvider::Claude,
            timestamp_ms: Local::now().timestamp_millis() - days_ago * 86_400_000,
            model: "claude-fable-5".to_owned(),
            session_id: session.to_owned(),
            project: project.to_owned(),
            totals: TokenTotals {
                uncached_input: 100,
                ..TokenTotals::default()
            },
            reported_cost_usd: None,
            dedupe_key: None,
        };
        aggregator.add(&record(0, "session-now", "/a"), &rates);
        aggregator.add(&record(45, "session-then", "/b"), &rates);

        let history = derive_history(
            aggregator,
            MONTHLY_WINDOW,
            since,
            today,
            PricingStatus::Fresh,
            0,
            0,
            Vec::new(),
            Duration::ZERO,
        );
        assert_eq!(history.months.len(), 2, "45 days apart spans two months");
        assert!(history.months[0].first_day < history.months[1].first_day);
        assert_eq!(history.months[1].sessions, 1);
        assert_eq!(history.projects.len(), 2);
        // Equal costs tie-break by tokens; both carry half the total each.
        assert!((history.projects[0].cost_share - 0.5).abs() < 1e-9);
        assert!(
            history
                .month(first_of_month(today))
                .is_some_and(|month| month.total_tokens == 100)
        );
    }

    #[test]
    fn out_of_window_and_unpriced_records_are_classified() {
        let rates = RateTable::unavailable();
        let today = Local::now().date_naive();
        let roots: [PathBuf; 0] = [];
        let mut aggregator = Aggregator::new(today, today, &roots);
        let mut record = UsageRecord {
            provider: UsageProvider::Codex,
            timestamp_ms: Local::now().timestamp_millis(),
            model: "gpt-5.3-codex".to_owned(),
            session_id: String::new(),
            project: String::new(),
            totals: TokenTotals {
                uncached_input: 100,
                ..TokenTotals::default()
            },
            reported_cost_usd: None,
            dedupe_key: None,
        };
        aggregator.add(&record, &rates);
        // Ten days ago falls outside a one-day window.
        record.timestamp_ms -= 10 * 86_400_000;
        aggregator.add(&record, &rates);

        let history = derive_history(
            aggregator,
            UsageWindow::TrailingDays(1),
            today,
            today,
            PricingStatus::Unavailable,
            0,
            0,
            Vec::new(),
            Duration::ZERO,
        );
        assert_eq!(history.records, 1);
        assert_eq!(history.sessions, 0, "an empty session id is not a session");
        assert!((history.quality.unpriced_share - 1.0).abs() < f64::EPSILON);
        assert_eq!(history.cost_usd, 0.0);
    }

    #[test]
    fn model_names_normalize_and_family_names_stay_unpriced() {
        assert_eq!(
            normalize_model_name("anthropic/Claude-Fable-5"),
            "claude-fable-5"
        );
        assert_eq!(normalize_model_name("  gpt-5.3-codex "), "gpt-5.3-codex");
        let rates = rate_table(&[("fable", FLAT_RATE), ("claude-fable-5", FLAT_RATE)]);
        assert!(
            lookup_rate(&rates, "Fable").is_none(),
            "bare family names are ambiguous"
        );
        assert!(lookup_rate(&rates, "<synthetic>").is_none());
        assert!(lookup_rate(&rates, "anthropic/claude-fable-5").is_some());
    }

    #[test]
    fn rate_tables_parse_and_round_trip_through_the_disk_cache() {
        let document: Value = serde_json::from_str(
            r#"{
                "claude-fable-5": {"input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6,
                    "cache_read_input_token_cost": 1e-7, "cache_creation_input_token_cost": 1.25e-6},
                "no-output-rate": {"input_cost_per_token": 1e-6},
                "gpt-5.3-codex": {"input_cost_per_token": 3e-6, "output_cost_per_token": 9e-6}
            }"#,
        )
        .unwrap();
        let rates = parse_rate_table(&document);
        assert_eq!(rates.len(), 2, "half-priced models are dropped");
        // Missing cache rates fall back to the input rate, not to free.
        assert_eq!(rates["gpt-5.3-codex"].cache_read, 3e-6);

        let dir = std::env::temp_dir().join(format!("waku-usage-rates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RATES_CACHE_FILE);
        write_rates_cache(&path, 12345, &rates);
        let (fetched_at_ms, restored) = read_rates_cache(&path).expect("cache round-trips");
        assert_eq!(fetched_at_ms, 12345);
        assert_eq!(restored.len(), rates.len());
        assert_eq!(restored["claude-fable-5"], rates["claude-fable-5"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_and_month_enumeration_cover_calendars() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        let ymd =
            |year, month, day_of_month| NaiveDate::from_ymd_opt(year, month, day_of_month).unwrap();
        assert_eq!(
            UsageWindow::TrailingDays(7).bounds(day),
            (ymd(2026, 8, 3), day)
        );
        // Twelve months = this month plus the eleven before it.
        assert_eq!(UsageWindow::Months(12).bounds(day), (ymd(2025, 9, 1), day));
        assert_eq!(UsageWindow::ThisMonth.bounds(day), (ymd(2026, 8, 1), day));
        // The whole previous month, ending before today — across a year
        // boundary too.
        assert_eq!(
            UsageWindow::LastMonth.bounds(day),
            (ymd(2026, 7, 1), ymd(2026, 7, 31))
        );
        assert_eq!(
            UsageWindow::LastMonth.bounds(ymd(2026, 1, 15)),
            (ymd(2025, 12, 1), ymd(2025, 12, 31))
        );
        let months = enumerate_months(
            NaiveDate::from_ymd_opt(2025, 11, 15).unwrap(),
            NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
        );
        assert_eq!(months.len(), 4);
        assert_eq!(months[0], NaiveDate::from_ymd_opt(2025, 11, 1).unwrap());
        assert_eq!(months[3], NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
        assert_eq!(
            days_in_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()),
            28
        );
        assert_eq!(
            days_in_month(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap()),
            29
        );
    }

    #[test]
    fn day_enumeration_is_inclusive() {
        let since = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let until = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let days = enumerate_days(since, until);
        assert_eq!(days.len(), 3);
        assert_eq!(days[0], since);
        assert_eq!(days[2], until);
        assert_eq!(enumerate_days(until, since), Vec::<NaiveDate>::new());
    }

    fn claude_usage_line(
        timestamp: &str,
        session_id: &str,
        cwd: &Path,
        model: &str,
        msg_id: &str,
        req_id: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": timestamp,
            "requestId": req_id,
            "sessionId": session_id,
            "cwd": cwd.display().to_string(),
            "message": {
                "id": msg_id,
                "model": model,
                "usage": {
                    "input_tokens": input_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": output_tokens
                }
            }
        })
        .to_string()
    }

    #[test]
    fn collect_scan_roots_deduplicates_canonical_account_and_default_roots() {
        let base =
            std::env::temp_dir().join(format!("waku-usage-roots-dedup-{}", uuid::Uuid::new_v4()));
        let claude_default = base.join("claude-default");
        let claude_work = base.join("claude-work");
        let codex_work = base.join("codex-work");
        std::fs::create_dir_all(claude_default.join("projects")).unwrap();
        std::fs::create_dir_all(claude_work.join("projects").join("nested")).unwrap();
        std::fs::create_dir_all(codex_work.join("sessions").join("nested")).unwrap();

        let default_roots = vec![(UsageProvider::Claude, claude_default.join("projects"))];
        let mut account_dirs = vec![
            // Same canonical root as default Claude root via a non-canonical spelling.
            (UsageProvider::Claude, claude_default.join(".")),
            // Distinct Claude account dir plus a non-canonical duplicate.
            (UsageProvider::Claude, claude_work.clone()),
            (
                UsageProvider::Claude,
                claude_work.join("projects").join(".."),
            ),
            // Distinct Codex account dir plus a non-canonical duplicate.
            (UsageProvider::Codex, codex_work.clone()),
            (UsageProvider::Codex, codex_work.join("sessions").join("..")),
        ];
        let symlink_alias = base.join("claude-work-link");
        if crate::fs_ext::symlink(&claude_work, &symlink_alias).is_ok() {
            account_dirs.push((UsageProvider::Claude, symlink_alias));
        }

        let roots = collect_scan_roots(default_roots, &account_dirs);
        assert_eq!(roots.len(), 3);
        assert_eq!(
            roots[0],
            (UsageProvider::Claude, claude_default.join("projects"))
        );
        assert_eq!(
            roots[1],
            (UsageProvider::Claude, claude_work.join("projects"))
        );
        assert_eq!(
            roots[2],
            (UsageProvider::Codex, codex_work.join("sessions"))
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_with_account_dirs_scans_multiple_dirs_and_deduplicates_canonical_roots() {
        let base =
            std::env::temp_dir().join(format!("waku-usage-multi-dir-{}", uuid::Uuid::new_v4()));
        let project_root = base.join("repo");
        let project_sub = project_root.join("crates/core");
        let claude_dir_a = base.join("claude-a");
        let claude_dir_b = base.join("claude-b");
        let codex_dir = base.join("codex-work");
        let missing_dir = base.join("nonexistent-account");

        std::fs::create_dir_all(&project_sub).unwrap();
        std::fs::create_dir_all(claude_dir_a.join("projects/p1")).unwrap();
        std::fs::create_dir_all(claude_dir_b.join("projects/p2")).unwrap();
        std::fs::create_dir_all(codex_dir.join("sessions/2026/08")).unwrap();
        std::fs::create_dir_all(codex_dir.join("helper")).unwrap();

        let now_rfc3339 = Utc::now().to_rfc3339();
        let unique_id = uuid::Uuid::new_v4();
        let claude_model_a = format!("waku-test-claude-a-{unique_id}");
        let claude_model_b = format!("waku-test-claude-b-{unique_id}");
        let codex_model = format!("waku-test-codex-{unique_id}");

        std::fs::write(
            claude_dir_a.join("projects/p1/session-a.jsonl"),
            format!(
                "{}\n",
                claude_usage_line(
                    &now_rfc3339,
                    &format!("claude-sess-a-{unique_id}"),
                    &project_sub,
                    &claude_model_a,
                    &format!("msg-a-{unique_id}"),
                    &format!("req-a-{unique_id}"),
                    1_000,
                    250,
                )
            ),
        )
        .unwrap();
        std::fs::write(
            claude_dir_b.join("projects/p2/session-b.jsonl"),
            format!(
                "{}\n",
                claude_usage_line(
                    &now_rfc3339,
                    &format!("claude-sess-b-{unique_id}"),
                    &project_root,
                    &claude_model_b,
                    &format!("msg-b-{unique_id}"),
                    &format!("req-b-{unique_id}"),
                    2_000,
                    500,
                )
            ),
        )
        .unwrap();
        let codex_lines = [
            codex_meta(
                &now_rfc3339,
                &format!("codex-sess-{unique_id}"),
                &project_root.display().to_string(),
                None,
                None,
            ),
            serde_json::json!({
                "timestamp": now_rfc3339,
                "type": "turn_context",
                "payload": {
                    "model": codex_model,
                    "cwd": project_root.display().to_string(),
                },
            })
            .to_string(),
            codex_count(&now_rfc3339, 3_000, 400),
        ];
        std::fs::write(
            codex_dir.join("sessions/2026/08/rollout.jsonl"),
            format!("{}\n", codex_lines.join("\n")),
        )
        .unwrap();

        let rates = rate_table(&[
            (&claude_model_a, FLAT_RATE),
            (&claude_model_b, FLAT_RATE),
            (&codex_model, FLAT_RATE),
        ]);
        let project_roots = [project_root.clone()];
        let mut cache = ScanCache::new();

        // Capture host-machine baseline (default roots only) so assertions stay
        // exact even when the test host has real ~/.claude or ~/.codex files.
        let baseline = scan_with_account_dirs(
            &mut cache,
            &rates,
            UsageWindow::TrailingDays(7),
            &project_roots,
            &[],
        );

        let mut account_dirs = vec![
            (UsageProvider::Claude, claude_dir_a.clone()),
            // Non-canonical duplicate of claude_dir_a must be deduplicated.
            (UsageProvider::Claude, claude_dir_a.join(".")),
            (UsageProvider::Claude, claude_dir_b.clone()),
            (UsageProvider::Codex, codex_dir.clone()),
            // Non-canonical duplicate of codex_dir must be deduplicated: Codex
            // records carry no dedupe_key, so scanning twice would double tokens.
            (UsageProvider::Codex, codex_dir.join("helper").join("..")),
            // Missing account directory is skipped without error.
            (UsageProvider::Claude, missing_dir),
        ];
        let codex_link = base.join("codex-work-link");
        if crate::fs_ext::symlink(&codex_dir, &codex_link).is_ok() {
            account_dirs.push((UsageProvider::Codex, codex_link));
        }

        let history = scan_with_account_dirs(
            &mut cache,
            &rates,
            UsageWindow::TrailingDays(7),
            &project_roots,
            &account_dirs,
        );

        assert_eq!(history.scanned_files, baseline.scanned_files + 3);
        assert_eq!(history.errors, baseline.errors);

        let model_tokens = |provider: UsageProvider, model: &str| {
            history
                .models
                .iter()
                .find(|slice| slice.provider == provider && slice.model == model)
                .map(|slice| slice.total_tokens)
        };
        assert_eq!(
            model_tokens(UsageProvider::Claude, &claude_model_a),
            Some(1_250)
        );
        assert_eq!(
            model_tokens(UsageProvider::Claude, &claude_model_b),
            Some(2_500)
        );
        assert_eq!(
            model_tokens(UsageProvider::Codex, &codex_model),
            Some(3_400)
        );

        let project_key = project_root.display().to_string();
        let project = history
            .projects
            .iter()
            .find(|slice| slice.path == project_key)
            .expect("all account dirs attributed to the synthetic project root");
        assert_eq!(project.sessions, 3);
        assert_eq!(project.total_tokens, 1_250 + 2_500 + 3_400);
        assert_eq!(
            project.by_provider[UsageProvider::Claude.index()].total_tokens,
            1_250 + 2_500
        );
        assert_eq!(
            project.by_provider[UsageProvider::Codex.index()].total_tokens,
            3_400
        );

        // Warm rescan reuses cached entries and produces identical counts.
        let rescanned = scan_with_account_dirs(
            &mut cache,
            &rates,
            UsageWindow::TrailingDays(7),
            &project_roots,
            &account_dirs,
        );
        assert_eq!(rescanned.scanned_files, history.scanned_files);
        let rescanned_project = rescanned
            .projects
            .iter()
            .find(|slice| slice.path == project_key)
            .expect("warm rescan keeps synthetic project totals");
        assert_eq!(rescanned_project.total_tokens, project.total_tokens);
        assert_eq!(rescanned_project.sessions, project.sessions);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn antigravity_lines_parse_disjoint_tokens_and_step_dedupe_key() {
        let line = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-23T07:31:48Z","model":"gemini-3.8-flash-low","input_tokens":8591,"cache_read_tokens":1200,"output_tokens":217}"#;
        let record =
            parse_antigravity_line(line, "conv-123", "/Users/me/repo").expect("line has usage");
        assert_eq!(record.provider, UsageProvider::Antigravity);
        assert_eq!(record.model, "gemini-3.8-flash-low");
        assert_eq!(record.session_id, "conv-123");
        assert_eq!(record.project, "/Users/me/repo");
        assert_eq!(record.dedupe_key.as_deref(), Some("conv-123:1"));
        assert_eq!(record.reported_cost_usd, None);
        assert_eq!(record.totals.uncached_input, 8591);
        assert_eq!(record.totals.cached_input, 1200);
        assert_eq!(record.totals.cache_creation, 0);
        assert_eq!(record.totals.output, 217);
        assert_eq!(record.totals.reasoning, 0);
        assert_eq!(record.totals.total(), 8591 + 1200 + 217);

        let missing_model = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-23T07:32:00Z","input_tokens":10,"output_tokens":5}"#;
        let fallback = parse_antigravity_line(missing_model, "conv-123", "").unwrap();
        assert_eq!(fallback.model, "unknown");
        assert_eq!(fallback.dedupe_key.as_deref(), Some("conv-123:2"));

        assert!(
            parse_antigravity_line(
                r#"{"step_index":3,"type":"USER_INPUT","created_at":"2026-11-23T07:32:00Z","input_tokens":10,"output_tokens":5}"#,
                "conv-123",
                "",
            )
            .is_none()
        );
        assert!(
            parse_antigravity_line(
                r#"{"step_index":4,"type":"PLANNER_RESPONSE","created_at":"2026-11-23T07:32:00Z","input_tokens":0,"cache_read_tokens":0,"output_tokens":0}"#,
                "conv-123",
                "",
            )
            .is_none()
        );
    }

    #[test]
    fn antigravity_rate_lookup_strips_one_effort_suffix() {
        let base_rate = ModelRate {
            input: 1e-6,
            output: 2e-6,
            cache_read: 2.5e-7,
            cache_creation: 1e-6,
        };
        let max_override = ModelRate {
            input: 5e-6,
            output: 10e-6,
            cache_read: 1e-6,
            cache_creation: 5e-6,
        };
        let rates = rate_table(&[
            ("gemini-3.8-flash", base_rate),
            ("gemini-3.8-flash-max", max_override),
            ("opus", FLAT_RATE),
        ]);

        for suffix in ["-low", "-medium", "-high", "-xhigh"] {
            let model = format!("gemini-3.8-flash{suffix}");
            assert_eq!(lookup_rate(&rates, &model), Some(&base_rate));
        }
        assert_eq!(
            lookup_rate(&rates, "gemini-3.8-flash-max"),
            Some(&max_override),
            "exact rate takes precedence over stripped base"
        );
        assert_eq!(
            lookup_rate(&rates, "gemini-3.8-flash-low-high"),
            None,
            "at most one trailing effort suffix is stripped"
        );
        assert_eq!(
            lookup_rate(&rates, "opus-high"),
            None,
            "unpriceable bare family names stay unpriced after stripping"
        );
    }

    #[test]
    fn antigravity_brain_roots_respect_priority_and_sidecar_parent() {
        let home = PathBuf::from("/tmp/home-user");
        let sidecar = std::ffi::OsStr::new("/tmp/home-user/.gemini/antigravity-cli/sidecar_data");
        let roots = antigravity_brain_roots_from(Some(sidecar), Some(&home));
        assert_eq!(
            roots,
            vec![
                home.join(".gemini/antigravity-cli/brain"),
                home.join(".gemini/antigravity-acp/brain"),
                home.join(".gemini/antigravity/brain"),
            ]
        );

        let custom = std::ffi::OsStr::new("/opt/agy-custom");
        let custom_roots = antigravity_brain_roots_from(Some(custom), Some(&home));
        assert_eq!(custom_roots[0], PathBuf::from("/opt/agy-custom/brain"));
        assert_eq!(custom_roots.len(), 4);
    }

    #[test]
    fn antigravity_uri_conversion_handles_windows_encoded_drive_letters() {
        let converted = antigravity_uri_to_workspace_path("file:///c%3A/Users/alice/git/waku")
            .expect("file URI parses");
        #[cfg(windows)]
        assert_eq!(converted, r"C:\Users\alice\git\waku");
        #[cfg(not(windows))]
        assert_eq!(converted, "/c:/Users/alice/git/waku");
    }

    #[test]
    fn antigravity_scan_ignores_transcript_full_maps_projects_and_dedupes_steps() {
        let base =
            std::env::temp_dir().join(format!("waku-usage-agy-{}", uuid::Uuid::new_v4()));
        let agy_root = base.join("agy");
        let brain = agy_root.join("brain");
        let db_project = base.join("repo-db");
        let hist_project = base.join("repo-hist");
        std::fs::create_dir_all(&db_project).unwrap();
        std::fs::create_dir_all(&hist_project).unwrap();

        let now_rfc3339 = Utc::now().to_rfc3339();
        let unique_id = uuid::Uuid::new_v4();
        let base_model = format!("waku-test-gemini-{unique_id}");
        let effort_model = format!("{base_model}-high");
        let conv_db = format!("conv-db-{unique_id}");
        let conv_hist = format!("conv-hist-{unique_id}");
        let conv_both = format!("conv-both-{unique_id}");

        let agy_line = |step: u64, input: u64, cache_read: u64, output: u64| {
            serde_json::json!({
                "step_index": step,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": now_rfc3339,
                "model": effort_model,
                "input_tokens": input,
                "cache_read_tokens": cache_read,
                "output_tokens": output,
            })
            .to_string()
        };

        let db_logs = brain.join(&conv_db).join(".system_generated/logs");
        let hist_logs = brain.join(&conv_hist).join(".system_generated/logs");
        let both_logs = brain.join(&conv_both).join(".system_generated/logs");
        std::fs::create_dir_all(&db_logs).unwrap();
        std::fs::create_dir_all(&hist_logs).unwrap();
        std::fs::create_dir_all(&both_logs).unwrap();

        // Include a duplicate step_index inside transcript.jsonl; only one counts.
        std::fs::write(
            db_logs.join("transcript.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                agy_line(1, 1_000, 200, 100),
                agy_line(1, 1_000, 200, 100),
                agy_line(2, 500, 0, 50),
            ),
        )
        .unwrap();
        // transcript_full.jsonl must be ignored by discovery.
        std::fs::write(
            db_logs.join("transcript_full.jsonl"),
            format!("{}\n", agy_line(99, 999_999, 0, 999_999)),
        )
        .unwrap();
        std::fs::write(
            hist_logs.join("transcript.jsonl"),
            format!("{}\n", agy_line(1, 300, 100, 40)),
        )
        .unwrap();
        std::fs::write(
            both_logs.join("transcript.jsonl"),
            format!("{}\n", agy_line(1, 400, 0, 60)),
        )
        .unwrap();

        // Populate conversation_summaries.db with file:// URIs (and verify
        // percent-encoded Windows drive letter URI parsing inside SQLite too).
        let db_uri = url::Url::from_file_path(&db_project)
            .expect("temp path converts to file:// URI")
            .to_string();
        let conn = rusqlite::Connection::open(agy_root.join("conversation_summaries.db")).unwrap();
        conn.execute(
            "CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, workspace_uris TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, workspace_uris) VALUES (?1, ?2)",
            rusqlite::params![conv_db, serde_json::json!([db_uri]).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, workspace_uris) VALUES (?1, ?2)",
            rusqlite::params![
                conv_both,
                serde_json::json!(["file:///c%3A/Users/alice/from-db"]).to_string()
            ],
        )
        .unwrap();
        drop(conn);

        // history.jsonl maps conv_hist and tries to override conv_both; DB must win for conv_both.
        std::fs::write(
            agy_root.join("history.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "conversationId": conv_hist,
                    "workspace": hist_project.display().to_string(),
                }),
                serde_json::json!({
                    "conversationId": conv_both,
                    "workspace": hist_project.display().to_string(),
                }),
            ),
        )
        .unwrap();

        let rates = rate_table(&[(&base_model, FLAT_RATE)]);
        let project_roots = [db_project.clone(), hist_project.clone()];
        let mut cache = ScanCache::new();
        let baseline = scan_with_account_dirs(
            &mut cache,
            &rates,
            UsageWindow::TrailingDays(7),
            &project_roots,
            &[],
        );

        let account_dirs = vec![(UsageProvider::Antigravity, agy_root.clone())];
        let history = scan_with_account_dirs(
            &mut cache,
            &rates,
            UsageWindow::TrailingDays(7),
            &project_roots,
            &account_dirs,
        );

        assert_eq!(
            history.scanned_files,
            baseline.scanned_files + 3,
            "transcript_full.jsonl must be excluded"
        );
        let model_slice = history
            .models
            .iter()
            .find(|slice| {
                slice.provider == UsageProvider::Antigravity && slice.model == effort_model
            })
            .expect("Antigravity model slice present");
        let expected_db_tokens = (1_000 + 200 + 100) + (500 + 50);
        let expected_hist_tokens = 300 + 100 + 40;
        let expected_both_tokens = 400 + 60;
        assert_eq!(
            model_slice.total_tokens,
            expected_db_tokens + expected_hist_tokens + expected_both_tokens
        );
        assert!(model_slice.cost_usd > 0.0, "effort-suffix lookup priced the model");

        let db_key = db_project.display().to_string();
        let db_slice = history
            .projects
            .iter()
            .find(|slice| slice.path == db_key)
            .expect("DB workspace mapped");
        assert_eq!(db_slice.total_tokens, expected_db_tokens);
        assert_eq!(
            db_slice.by_provider[UsageProvider::Antigravity.index()].total_tokens,
            expected_db_tokens
        );

        let hist_key = hist_project.display().to_string();
        let hist_slice = history
            .projects
            .iter()
            .find(|slice| slice.path == hist_key)
            .expect("history.jsonl workspace mapped");
        assert_eq!(
            hist_slice.total_tokens, expected_hist_tokens,
            "conv_both stays on DB workspace instead of falling back to history.jsonl"
        );

        let expected_both_key =
            antigravity_uri_to_workspace_path("file:///c%3A/Users/alice/from-db").unwrap();
        let both_slice = history
            .projects
            .iter()
            .find(|slice| slice.path == expected_both_key)
            .expect("encoded Windows URI workspace mapped");
        assert_eq!(both_slice.total_tokens, expected_both_tokens);

        // Warm rescan reuses cache, preserves totals, and skips re-opening
        // `conversation_summaries.db` / `history.jsonl` when cached records
        // already know their project.
        let loads_before_warm = ANTIGRAVITY_WORKSPACE_MAP_LOADS.load(std::sync::atomic::Ordering::Relaxed);
        let rescanned = scan_with_account_dirs(
            &mut cache,
            &rates,
            UsageWindow::TrailingDays(7),
            &project_roots,
            &account_dirs,
        );
        let loads_after_warm = ANTIGRAVITY_WORKSPACE_MAP_LOADS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            loads_after_warm, loads_before_warm,
            "warm ScanCache hits must not reload conversation_summaries.db or history.jsonl"
        );
        let rescanned_model = rescanned
            .models
            .iter()
            .find(|slice| {
                slice.provider == UsageProvider::Antigravity && slice.model == effort_model
            })
            .unwrap();
        assert_eq!(rescanned_model.total_tokens, model_slice.total_tokens);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn account_transcript_root_strips_sidecar_data_consistently() {
        let base = Path::new("/tmp/agy-work");
        assert_eq!(
            account_transcript_root(UsageProvider::Antigravity, base),
            base.join("brain")
        );
        assert_eq!(
            account_transcript_root(UsageProvider::Antigravity, &base.join("sidecar_data")),
            base.join("brain")
        );
        assert_eq!(
            antigravity_brain_roots_from(
                Some(base.join("sidecar_data").as_os_str()),
                None,
            ),
            vec![base.join("brain")]
        );
    }
}
