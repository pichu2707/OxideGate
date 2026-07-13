# Tasks: Codex Quota Telemetry — Slice 1 (raw capture in `/requests`)

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | 350-430 |
| 400-line budget risk | Medium (close to/possibly over budget — 7 unit-test scenarios exceed design's original 3-test estimate of 250-320) |
| Chained PRs recommended | Yes |
| Suggested split | PR 1 → PR 2 |
| Delivery strategy | ask-on-risk |
| Chain strategy | pending |

Decision needed before apply: Yes
Chained PRs recommended: Yes
Chain strategy: pending
400-line budget risk: Medium

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| 1 | `codex_quota` module: struct, parser, 7 unit tests | PR 1 | Standalone, no callers yet; base = main. ~270-320 lines. |
| 2 | Wire `CodexQuota` through `MetricBase`→`RequestMetric`→`RecentRequest`→`proxy.rs`, fix stale doc, extend `recent.rs` tests, live verify | PR 2 | Depends on PR 1. ~90-120 lines. Base = main (stacked) or PR 1 branch (feature-branch-chain), per chosen chain strategy. |

## Phase 1: Foundation — `codex_quota` module

- [x] 1.1 Create `src/telemetry/codex_quota.rs` with `//!` module doc: shared sanitization contract (empty→`None`, malformed→`None`, never fabricate/panic).
- [x] 1.2 Define `CodexQuota` struct, 12 `Option` fields, `#[derive(Debug, Clone, Serialize, PartialEq)]`, one-line `///` per field: `plan_type`, `active_limit`, `credits_balance` (`String`); `primary/secondary_used_percent`, `primary/secondary_window_minutes`, `primary_reset_after_seconds` (`u64`); `primary/secondary_reset_at` (`i64`); `credits_has_credits`, `credits_unlimited` (`bool`).
- [x] 1.3 Implement `CodexQuota::from_headers(&HeaderMap) -> Option<Self>`: returns `None` if no `x-codex-*` header is present at all; else parses each field independently.
- [x] 1.4 Add private parse helpers per type (string/u64/i64/bool): missing/empty/malformed → `None`, never panic or fabricate a default; bool only on exact `"True"`/`"False"`.
- [x] 1.5 `src/telemetry/mod.rs`: add `pub mod codex_quota;` (the `pub use codex_quota::CodexQuota;` re-export moved to task 3.9's commit — see Apply Progress note: an unused `pub use` triggers `unused_imports` in this bin-only crate until a caller consumes it).

## Phase 2: Unit tests (`codex_quota.rs`, spec scenarios)

- [x] 2.1 Test: all 12 headers present with valid values → all fields `Some` with correct parsed values.
- [x] 2.2 Test: no `x-codex-*` header present → `from_headers` returns `None`.
- [x] 2.3 Test: header present but empty (`x-codex-secondary-reset-at`) → field `None`, never `Some("")`/`Some(0)`.
- [x] 2.4 Test: malformed numeric value → field `None`, no panic.
- [x] 2.5 Test: `credits-has-credits`/`credits-unlimited` = `"True"`/`"False"` → `Some(bool)`.
- [x] 2.6 Test: unrecognized boolean value (lowercase, `"1"`, empty) → `None`.
- [x] 2.7 Honesty test: `CodexQuota` carries no USD field and no path to `cost_estimate_usd` — structural separation.

## Phase 3: Wiring through the metric chain

- [ ] 3.1 `src/telemetry/metered.rs`: add `pub codex_quota: Option<CodexQuota>` to `MetricBase`.
- [ ] 3.2 `src/telemetry/metered.rs` (`MeteredBody::emit`, ~250): add `codex_quota: self.base.codex_quota.clone()` to the `RequestMetric` literal.
- [ ] 3.3 `src/telemetry/logger.rs`: add `pub codex_quota: Option<CodexQuota>` to `RequestMetric`.
- [ ] 3.4 `src/telemetry/logger.rs`: fix `tools_by_server` doc — remove "ÚNICO CAMPO NO-PLANO" claim, no longer true once `codex_quota` lands.
- [ ] 3.5 `src/telemetry/recent.rs`: add `pub codex_quota: Option<CodexQuota>` to `RecentRequest`.
- [ ] 3.6 `src/telemetry/recent.rs` (`RecentRequest::from`): add `codex_quota: m.codex_quota.clone()`.
- [ ] 3.7 `src/middleware/proxy.rs` (`base` literal, ~209): add `codex_quota: CodexQuota::from_headers(resp.headers())`.
- [ ] 3.8 `src/middleware/proxy.rs` (upstream error branch, ~161): add `codex_quota: None`.
- [ ] 3.9 `src/middleware/proxy.rs`: import `CodexQuota` from `crate::telemetry`.

## Phase 4: Wiring tests (`recent.rs`)

- [ ] 4.1 Update `base_metric()` test helper: add `codex_quota: None`.
- [ ] 4.2 Extend `proyeccion_copia_campos_fielmente_incluyendo_none`: assert `row.codex_quota` is `None`.
- [ ] 4.3 New test: projection copies `codex_quota` faithfully when `Some` (fixture `CodexQuota`, assert equality).
- [ ] 4.4 New test: round-trip serde of `RecentRequest` with `codex_quota: Some(..)` preserves all nested fields (pattern of `round_trip_serde_con_tools_by_server_presente`).
- [ ] 4.5 New test: round-trip serde with `codex_quota: None` serializes to `null` (pattern of `round_trip_serde_con_client_none`).

## Phase 5: Verification and docs

- [ ] 5.1 Run `cargo test` and `cargo clippy --all-targets` — all green, no warnings.
- [ ] 5.2 Live verify: route a real `gpt-5.5` request via Codex backend (`OPENAI_API_BASE=https://chatgpt.com/backend-api/codex`, Codex CLI custom provider with `requires_openai_auth`); confirm the `/requests` row carries parsed quota fields, and Anthropic/Gemini/API-key rows carry `codex_quota: null` (recipe: `docs/telemetry-level-1.md` §5.3).
- [ ] 5.3 Fix `proposal.md` "~11 campos" typo to "12 campos" (artifact-only, no code impact).

**Out of scope (not tasked here):** `/stats` aggregation, TUI gauge, notional API cost, marginal delta — slices 2-5 per `proposal.md` chain order.
