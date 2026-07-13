//! Telemetría del proxy: mide sin estorbar el camino crítico del request.
pub mod codex_quota;
pub mod logger;
pub mod metered;
pub mod pricing;
pub mod recent;
pub mod stats;

// `pub use codex_quota::CodexQuota;` se agrega en el siguiente commit,
// cuando `MetricBase`/`RequestMetric`/`RecentRequest` empiecen a usarlo de
// verdad (`cargo clippy` marca un re-export como import no usado si nada en
// el crate lo consume todavía, ver la nota de scaffolding en
// `codex_quota.rs`).
pub use logger::{RequestMetric, TelemetrySink};
pub use metered::{MeteredBody, MetricBase};
pub use recent::{RecentRequest, RecentRequests};
pub use stats::{StatsRegistry, StatsSnapshot};
