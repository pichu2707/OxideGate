//! Estado compartido que atraviesa todos los handlers del proxy.
use crate::config::AppConfig;
use crate::telemetry::TelemetrySink;
use std::sync::Arc;

/// Se clona barato (todo es Arc / handles) y viaja por el `with_state` de axum.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub http: reqwest::Client,
    pub telemetry: TelemetrySink,
}
