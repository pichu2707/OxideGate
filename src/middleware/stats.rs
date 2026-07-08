//! Endpoint HTTP de agregación en vivo por `(proveedor, modelo)`.
//!
//! Este archivo es el único que conoce axum en toda la cadena de stats: la
//! acumulación en sí (`StatsRegistry`) vive en
//! [`telemetry::stats`](crate::telemetry::stats) y es framework-agnóstica.
//! Acá solo se toma el read-lock, se clona el snapshot y se serializa.
use crate::state::AppState;
use crate::telemetry::StatsSnapshot;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

/// `GET /stats` → snapshot en vivo de la telemetría agregada por modelo.
///
/// No requiere autenticación: el proxy bindea en `127.0.0.1` y el snapshot
/// solo expone agregados y conteos de huellas (nunca prompts ni huellas
/// individuales), así que no hay secretos que proteger.
pub async fn handle_stats(State(state): State<Arc<AppState>>) -> Response {
    let registry = state.telemetry.stats();

    // Read-lock BREVE: se toma, se construye el snapshot (todo síncrono) y se
    // suelta antes de devolver la respuesta. Nunca cruza un `.await`.
    let snapshot: StatsSnapshot = match registry.read() {
        Ok(guard) => guard.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };

    Json(snapshot).into_response()
}
