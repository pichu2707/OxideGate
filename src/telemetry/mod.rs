//! Telemetría del proxy: mide sin estorbar el camino crítico del request.
pub mod codex_quota;
pub mod logger;
pub mod metered;
pub mod pricing;
pub mod recent;
pub mod stats;

pub use codex_quota::CodexQuota;
pub use logger::{RequestMetric, TelemetrySink};
pub use metered::{MeteredBody, MetricBase};
pub use recent::{RecentRequest, RecentRequests};
pub use stats::{StatsRegistry, StatsSnapshot};
