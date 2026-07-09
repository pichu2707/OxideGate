//! Endpoint HTTP del detalle en vivo de los últimos requests atendidos.
//!
//! Este archivo es el único que conoce axum en toda la cadena de requests
//! recientes: el buffer en sí (`RecentRequests`) vive en
//! [`telemetry::recent`](crate::telemetry::recent) y es framework-agnóstico.
//! Acá solo se toma el read-lock, se clona el snapshot y se serializa.
use crate::state::AppState;
use crate::telemetry::RecentRequest;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

/// `GET /requests` → detalle en vivo de los últimos requests individuales,
/// en orden cronológico (más viejo primero, más nuevo al final).
///
/// No requiere autenticación: el proxy bindea en `127.0.0.1`, igual que
/// `/stats`. A diferencia de `telemetry.jsonl` (que sí guarda `prompt_hash`
/// por request para poder correlacionar redundancia offline), este endpoint
/// JAMÁS expone `prompt_hash`: `RecentRequest` no tiene ese campo, así que no
/// hay huella individual que se pueda filtrar, aunque no haya auth.
pub async fn handle_requests(State(state): State<Arc<AppState>>) -> Response {
    let registry = state.telemetry.recent();

    // Read-lock BREVE: se toma, se construye el snapshot (todo síncrono) y se
    // suelta antes de devolver la respuesta. Nunca cruza un `.await`.
    let snapshot: Vec<RecentRequest> = match registry.read() {
        Ok(guard) => guard.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };

    Json(snapshot).into_response()
}
