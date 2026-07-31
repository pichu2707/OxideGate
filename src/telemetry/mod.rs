//! Telemetría del proxy: mide sin estorbar el camino crítico del request.
pub mod cache_attribution;
pub mod codex_quota;
pub mod logger;
pub mod metered;
pub mod pricing;
pub mod recent;
pub mod rehydrate;
pub mod section_share;
pub mod session;
pub mod stats;

pub use cache_attribution::CacheBySection;
pub use codex_quota::CodexQuota;
pub use logger::{RequestMetric, TelemetrySink};
pub use metered::{MeteredBody, MetricBase};
pub use recent::{RecentRequest, RecentRequests};
pub use rehydrate::Rehydrated;
pub use section_share::SectionShare;
pub use session::{SessionAttribution, SessionSource};
pub use stats::{SessionRegistry, SessionSnapshot, StatsRegistry, StatsSnapshot};
