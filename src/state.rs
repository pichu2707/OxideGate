//! Estado compartido que atraviesa todos los handlers del proxy.
use crate::config::AppConfig;
use crate::telemetry::TelemetrySink;
use crate::telemetry::power::PowerMeter;
use std::sync::Arc;

/// Se clona barato (todo es Arc / handles) y viaja por el `with_state` de axum.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub http: reqwest::Client,
    pub telemetry: TelemetrySink,
    /// Muestreador de potencia de la GPU, o `None` si no hay `nvidia-smi` o
    /// el muestreo está apagado por entorno. `None` no es un fallo: es un
    /// campo de energía vacío, que es la respuesta correcta cuando no hay
    /// nada que leer (ver `telemetry::power`).
    pub power: Option<Arc<PowerMeter>>,
}
