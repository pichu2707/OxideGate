//! Endpoint HTTP de liveness: responde si el servidor está sirviendo.
//!
//! A diferencia de `/stats` y `/requests`, este handler NO depende de
//! `AppState`: no toma locks de telemetría ni de ninguna estructura
//! compartida. Es la ruta más barata del binario a propósito, porque el
//! plugin de OpenCode la usa para decidir en caliente si redirige tráfico de
//! Codex hacia acá antes de tocar nada más pesado.
use axum::{
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// `GET /health` → liveness: 200 con un payload mínimo y estable mientras el
/// servidor esté sirviendo peticiones. No agrega métricas, uptime ni
/// versión: un cliente externo depende de este contrato, así que se
/// mantiene chico a propósito.
pub async fn handle_health() -> Response {
    Json(json!({"status": "ok"})).into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{header, StatusCode};

    #[tokio::test]
    async fn handle_health_responde_200_con_status_ok() {
        let response = super::handle_health().await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);
    }
}
