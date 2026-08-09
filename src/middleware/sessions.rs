//! Endpoint HTTP de agregación por sesión: `GET /sessions`.
//!
//! Endpoint APARTE en vez de un campo nuevo en `/stats`: aquel devuelve un
//! array y el monitor lo deserializa como tal, así que convertirlo en objeto
//! rompería a todo consumidor existente. Un endpoint hermano es aditivo, y
//! una build anterior simplemente devuelve 404.
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use crate::state::AppState;
use crate::telemetry::SessionSnapshot;

/// `GET /sessions` → agregación por `(source, key)`.
///
/// Responde qué costó cada SESIÓN, no cada modelo. Para quien corre varios
/// agentes a la vez, el gasto por modelo no dice quién lo generó.
pub async fn handle_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<crate::middleware::SinceQuery>,
) -> Response {
    // Mismo contrato de `?since=` que `/stats`: fecha o días, y 400 si no se
    // entiende. Las dos rutas tienen que comportarse igual o el parámetro deja
    // de ser aprendible.
    let desde =
        match crate::middleware::parse_since(q.since.as_deref(), chrono::Utc::now().date_naive()) {
            Ok(d) => d,
            Err(e) => return (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
        };
    let registry = state.telemetry.sessions();

    // Read-lock BREVE, sin `.await` dentro: se toma, se construye el snapshot
    // (todo síncrono) y se suelta antes de responder.
    let snapshot: SessionSnapshot = match registry.read() {
        Ok(guard) => guard.snapshot(desde),
        Err(poisoned) => poisoned.into_inner().snapshot(desde),
    };

    Json(serde_json::json!({
        "sessions": snapshot.0,
        // Si se alcanzó el tope de sesiones distintas, las filas son una COTA
        // INFERIOR honesta: faltan claves nuevas que se dejaron de admitir.
        "saturated": snapshot.1,
    }))
    .into_response()
}
