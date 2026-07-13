//! Captura cruda de la cuota de suscripción de Codex (OAuth, `chatgpt.com`).
//!
//! Cuando una petición se enruta al backend de Codex vía OAuth (plan de
//! suscripción, no API key), la respuesta del upstream trae doce cabeceras
//! `x-codex-*` que describen el estado de la cuota: porcentaje de ventana
//! consumido, minutos de ventana, momentos de reseteo y, si el plan lo
//! expone, el saldo de créditos. Este módulo parsea esas cabeceras SIN
//! derivar nada: ni agregación, ni coste nocional, ni delta entre filas —
//! eso son rebanadas posteriores del mismo eje (ver `proposal.md`).
//!
//! # Cuota, NUNCA dólares
//!
//! [`CodexQuota`] es un tipo separado, en un módulo separado, SIN ningún
//! campo en USD. Es estructuralmente incapaz de alimentar
//! `cost_estimate_usd`: la cuota es un PORCENTAJE de ventana consumida en un
//! plan de suscripción de precio fijo, no un importe. Mezclar ambas monedas
//! en un mismo número sería directamente incorrecto — no una simplificación,
//! un error. Ver la sección "Invariante de honestidad" de `design.md` para
//! la garantía completa.
//!
//! # Presencia como única señal discriminadora
//!
//! [`CodexQuota::from_headers`] se dispara por la PRESENCIA de cabeceras
//! `x-codex-*` en la respuesta, nunca por la identidad del upstream ni por
//! el slug del modelo. `api.openai.com` vía API key comparte el proveedor
//! `openai` pero jamás manda estas cabeceras; Anthropic y Gemini tampoco.
//! Si ninguna de las doce cabeceras está presente, `from_headers` devuelve
//! `None` completo — nunca un `CodexQuota` con los doce campos en `None`
//! (esa forma se reserva para "hay cuota pero algún campo puntual faltó").
//!
//! # Contrato de saneo (compartido por los doce campos)
//!
//! - Cabecera ausente → campo `None`.
//! - Cabecera presente pero con valor vacío → campo `None`, NUNCA `Some("")`
//!   ni un `0` fabricado (`x-codex-secondary-reset-at` llega vacía en
//!   captura real, confirmado en `spec.md`).
//! - Valor numérico no parseable → campo `None`. El parseo NUNCA hace
//!   `panic`: un dialecto de cabecera mal formado no puede tumbar el proxy.
//! - Campos booleanos: solo `"True"`/`"False"` (capitalizados, tal como los
//!   manda Codex) parsean a `bool`; cualquier otro valor, incluido vacío o
//!   minúsculas, mapea a `None`.
//!
//! # Nota de scaffolding (temporal, este commit)
//!
//! `OxideGate` es un crate SOLO-binario (sin `src/lib.rs`): sin un blanco de
//! biblioteca, `#[warn(dead_code)]` trata `pub` como "reachable" únicamente
//! si algo en el grafo alcanzable desde `main` lo usa de verdad, no por ser
//! `pub`. Este commit introduce el módulo en aislamiento (unidad de trabajo
//! 1 de 2, ver `tasks.md`); el cableado que lo vuelve alcanzable llega en el
//! commit siguiente. El `allow` de abajo es ESTRICTAMENTE transitorio: se
//! retira en cuanto `MetricBase`/`RequestMetric`/`RecentRequest` empiecen a
//! construir `CodexQuota` de verdad.
#![allow(dead_code)]
use reqwest::header::HeaderMap;

/// Nombres de las doce cabeceras `x-codex-*`, en el mismo orden que los
/// campos de [`CodexQuota`]. Única fuente de verdad de los nombres de
/// cabecera: [`CodexQuota::from_headers`] los reutiliza tanto para decidir
/// presencia como para el parseo campo a campo.
const HEADER_NAMES: [&str; 12] = [
    "x-codex-plan-type",
    "x-codex-active-limit",
    "x-codex-credits-balance",
    "x-codex-primary-used-percent",
    "x-codex-secondary-used-percent",
    "x-codex-primary-window-minutes",
    "x-codex-secondary-window-minutes",
    "x-codex-primary-reset-after-seconds",
    "x-codex-primary-reset-at",
    "x-codex-secondary-reset-at",
    "x-codex-credits-has-credits",
    "x-codex-credits-unlimited",
];

/// Estado de la cuota de suscripción de Codex en el momento de una
/// respuesta puntual, tal como lo reportan las cabeceras `x-codex-*` del
/// upstream. Ver la documentación del módulo para el contrato de saneo
/// compartido por los doce campos y la garantía de separación respecto de
/// `cost_estimate_usd`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CodexQuota {
    /// Tipo de plan de suscripción (`x-codex-plan-type`), crudo. `None` si
    /// la cabecera falta o llega vacía.
    pub plan_type: Option<String>,
    /// Cuál de las dos ventanas (primaria/secundaria) es la que limita hoy
    /// (`x-codex-active-limit`), crudo. `None` si la cabecera falta o llega
    /// vacía.
    pub active_limit: Option<String>,
    /// Saldo de créditos del plan (`x-codex-credits-balance`), STRING CRUDA
    /// sin ningún intento de parseo numérico — el formato exacto del saldo
    /// (unidades, notación) no está garantizado por el upstream, así que se
    /// transporta tal cual llega. `None` si la cabecera falta o llega vacía.
    pub credits_balance: Option<String>,
    /// Porcentaje consumido de la ventana primaria de rate limit
    /// (`x-codex-primary-used-percent`). `None` si la cabecera falta, llega
    /// vacía o no parsea como entero.
    pub primary_used_percent: Option<u64>,
    /// Porcentaje consumido de la ventana secundaria de rate limit
    /// (`x-codex-secondary-used-percent`). Mismo saneo que
    /// `primary_used_percent`.
    pub secondary_used_percent: Option<u64>,
    /// Duración en minutos de la ventana primaria
    /// (`x-codex-primary-window-minutes`). Mismo saneo que
    /// `primary_used_percent`.
    pub primary_window_minutes: Option<u64>,
    /// Duración en minutos de la ventana secundaria
    /// (`x-codex-secondary-window-minutes`). Mismo saneo que
    /// `primary_used_percent`.
    pub secondary_window_minutes: Option<u64>,
    /// Segundos hasta que la ventana primaria resetea
    /// (`x-codex-primary-reset-after-seconds`). Mismo saneo que
    /// `primary_used_percent`.
    pub primary_reset_after_seconds: Option<u64>,
    /// Instante (timestamp unix) en que resetea la ventana primaria
    /// (`x-codex-primary-reset-at`). `None` si la cabecera falta, llega
    /// vacía o no parsea como entero.
    pub primary_reset_at: Option<i64>,
    /// Instante (timestamp unix) en que resetea la ventana secundaria
    /// (`x-codex-secondary-reset-at`). Confirmado en captura real que esta
    /// cabecera puede llegar PRESENTE pero VACÍA: ese caso sanea a `None`,
    /// nunca a `Some(0)`. Mismo saneo que `primary_reset_at` en el resto de
    /// los casos.
    pub secondary_reset_at: Option<i64>,
    /// `true`/`false` según el plan tenga créditos disponibles
    /// (`x-codex-credits-has-credits`). Solo se reconoce el valor EXACTO
    /// `"True"` o `"False"` (capitalizado, tal como lo manda Codex);
    /// cualquier otro valor, incluida una cabecera vacía, mapea a `None`.
    pub credits_has_credits: Option<bool>,
    /// `true`/`false` según el plan tenga créditos ilimitados
    /// (`x-codex-credits-unlimited`). Mismo saneo estricto que
    /// `credits_has_credits`.
    pub credits_unlimited: Option<bool>,
}

impl CodexQuota {
    /// Parsea las doce cabeceras `x-codex-*` de una respuesta del upstream.
    ///
    /// Devuelve `None` si NINGUNA de las doce cabeceras está presente — la
    /// presencia es la única señal discriminadora (ver doc del módulo):
    /// tráfico de Anthropic, Gemini o de OpenAI vía API key nunca trae
    /// ninguna de estas cabeceras, así que siempre cae en esta rama.
    ///
    /// Si AL MENOS UNA cabecera está presente, devuelve `Some(CodexQuota)`
    /// con los doce campos parseados independientemente: un campo puntual
    /// ausente, vacío o malformado sanea a `None` sin afectar al resto ni
    /// hacer `panic`.
    pub fn from_headers(headers: &HeaderMap) -> Option<CodexQuota> {
        let any_present = HEADER_NAMES.iter().any(|name| headers.contains_key(*name));
        if !any_present {
            return None;
        }

        Some(CodexQuota {
            plan_type: parse_string(headers, "x-codex-plan-type"),
            active_limit: parse_string(headers, "x-codex-active-limit"),
            credits_balance: parse_string(headers, "x-codex-credits-balance"),
            primary_used_percent: parse_u64(headers, "x-codex-primary-used-percent"),
            secondary_used_percent: parse_u64(headers, "x-codex-secondary-used-percent"),
            primary_window_minutes: parse_u64(headers, "x-codex-primary-window-minutes"),
            secondary_window_minutes: parse_u64(headers, "x-codex-secondary-window-minutes"),
            primary_reset_after_seconds: parse_u64(headers, "x-codex-primary-reset-after-seconds"),
            primary_reset_at: parse_i64(headers, "x-codex-primary-reset-at"),
            secondary_reset_at: parse_i64(headers, "x-codex-secondary-reset-at"),
            credits_has_credits: parse_bool(headers, "x-codex-credits-has-credits"),
            credits_unlimited: parse_bool(headers, "x-codex-credits-unlimited"),
        })
    }
}

/// Lee una cabecera cruda como string. `None` si falta, si el valor no es
/// UTF-8 válido, o si está presente pero vacía (nunca `Some("")`).
fn parse_string(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Lee una cabecera y la parsea como `u64`. `None` si falta, no es UTF-8
/// válido, está vacía, o no parsea como entero sin signo — nunca hace
/// `panic` ni fabrica un `0` por defecto.
fn parse_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    let raw = headers.get(name)?.to_str().ok()?;
    if raw.is_empty() {
        return None;
    }
    raw.parse::<u64>().ok()
}

/// Lee una cabecera y la parsea como `i64` (usada para los timestamps unix
/// de reseteo, que se modelan con signo aunque en la práctica sean
/// positivos). Mismo saneo que [`parse_u64`].
fn parse_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    let raw = headers.get(name)?.to_str().ok()?;
    if raw.is_empty() {
        return None;
    }
    raw.parse::<i64>().ok()
}

/// Lee una cabecera y la parsea como `bool`, aceptando ÚNICAMENTE los
/// literales exactos `"True"`/`"False"` (capitalizados, confirmados en
/// captura real). Cualquier otro valor — minúsculas, `"1"`, vacío, o
/// cabecera ausente — mapea a `None`.
fn parse_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    match headers.get(name)?.to_str().ok()? {
        "True" => Some(true),
        "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    /// Inserta una cabecera cruda en un `HeaderMap` de prueba.
    fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("valor de cabecera de prueba válido"),
        );
    }

    /// Scenario (spec §"Respuesta de Codex con las doce cabeceras
    /// presentes"): con las doce cabeceras presentes y válidas, los doce
    /// campos parsean a `Some` con el valor esperado — incluye el caso
    /// "valor numérico válido" de la spec.
    #[test]
    fn parsea_las_doce_cabeceras_presentes_con_valores_validos() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-codex-plan-type", "pro");
        insert(&mut headers, "x-codex-active-limit", "primary");
        insert(&mut headers, "x-codex-credits-balance", "12.50");
        insert(&mut headers, "x-codex-primary-used-percent", "4");
        insert(&mut headers, "x-codex-secondary-used-percent", "12");
        insert(&mut headers, "x-codex-primary-window-minutes", "300");
        insert(&mut headers, "x-codex-secondary-window-minutes", "10080");
        insert(&mut headers, "x-codex-primary-reset-after-seconds", "1800");
        insert(&mut headers, "x-codex-primary-reset-at", "1732000000");
        insert(&mut headers, "x-codex-secondary-reset-at", "1732600000");
        insert(&mut headers, "x-codex-credits-has-credits", "False");
        insert(&mut headers, "x-codex-credits-unlimited", "False");

        let quota = CodexQuota::from_headers(&headers).expect("debe parsear con las 12 cabeceras");

        assert_eq!(quota.plan_type, Some("pro".to_string()));
        assert_eq!(quota.active_limit, Some("primary".to_string()));
        assert_eq!(quota.credits_balance, Some("12.50".to_string()));
        assert_eq!(quota.primary_used_percent, Some(4));
        assert_eq!(quota.secondary_used_percent, Some(12));
        assert_eq!(quota.primary_window_minutes, Some(300));
        assert_eq!(quota.secondary_window_minutes, Some(10080));
        assert_eq!(quota.primary_reset_after_seconds, Some(1800));
        assert_eq!(quota.primary_reset_at, Some(1_732_000_000));
        assert_eq!(quota.secondary_reset_at, Some(1_732_600_000));
        assert_eq!(quota.credits_has_credits, Some(false));
        assert_eq!(quota.credits_unlimited, Some(false));
    }

    /// Scenario (spec §"Tráfico sin cabeceras de cuota"): sin ninguna
    /// cabecera `x-codex-*`, `from_headers` devuelve `None` completo (nunca
    /// un `CodexQuota` con los doce campos en `None`).
    #[test]
    fn sin_ninguna_cabecera_x_codex_devuelve_none() {
        let headers = HeaderMap::new();
        assert!(CodexQuota::from_headers(&headers).is_none());
    }

    /// Scenario (spec §"x-codex-secondary-reset-at vacío"): una cabecera
    /// presente pero con valor vacío sanea a `None`, nunca a `Some("")` ni
    /// a `Some(0)`.
    #[test]
    fn cabecera_presente_pero_vacia_da_none_no_some_vacio_ni_cero_fabricado() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-codex-plan-type", "pro");
        insert(&mut headers, "x-codex-secondary-reset-at", "");

        let quota = CodexQuota::from_headers(&headers).expect("hay al menos una cabecera presente");
        assert_eq!(quota.secondary_reset_at, None);
    }

    /// Scenario (spec §"Cabecera numérica malformada o ausente"): un valor
    /// no parseable como número sanea a `None` sin hacer `panic`.
    #[test]
    fn valor_numerico_malformado_da_none_sin_panic() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-codex-primary-used-percent", "no-es-un-numero");

        let quota = CodexQuota::from_headers(&headers).expect("hay al menos una cabecera presente");
        assert_eq!(quota.primary_used_percent, None);
    }

    /// Scenario (spec §"Valores booleanos reconocidos"): `"True"`/`"False"`
    /// parsean a `Some(bool)` correctamente.
    #[test]
    fn booleanos_true_false_capitalizados_parsean_correctamente() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-codex-credits-has-credits", "False");
        insert(&mut headers, "x-codex-credits-unlimited", "False");

        let quota = CodexQuota::from_headers(&headers).expect("hay al menos una cabecera presente");
        assert_eq!(quota.credits_has_credits, Some(false));
        assert_eq!(quota.credits_unlimited, Some(false));
    }

    /// Scenario (spec §"Valor booleano no reconocido"): minúsculas, `"1"` u
    /// otro valor distinto de `"True"`/`"False"` sanea a `None`.
    #[test]
    fn booleano_no_reconocido_da_none() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-codex-credits-has-credits", "true");
        insert(&mut headers, "x-codex-credits-unlimited", "1");

        let quota = CodexQuota::from_headers(&headers).expect("hay al menos una cabecera presente");
        assert_eq!(quota.credits_has_credits, None);
        assert_eq!(quota.credits_unlimited, None);
    }

    /// Prueba de honestidad estructural: `CodexQuota` no tiene ningún campo
    /// en dólares y su JSON serializado nunca contiene rastro de USD/coste —
    /// la separación de `cost_estimate_usd` es estructural, no una
    /// convención que dependa de que nadie se olvide de respetarla.
    #[test]
    fn codex_quota_no_tiene_campo_en_dolares_ni_ruta_a_cost_estimate_usd() {
        let quota = CodexQuota {
            plan_type: Some("pro".to_string()),
            active_limit: Some("primary".to_string()),
            credits_balance: Some("12.50".to_string()),
            primary_used_percent: Some(4),
            secondary_used_percent: Some(12),
            primary_window_minutes: Some(300),
            secondary_window_minutes: Some(10080),
            primary_reset_after_seconds: Some(1800),
            primary_reset_at: Some(1_732_000_000),
            secondary_reset_at: Some(1_732_600_000),
            credits_has_credits: Some(false),
            credits_unlimited: Some(false),
        };

        let json = serde_json::to_string(&quota).expect("CodexQuota serializa a JSON");
        let lowered = json.to_lowercase();
        assert!(!lowered.contains("usd"), "CodexQuota no debe mencionar USD");
        assert!(
            !lowered.contains("cost"),
            "CodexQuota no debe mencionar cost/cost_estimate"
        );
    }
}
