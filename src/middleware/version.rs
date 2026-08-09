//! Endpoint HTTP de capacidades: qué versión sirve este proxy y qué publica.
//!
//! # Por qué existe, y por qué NO es `/health`
//!
//! `/health` es liveness y se mantiene chico a propósito (ver
//! [`middleware::health`](crate::middleware::health)): un cliente externo
//! depende de que su payload no cambie. Pero entonces no quedaba ninguna ruta
//! que respondiera «soy la versión X y publico estos campos», y un consumidor
//! sólo podía averiguarlo haciendo una petición real, mirando el JSON y
//! deduciéndolo. Es decir: **sondear por ausencia**.
//!
//! Eso ya falló dos veces, siempre igual: dos piezas correctas a ambos lados y
//! un fallo silencioso en medio, porque el consumidor no puede distinguir
//! «este proxy no lo soporta» de «aquí no había dato».
//!
//! - `tool_names`: `oxidegate-lens` consume el campo, está testeado y en su
//!   `main`, y no hacía nada contra un proxy 0.3.1 porque el campo no existía.
//! - `GET /health`: existía en el código desde 0.3.0 mientras el tap servía
//!   0.2.1. El plugin sondeaba la ruta, recibía 404 y caía al proveedor
//!   directo en silencio.
//!
//! Es exactamente la confusión que el resto del proyecto se niega a cometer
//! con los datos: **un hueco honesto no es un cero**. Esta ruta le da al
//! consumidor la tercera respuesta que le faltaba.
//!
//! # Es aditiva
//!
//! Una build anterior devuelve 404 en `/version`, y eso ya es una respuesta
//! útil: «este proxy es anterior a la introducción del contrato». `/health`
//! queda intacto.
//!
//! # No depende de `AppState`
//!
//! Igual que `/health`: no toma locks de telemetría. Lo que declara es
//! estático — se conoce en tiempo de compilación, no depende de qué se haya
//! medido.
use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Versión del CONTRATO que publica este proxy, independiente de la versión
/// del crate.
///
/// Sube **solo cuando se rompe** algo que un consumidor ya usaba: renombrar un
/// campo, cambiar su tipo o cambiar su unidad. Añadir un campo o un endpoint
/// nuevo es aditivo y NO la mueve — para eso está [`FIELDS`], que permite
/// detectar la presencia de una capacidad concreta sin tener que comparar
/// números de versión.
///
/// Arranca en `1` porque es el primer contrato declarado: antes de esta ruta
/// no había ninguno que un consumidor pudiera leer.
pub const CONTRACT_VERSION: u32 = 1;

/// Endpoints de telemetría que sirve este binario.
///
/// `/health` se lista igual que el resto: un consumidor que descubre el proxy
/// por `/version` necesita saber que la ruta de liveness existe, y omitirla
/// aquí reproduciría en pequeño el mismo agujero que esta ruta viene a tapar.
pub const ENDPOINTS: [&str; 6] = [
    "/health",
    "/stats",
    "/sessions",
    "/requests",
    "/mcp",
    "/history",
];

/// Campos que un consumidor puede necesitar sondear antes de dibujar una
/// columna.
///
/// NO es la lista completa de claves publicadas —para eso está el propio JSON
/// de cada endpoint— sino el subconjunto que marca una CAPACIDAD: campos
/// añadidos después de que hubiera consumidores, cuya ausencia significa
/// «actualiza el proxy» y no «aquí no había dato».
///
/// La diferencia importa: con esta lista, `oxidegate-savings --doctor` puede
/// decir *«tu OxideGate es antiguo, actualiza para ver esta columna»* en vez
/// de enseñar una tabla vacía sin explicar por qué.
///
/// Un test (`telemetry::recent`) comprueba que cada entrada de aquí aparece de
/// verdad en el JSON de `/requests`: esta lista no puede anunciar una
/// capacidad que el proxy no tenga.
pub const FIELDS: [&str; 14] = [
    "cache_by_section",
    "input_share_by_section",
    "prompt_bytes",
    "tool_names",
    "tool_calls",
    "tool_search",
    "tools_flattened",
    "skills",
    "instructions",
    "effort_forced",
    "response_bytes",
    "codex_quota",
    "session",
    "prepare_us",
];

/// Payload de `/version`. Función pura para poder afirmar sobre ella en tests
/// sin levantar el servidor.
fn version_payload() -> serde_json::Value {
    json!({
        "oxidegate": env!("CARGO_PKG_VERSION"),
        "contract": CONTRACT_VERSION,
        "endpoints": ENDPOINTS,
        "fields": FIELDS,
    })
}

/// `GET /version` → qué versión es este proxy y qué capacidades publica.
///
/// Ruta de descubrimiento, no de liveness: para saber si el proxy está vivo
/// sigue estando `/health`, que es más barata y cuyo contrato no se toca.
pub async fn handle_version() -> Response {
    Json(version_payload()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};

    async fn body_json() -> serde_json::Value {
        let response = handle_version().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// La versión anunciada es la del binario que responde. Si se desacoplara,
    /// un consumidor podría creer que habla con una build que no es.
    #[tokio::test]
    async fn anuncia_la_version_real_del_binario() {
        let v = body_json().await;

        assert_eq!(v["oxidegate"], env!("CARGO_PKG_VERSION"));
    }

    /// Las cuatro claves son el contrato de esta ruta. Que falte cualquiera
    /// deja al consumidor otra vez deduciendo.
    #[tokio::test]
    async fn publica_las_cuatro_claves_del_contrato() {
        let v = body_json().await;

        for clave in ["oxidegate", "contract", "endpoints", "fields"] {
            assert!(v.get(clave).is_some(), "falta la clave `{clave}`: {v}");
        }
        assert_eq!(v["contract"], CONTRACT_VERSION);
    }

    /// `endpoints` lista rutas, no descripciones. Un consumidor las concatena
    /// a la base-URL tal cual.
    #[tokio::test]
    async fn los_endpoints_son_rutas_absolutas_y_estan_todos() {
        let v = body_json().await;
        let listados: Vec<&str> = v["endpoints"]
            .as_array()
            .expect("endpoints debe ser un array")
            .iter()
            .map(|e| e.as_str().expect("cada endpoint es un string"))
            .collect();

        for ruta in ["/health", "/stats", "/sessions", "/requests"] {
            assert!(listados.contains(&ruta), "no lista {ruta}: {listados:?}");
        }
        assert!(
            listados.iter().all(|r| r.starts_with('/')),
            "hay endpoints que no son rutas: {listados:?}"
        );
    }

    /// `fields` no puede venir vacío: es la única parte de esta ruta que
    /// permite distinguir «no soportado» de «sin dato», que es el fallo
    /// concreto que motivó el endpoint.
    #[tokio::test]
    async fn fields_declara_al_menos_las_capacidades_que_ya_rompieron() {
        let v = body_json().await;
        let campos: Vec<&str> = v["fields"]
            .as_array()
            .expect("fields debe ser un array")
            .iter()
            .map(|e| e.as_str().expect("cada campo es un string"))
            .collect();

        // `tool_names` es el caso literal: la lens lo consume y el proxy
        // instalado no lo tenía.
        assert!(
            campos.contains(&"tool_names"),
            "no declara tool_names: {campos:?}"
        );
        assert!(!campos.is_empty());
    }

    /// El contrato arranca en 1 y solo sube al ROMPER. Este test no impide
    /// subirlo: obliga a que subirlo sea un acto deliberado y visible en el
    /// diff, no un efecto colateral de añadir un campo.
    #[test]
    fn la_version_del_contrato_es_independiente_de_la_del_crate() {
        assert_eq!(CONTRACT_VERSION, 1);
    }
}
