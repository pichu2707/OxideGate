//! `GET /history` — desde cuándo miden los agregados.
//!
//! # Por qué una ruta nueva y no un campo en `/stats`
//!
//! Un agregado sin ventana declarada es **un número sin unidades**: hoy
//! `/stats` no dice si cubre cinco minutos o cinco días, y desde que existe la
//! rehidratación (`telemetry::rehydrate`) esa diferencia importa de verdad.
//!
//! Lo natural sería añadir el dato a `/stats`. **No se puede sin romper**:
//! `StatsSnapshot` es un `Vec`, así que `/stats` serializa como ARRAY. Meterle
//! la ventana lo convertiría en objeto — ruptura de contrato, subida de
//! `CONTRACT_VERSION`, y de paso rompería el `brew test` de la fórmula, que
//! afirma `[]` sobre un proxy recién arrancado.
//!
//! Una ruta nueva es aditiva: `ENDPOINTS` pasa de cuatro a cinco, nadie que ya
//! consumiera `/stats` se entera, y quien quiera la ventana la pide.
//!
//! # No depende de `AppState`
//!
//! Igual que `/health` y `/version`: el resultado de la rehidratación se
//! calcula una vez al arrancar y no cambia después, así que se sirve desde un
//! valor congelado en vez de tomar el lock de telemetría.

use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::telemetry::Rehydrated;

/// `GET /history` → qué histórico se releyó al arrancar.
///
/// ```json
/// {
///   "window_days": 7,
///   "rows": 1284,
///   "oldest": "2026-07-24T09:12:44Z",
///   "skipped_old": 1676,
///   "skipped_bad": 0
/// }
/// ```
///
/// `oldest` es la respuesta a «¿desde cuándo mide `/stats`?». `null` significa
/// que no se rehidrató nada: primer arranque, ventana a cero, o ninguna fila
/// dentro de la ventana. **No significa «desde ahora»** — quien quiera esa
/// lectura tiene que mirar también `rows`.
///
/// `skipped_bad > 0` es la señal de que el fichero tiene filas ilegibles. Se
/// publica en vez de esconderse: un histórico que se está corrompiendo debe
/// poder verse desde fuera antes de que se coma la ventana entera.
pub async fn handle_history(State(estado): State<HistoryState>) -> Response {
    Json(json!({
        "window_days": estado.window_days,
        "rows": estado.rehydrated.rows,
        "oldest": estado.rehydrated.oldest,
        "skipped_old": estado.rehydrated.skipped_old,
        "skipped_bad": estado.rehydrated.skipped_bad,
    }))
    .into_response()
}

/// Estado congelado de la rehidratación, resuelto una vez al arrancar.
#[derive(Debug, Clone, Default)]
pub struct HistoryState {
    /// Ventana en días que se pidió (`OXIDEGATE_HISTORY_DAYS`). `0` = la
    /// rehidratación estaba desactivada.
    pub window_days: u32,
    /// Lo que la rehidratación consiguió leer.
    pub rehydrated: Rehydrated,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};

    async fn body(estado: HistoryState) -> serde_json::Value {
        let r = handle_history(State(estado)).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Las cinco claves son el contrato de esta ruta.
    #[tokio::test]
    async fn publica_las_cinco_claves() {
        let v = body(HistoryState::default()).await;

        for c in [
            "window_days",
            "rows",
            "oldest",
            "skipped_old",
            "skipped_bad",
        ] {
            assert!(v.get(c).is_some(), "falta `{c}`: {v}");
        }
    }

    /// Sin rehidratación, `oldest` es `null` — NO una fecha inventada ni la de
    /// arranque. Es la misma distinción hueco-vs-cero del resto del proyecto.
    #[tokio::test]
    async fn sin_rehidratacion_oldest_es_null_y_no_una_fecha_fabricada() {
        let v = body(HistoryState::default()).await;

        assert!(v["oldest"].is_null());
        assert_eq!(v["rows"], 0);
    }

    /// Las filas ilegibles se publican aunque la rehidratación fuera bien: son
    /// una señal independiente de que el fichero se está corrompiendo.
    #[tokio::test]
    async fn las_filas_ilegibles_se_publican_aunque_haya_habido_rehidratacion() {
        let v = body(HistoryState {
            window_days: 7,
            rehydrated: Rehydrated {
                rows: 100,
                skipped_old: 20,
                skipped_bad: 3,
                oldest: Some("2026-07-24T09:12:44Z".to_string()),
            },
        })
        .await;

        assert_eq!(v["rows"], 100);
        assert_eq!(v["skipped_bad"], 3);
        assert_eq!(v["oldest"], "2026-07-24T09:12:44Z");
        assert_eq!(v["window_days"], 7);
    }
}
