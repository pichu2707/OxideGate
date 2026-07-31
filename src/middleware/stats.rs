//! Endpoint HTTP de agregación en vivo por `(proveedor, modelo)`.
//!
//! Este archivo es el único que conoce axum en toda la cadena de stats: la
//! acumulación en sí (`StatsRegistry`) vive en
//! [`telemetry::stats`](crate::telemetry::stats) y es framework-agnóstica.
//! Acá solo se toma el read-lock, se clona el snapshot y se serializa.
use crate::state::AppState;
use crate::telemetry::StatsSnapshot;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

/// `GET /stats` → snapshot en vivo de la telemetría agregada por modelo.
///
/// No requiere autenticación: el proxy bindea en `127.0.0.1` y el snapshot
/// solo expone agregados y conteos de huellas (nunca prompts ni huellas
/// individuales), así que no hay secretos que proteger.
/// Admite `?since=` para acotar la ventana: una fecha (`2026-07-24`) o días
/// hacia atrás (`7d`). Sin el parámetro, el comportamiento es el de siempre —
/// todo lo acumulado— así que la ruta sigue siendo compatible.
///
/// Un `since` ilegible devuelve **400**, nunca todo el histórico: servir una
/// ventana distinta de la pedida sin decirlo dejaría al consumidor mirando un
/// rango que no es el suyo y creyendo que sí.
pub async fn handle_stats(
    State(state): State<Arc<AppState>>,
    Query(q): Query<crate::middleware::SinceQuery>,
) -> Response {
    let desde = match crate::middleware::parse_since(q.since.as_deref(), chrono::Utc::now().date_naive()) {
        Ok(d) => d,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let registry = state.telemetry.stats();

    // Read-lock BREVE: se toma, se construye el snapshot (todo síncrono) y se
    // suelta antes de devolver la respuesta. Nunca cruza un `.await`.
    let snapshot: StatsSnapshot = match registry.read() {
        Ok(guard) => guard.snapshot(desde),
        Err(poisoned) => poisoned.into_inner().snapshot(desde),
    };

    Json(snapshot).into_response()
}
