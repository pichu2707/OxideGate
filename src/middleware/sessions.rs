//! Endpoint HTTP de agregación por sesión: `GET /sessions`.
//!
//! Endpoint APARTE en vez de un campo nuevo en `/stats`: aquel devuelve un
//! array y el monitor lo deserializa como tal, así que convertirlo en objeto
//! rompería a todo consumidor existente. Un endpoint hermano es aditivo, y
//! una build anterior simplemente devuelve 404.
use axum::{Json, extract::State, response::{IntoResponse, Response}};
use std::sync::Arc;

use crate::state::AppState;
use crate::telemetry::SessionSnapshot;

/// `GET /sessions` → agregación por `(source, key)`.
///
/// Responde qué costó cada SESIÓN, no cada modelo. Para quien corre varios
/// agentes a la vez, el gasto por modelo no dice quién lo generó.
pub async fn handle_sessions(State(state): State<Arc<AppState>>) -> Response {
    let registry = state.telemetry.sessions();

    // Read-lock BREVE, sin `.await` dentro: se toma, se construye el snapshot
    // (todo síncrono) y se suelta antes de responder.
    let snapshot: SessionSnapshot = match registry.read() {
        Ok(guard) => guard.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };

    Json(serde_json::json!({
        "sessions": snapshot.0,
        // Si se alcanzó el tope de sesiones distintas, las filas son una COTA
        // INFERIOR honesta: faltan claves nuevas que se dejaron de admitir.
        "saturated": snapshot.1,
    }))
    .into_response()
}
