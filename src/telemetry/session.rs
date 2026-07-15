//! Tipo de acarreo del eje de atribución de sesiones (rebanada 1).
//!
//! Este módulo define únicamente la FORMA de la atribución de sesión
//! ([`SessionAttribution`]/[`SessionSource`]); la RESOLUCIÓN (leer cabeceras,
//! aplicar precedencia, sanear) vive en `middleware::proxy::session_of`, junto
//! a `client_of`, porque es transporte agnóstico de proveedor, no telemetría
//! (ver `design.md` §"Decisión 1"). Aquí solo viaja el dato ya resuelto por la
//! misma cadena que ya recorren `client`, `tools_by_server` y `codex_quota`:
//! `MetricBase` → `RequestMetric` → `RecentRequest`.
//!
//! # Contrato de honestidad: `source` y `key` son inseparables
//!
//! Una `key` de `claude-cli/1.2.3` significa cosas opuestas según su
//! `source`: con [`SessionSource::Native`] es una sesión real atribuida por
//! Claude Code; con [`SessionSource::Unattributed`] es solo el `User-Agent`
//! del fallback, NO una identidad. Por eso ambos campos viajan SIEMPRE juntos
//! en un único struct, nunca como dos campos planos independientes: es
//! estructuralmente imposible tener una `key` sin su procedencia.
//!
//! # Precedencia de tres niveles (aplicada por el resolver)
//!
//! 1. `X-OxideGate-Session` (header de request) → [`SessionSource::Explicit`].
//! 2. `x-claude-code-session-id` (header de request, nativo de Claude Code) →
//!    [`SessionSource::Native`].
//! 3. Fallback: `User-Agent` de la petición, o la constante `"unattributed"`
//!    si el `User-Agent` falta o no es UTF-8 válido → [`SessionSource::Unattributed`].
//!
//! Un header de atribución presente pero con valor vacío se trata como
//! ausente y la resolución cae al siguiente nivel: la `key` NUNCA es un
//! string vacío (`Some("")`), en ninguna rama.
//!
//! # `session` NO es `Option`
//!
//! El campo que acarrea este tipo en `MetricBase`/`RequestMetric`/
//! `RecentRequest` es `session: SessionAttribution`, nunca
//! `Option<SessionAttribution>`: la precedencia SIEMPRE resuelve a algo — la
//! peor rama es `Unattributed`, que es un bucket explícito, no una ausencia.
//! Modelarlo como `Option` colapsaría "no hubo header" con "no hay sesión" y
//! perdería la señal que este eje existe para exponer.
//!
//! # Invariante de privacidad
//!
//! El resolver lee EXCLUSIVAMENTE `X-OxideGate-Session`,
//! `x-claude-code-session-id` y `User-Agent`. Jamás `Authorization`,
//! `x-api-key` ni `x-goog-api-key`. La `key` es siempre una etiqueta o
//! identificador opaco, jamás una credencial cruda.

/// Señal de precedencia que ganó la resolución de sesión: indica CÓMO
/// interpretar la `key` que la acompaña en [`SessionAttribution`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    /// `X-OxideGate-Session` presente y no vacío: etiqueta explícita
    /// asignada por quien invoca (máxima precedencia).
    Explicit,
    /// `X-OxideGate-Session` ausente o vacío, `x-claude-code-session-id`
    /// presente y no vacío: id de sesión nativo de Claude Code.
    Native,
    /// Ni el header explícito ni el nativo resolvieron: bucket de fallback
    /// honesto, la `key` es el `User-Agent` (o la constante
    /// `"unattributed"` si no hay `User-Agent` legible). NUNCA una
    /// identidad inventada.
    Unattributed,
}

/// Sesión resuelta para un request puntual: procedencia (`source`) y valor
/// opaco (`key`), inseparables por construcción (ver doc del módulo).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SessionAttribution {
    /// Qué señal de precedencia ganó la resolución. Fija cómo interpretar
    /// `key`: una `key` de fallback bajo `Unattributed` NO es una identidad,
    /// aunque el string parezca uno.
    pub source: SessionSource,
    /// Valor opaco resuelto: el header de atribución crudo
    /// (`Explicit`/`Native`) o el `User-Agent`/constante de fallback
    /// (`Unattributed`). Sin su `source` acompañante, `key` no es
    /// interpretable — nunca se expone ni se acarrea por separado.
    pub key: String,
}
