//! Endpoint HTTP del cruce coste-vs-uso por servidor MCP.
//!
//! Único archivo de esta cadena que conoce axum: la agregación vive en
//! [`telemetry::mcp`](crate::telemetry::mcp) y es framework-agnóstica. Acá
//! solo se toma el read-lock, se construye el snapshot y se serializa.
use crate::state::AppState;
use crate::telemetry::McpSnapshot;
use axum::{Json, extract::State, response::IntoResponse, response::Response};
use std::sync::Arc;

/// `GET /mcp` → qué cuesta cada servidor MCP y cuánto se usa.
///
/// Responde a la única pregunta que necesita los dos lados del cable a la
/// vez: los bytes viven en la petición y las invocaciones en la respuesta.
///
/// **Publica el cruce Y el veredicto, con la evidencia al lado.** Cada fila
/// lleva las peticiones concluyentes que lo sostienen y los descartes por
/// motivo, y el snapshot lleva el `umbral` contra el que se comparó — porque
/// ese número es un juicio, no una medida, y quien no esté de acuerdo tiene
/// los conteos crudos para aplicar el suyo.
///
/// No admite `?since=`, a diferencia de `/stats`. La pregunta que contesta es
/// sobre un HÁBITO ("¿lo usas o no?"), y un hábito se mide sobre todo lo
/// acumulado: acotar la ventana solo serviría para bajar artificialmente las
/// peticiones concluyentes y convertir un `unused` en un `insufficient_data`.
/// Si algún día hace falta, entra como parámetro nuevo y aditivo.
///
/// No requiere autenticación, igual que el resto: el proxy bindea en
/// `127.0.0.1` y esto expone nombres de servidor y conteos, nunca prompts ni
/// huellas. Ver la §3 de `docs/telemetry-per-request.md` antes de exponerlo
/// fuera de localhost: los identificadores de herramienta describen qué
/// integraciones tiene montadas quien usa el proxy.
pub async fn handle_mcp(State(state): State<Arc<AppState>>) -> Response {
    let registry = state.telemetry.mcp();

    // Read-lock BREVE: se toma, se construye el snapshot (todo síncrono) y se
    // suelta antes de devolver la respuesta. Nunca cruza un `.await`.
    let snapshot: McpSnapshot = match registry.read() {
        Ok(guard) => guard.snapshot(),
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };

    Json(snapshot).into_response()
}
