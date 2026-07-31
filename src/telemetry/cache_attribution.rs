//! Atribución ESTIMADA de la caché de prompt a cada sección del contexto.
//!
//! Responde a una única pregunta: **qué cubo del contexto cayó dentro del
//! prefijo cacheado**. Sin eso, cualquier reparto de coste por sección se
//! equivoca por un factor cercano a 10 justo en los cubos que más pesan, porque
//! un token leído de caché cuesta el 10% de la tarifa.
//!
//! # Esto es una ESTIMACIÓN, no una medición
//!
//! El resto de campos `context_*_bytes` de [`RequestMetric`] son medición
//! directa: se cuentan bytes del body. Lo de aquí NO lo es. El proveedor
//! reporta los tokens cacheados en TOTAL, nunca por sección, así que la
//! posición de la frontera se deduce convirtiendo tokens a bytes con la tasa
//! plana de la propia petición. Por eso vive en su propio objeto anidado
//! (`cache_by_section`) y no suelto entre los campos medidos: la frontera entre
//! lo medido y lo estimado tiene que verse en la ESTRUCTURA, no solo en la doc.
//!
//! # El método: paseo por el prefijo
//!
//! El caché de prompt hace *prefix match*: la región cacheada es un prefijo
//! CONTIGUO del prompt. Conocido el orden en que el proveedor ensambla las
//! secciones, basta con convertir los tokens cacheados a una posición en bytes
//! y consumir secciones en ese orden hasta agotarla.
//!
//! `cache_read_tokens` es AUTORITATIVO —lo reporta el proveedor—, así que este
//! módulo NO predice la frontera: la OBSERVA y la coloca.
//!
//! # Qué lo respalda
//!
//! Medido sobre `telemetry.jsonl` (2647 peticiones con status 200, cubos y
//! campos de caché). El falsador —que el paseo llegue a afirmar que el ÚLTIMO
//! TURNO estaba cacheado, imposible por construcción porque es contenido
//! nuevo— no dispara: 0,0% en `codex/gpt-5.5` (n=356), 0,4% en `openai/gpt-5.5`
//! (n=281) y 6,0% en `anthropic/claude-opus-4-8` (n=133) con desbordamiento p95
//! de solo 0,051. Ese residuo encaja con el error de ±10% de la tasa
//! tokens/byte ya medido (ver `docs/telemetry-per-request.md`).
//!
//! Y el efecto justifica el trabajo: en `claude-opus-4-8`, `tools` es el 56,1%
//! de los BYTES pero solo el 22,5% de lo que se PAGA (−33,6 pt), mientras
//! `last_turn` pasa del 7,8% al 31,1% (+23,4 pt). Medir en bytes subestima ~4x
//! lo que cuesta la pregunta real del usuario.
//!
//! # Límites declarados de `prefix_walk_v1`
//!
//! - **Solo se pasea `cache_read`**, no `cache_write`. La lectura se factura al
//!   10% (error de ~10x si se ignora) y la escritura al 125% (error de 1,25x):
//!   el dinero está en la lectura. Atribuir también la escritura es la
//!   evolución natural, y por eso el método va versionado en el propio JSON.
//! - **Un solo orden de prefijo** para todos los proveedores. Validado
//!   empíricamente en los tres cohortes de arriba, no leído de una spec.
//! - La conversión tokens→bytes usa la tasa PLANA de la petición. Medido: las
//!   tasas por sección quedan en 0,90x–1,06x dentro de un mismo modelo, pero
//!   NUNCA mezclando modelos (agregar modelos fabrica un sesgo que no existe).

use serde::Serialize;

use crate::provider::ContextBreakdown;
use crate::telemetry::pricing::{cache_shape_for_upstream, CacheShape};

/// Identificador del algoritmo, publicado dentro del propio objeto.
///
/// Va en el JSON a propósito: permite cambiar el método (atribuir también
/// `cache_write`, afinar el orden por proveedor) sin romper a un consumidor,
/// que puede decidir si entiende esta versión antes de pintar nada.
pub const METHOD: &str = "prefix_walk_v1";

/// Bytes de cada sección que cayeron dentro del prefijo cacheado, ESTIMADOS.
///
/// Los cinco campos suman, como mucho, `ContextBreakdown::measured_bytes`.
/// Todo a cero significa "se midió la caché y no había nada cacheado"; que el
/// objeto entero sea `None` significa "no atribuible" (sin cubos, sin tokens de
/// caché reportados, o `upstream` desconocido). No colapsar ambos casos.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CacheBySection {
    /// Algoritmo con el que se calculó este objeto. Ver [`METHOD`].
    pub method: &'static str,
    /// Bytes de los esquemas de herramientas dentro del prefijo cacheado.
    pub tools_cached_bytes: usize,
    /// Bytes del prompt de sistema dentro del prefijo cacheado.
    pub system_cached_bytes: usize,
    /// Bytes del historial dentro del prefijo cacheado.
    pub history_cached_bytes: usize,
    /// Bytes del último turno dentro del prefijo cacheado. **Debería ser 0 casi
    /// siempre**: el último turno es contenido nuevo. Un valor alto y
    /// sostenido aquí es la señal de que el método dejó de describir el
    /// tráfico — es el falsador, publicado a propósito para que se pueda ver.
    pub last_turn_cached_bytes: usize,
    /// Bytes del resto de campos de control dentro del prefijo cacheado.
    pub other_cached_bytes: usize,
}

/// Orden en que las secciones entran en el prefijo cacheado.
///
/// Anthropic documenta que el prefijo se construye `tools` → `system` →
/// `messages`; dentro de `messages`, el historial precede al último turno por
/// definición. `other` (campos de control a nivel raíz: `model`, `temperature`,
/// `max_tokens`) va al final porque no es contenido del prompt.
///
/// Se aplica el MISMO orden a OpenAI y Gemini. No es una lectura de sus specs
/// —no lo documentan— sino una hipótesis que el falsador no ha conseguido
/// tumbar en 637 peticiones reales de esos proveedores.
const PREFIX_ORDER: [Section; 5] = [
    Section::Tools,
    Section::System,
    Section::History,
    Section::LastTurn,
    Section::Other,
];

/// Una sección del contexto, para poder recorrerlas en orden de prefijo.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Section {
    Tools,
    System,
    History,
    LastTurn,
    Other,
}

impl Section {
    /// Bytes MEDIDOS de esta sección en un desglose dado.
    fn bytes_in(self, c: &ContextBreakdown) -> usize {
        match self {
            Section::Tools => c.tools_bytes,
            Section::System => c.system_bytes,
            Section::History => c.history_bytes,
            Section::LastTurn => c.last_turn_bytes,
            Section::Other => c.other_bytes,
        }
    }

    /// Escribe los bytes atribuidos a esta sección en el resultado.
    fn assign_in(self, out: &mut CacheBySection, bytes: usize) {
        match self {
            Section::Tools => out.tools_cached_bytes = bytes,
            Section::System => out.system_cached_bytes = bytes,
            Section::History => out.history_cached_bytes = bytes,
            Section::LastTurn => out.last_turn_cached_bytes = bytes,
            Section::Other => out.other_cached_bytes = bytes,
        }
    }
}

/// Reparte el prefijo cacheado entre las secciones del contexto.
///
/// Devuelve `None` —"no atribuible", que NO es lo mismo que "nada cacheado"—
/// cuando falta cualquier pieza indispensable: sin un `upstream` reconocido no
/// se sabe si la caché se factura aparte o como subconjunto; sin desglose de
/// contexto no hay secciones que repartir; sin `cache_read_tokens` no hubo
/// medición de caché; y con `measured_bytes` a cero la conversión dividiría
/// por cero.
///
/// Se clava en el `upstream` y NO en el modelo a propósito, por dos razones que
/// se refuerzan:
///
/// 1. **Cobertura**: la FORMA de la contabilidad la fija el proveedor, así que
///    un modelo recién salido se atribuye bien desde el primer request, sin
///    esperar a que alguien le ponga tarifa en la tabla.
/// 2. **Honestidad**: dentro de una misma familia los MULTIPLICADORES divergen
///    —la familia 4o de OpenAI cobra la lectura de caché al 0,5 y la familia 5
///    al 0,1—, así que una «contabilidad de familia» con multiplicadores sería
///    falsa la mitad de las veces. Por eso esto toma [`CacheShape`], que no los
///    lleva: aquí no se factura nada, solo se coloca una frontera.
///
/// Función PURA y total: no toca el camino crítico del request (se llama al
/// emitir la métrica, con la respuesta ya cerrada) y no puede entrar en pánico
/// — la aritmética va saturada y la conversión se hace en `f64` acotado.
pub fn attribute_cache(
    upstream: &str,
    context: Option<&ContextBreakdown>,
    input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
) -> Option<CacheBySection> {
    let context = context?;
    let cache_read = cache_read_tokens?;
    let shape = cache_shape_for_upstream(upstream)?;

    if context.measured_bytes == 0 {
        return None;
    }

    let total_tokens = total_prompt_tokens(
        shape,
        input_tokens.unwrap_or(0),
        cache_read,
        cache_write_tokens.unwrap_or(0),
    );
    if total_tokens == 0 {
        return None;
    }

    // Fracción del prompt que llegó cacheada -> posición de la frontera en
    // bytes. Se acota a [0, 1] antes de convertir: un `cache_read` mayor que el
    // total solo puede venir de datos inconsistentes, y ahí preferimos
    // "todo cacheado" a un desbordamiento silencioso.
    let cached_fraction = (cache_read as f64 / total_tokens as f64).clamp(0.0, 1.0);
    let mut remaining = (context.measured_bytes as f64 * cached_fraction).round() as usize;

    let mut out = CacheBySection {
        method: METHOD,
        tools_cached_bytes: 0,
        system_cached_bytes: 0,
        history_cached_bytes: 0,
        last_turn_cached_bytes: 0,
        other_cached_bytes: 0,
    };

    for section in PREFIX_ORDER {
        let take = section.bytes_in(context).min(remaining);
        section.assign_in(&mut out, take);
        remaining -= take;
    }

    Some(out)
}

/// Total de tokens de PROMPT de la petición, según cómo facture la familia.
///
/// La distinción no es cosmética: en Anthropic la caché va APARTE del
/// `input_tokens` reportado (hay que sumarla para tener el prompt completo),
/// mientras que en OpenAI y Gemini ya está DENTRO. Usar la fórmula equivocada
/// desplaza la frontera del prefijo y atribuye a la sección que no es.
fn total_prompt_tokens(shape: CacheShape, input: u64, cache_read: u64, cache_write: u64) -> u64 {
    match shape {
        CacheShape::Separate => input.saturating_add(cache_read).saturating_add(cache_write),
        CacheShape::Subset => input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Desglose de juguete con secciones de tamaño cómodo para razonar:
    /// tools 400, system 100, history 400, last_turn 80, other 20 = 1000.
    fn breakdown() -> ContextBreakdown {
        ContextBreakdown {
            system_bytes: 100,
            tools_bytes: 400,
            history_bytes: 400,
            last_turn_bytes: 80,
            other_bytes: 20,
            measured_bytes: 1000,
            messages_count: 4,
        }
    }

    /// Sin desglose de contexto no hay secciones que repartir: "no atribuible".
    #[test]
    fn sin_contexto_no_es_atribuible() {
        assert!(attribute_cache("anthropic", None, Some(100), Some(50), Some(0)).is_none());
    }

    /// `cache_read_tokens` a `None` significa que NO se midió la caché. No es
    /// lo mismo que medirla y que diera cero: aquí no se puede atribuir nada.
    #[test]
    fn sin_tokens_de_cache_no_es_atribuible() {
        let c = breakdown();
        assert!(attribute_cache("anthropic", Some(&c), Some(100), None, None).is_none());
    }

    /// Upstream desconocido: no sabemos si la caché se factura aparte o dentro,
    /// así que la frontera no se puede colocar. Antes que inventar, `None`.
    #[test]
    fn upstream_desconocido_no_es_atribuible() {
        let c = breakdown();
        assert!(attribute_cache("proveedor-que-no-existe", Some(&c), Some(100), Some(50), Some(0)).is_none());
    }

    /// `measured_bytes` a cero haría dividir por cero al convertir la fracción.
    #[test]
    fn sin_bytes_medidos_no_es_atribuible() {
        let c = ContextBreakdown::default();
        assert!(attribute_cache("anthropic", Some(&c), Some(100), Some(50), Some(0)).is_none());
    }

    /// Caché medida y vacía: el objeto EXISTE con todo a cero. Distinguirlo de
    /// `None` es justo lo que permite decir "no se cacheó nada" en vez de
    /// "no se sabe".
    #[test]
    fn cache_medida_a_cero_da_objeto_con_ceros() {
        let c = breakdown();
        let got = attribute_cache("anthropic", Some(&c), Some(1000), Some(0), Some(0))
            .expect("con cubos y caché medida a cero debe haber objeto");
        assert_eq!(got.method, METHOD);
        assert_eq!(got.tools_cached_bytes, 0);
        assert_eq!(got.system_cached_bytes, 0);
        assert_eq!(got.history_cached_bytes, 0);
        assert_eq!(got.last_turn_cached_bytes, 0);
        assert_eq!(got.other_cached_bytes, 0);
    }

    /// Anthropic factura la caché APARTE: total = input + read + write.
    /// Con input 400, read 500 y write 100, el prompt son 1000 tokens y la
    /// mitad (500) llegó de caché -> 500 de los 1000 bytes medidos.
    /// El paseo llena tools (400) y luego 100 de system.
    #[test]
    fn anthropic_suma_la_cache_al_input_y_pasea_en_orden() {
        let c = breakdown();
        let got = attribute_cache("anthropic", Some(&c), Some(400), Some(500), Some(100))
            .expect("atribuible");
        assert_eq!(got.tools_cached_bytes, 400, "tools va primero y cabe entero");
        assert_eq!(got.system_cached_bytes, 100, "system recibe el resto del prefijo");
        assert_eq!(got.history_cached_bytes, 0);
        assert_eq!(got.last_turn_cached_bytes, 0);
        assert_eq!(got.other_cached_bytes, 0);
    }

    /// OpenAI factura la caché DENTRO del input: total = input_tokens.
    /// Con input 1000 y read 500, la fracción es 0,5 -> mismos 500 bytes.
    /// Si se aplicara la fórmula de Anthropic el total saldría 1500 y la
    /// frontera se desplazaría a 333 bytes, atribuyendo de menos a `tools`.
    #[test]
    fn openai_cuenta_la_cache_dentro_del_input() {
        let c = breakdown();
        let got = attribute_cache("openai", Some(&c), Some(1000), Some(500), Some(0))
            .expect("atribuible");
        assert_eq!(got.tools_cached_bytes, 400);
        assert_eq!(got.system_cached_bytes, 100);
        assert_eq!(got.history_cached_bytes, 0);
    }

    /// Prefijo caliente típico: casi todo cacheado menos el turno nuevo.
    /// input 100, read 900 -> 90% de 1000 = 900 bytes: tools+system+history
    /// (900 exactos) llenos, y el último turno intacto. Es el caso que la
    /// medición encuentra en ~55% de las filas reales.
    #[test]
    fn prefijo_estable_completo_deja_el_ultimo_turno_sin_cachear() {
        let c = breakdown();
        let got = attribute_cache("anthropic", Some(&c), Some(100), Some(900), Some(0))
            .expect("atribuible");
        assert_eq!(got.tools_cached_bytes, 400);
        assert_eq!(got.system_cached_bytes, 100);
        assert_eq!(got.history_cached_bytes, 400);
        assert_eq!(got.last_turn_cached_bytes, 0, "el turno nuevo NO puede estar cacheado");
        assert_eq!(got.other_cached_bytes, 0);
    }

    /// Hit parcial: el prefijo cacheado se queda a medio historial porque el
    /// historial creció desde el turno anterior. Es el ~45% restante de las
    /// filas reales, y el motivo de que el residuo medido sea bimodal.
    #[test]
    fn hit_parcial_corta_por_la_mitad_del_historial() {
        let c = breakdown();
        // input 300, read 700 -> 70% de 1000 = 700 bytes.
        let got = attribute_cache("anthropic", Some(&c), Some(300), Some(700), Some(0))
            .expect("atribuible");
        assert_eq!(got.tools_cached_bytes, 400);
        assert_eq!(got.system_cached_bytes, 100);
        assert_eq!(got.history_cached_bytes, 200, "el historial se corta por la mitad");
        assert_eq!(got.last_turn_cached_bytes, 0);
    }

    /// Datos inconsistentes (`cache_read` mayor que el prompt entero) no pueden
    /// desbordar: se acota a todo cacheado y ninguna sección excede lo medido.
    #[test]
    fn cache_incoherente_se_acota_y_no_desborda() {
        let c = breakdown();
        let got = attribute_cache("openai", Some(&c), Some(10), Some(9_999), Some(0))
            .expect("atribuible");
        assert_eq!(got.tools_cached_bytes, 400);
        assert_eq!(got.system_cached_bytes, 100);
        assert_eq!(got.history_cached_bytes, 400);
        assert_eq!(got.last_turn_cached_bytes, 80);
        assert_eq!(got.other_cached_bytes, 20);
        let suma = got.tools_cached_bytes
            + got.system_cached_bytes
            + got.history_cached_bytes
            + got.last_turn_cached_bytes
            + got.other_cached_bytes;
        assert_eq!(suma, c.measured_bytes, "nunca más bytes de los medidos");
    }

    /// GUARDA DE CONTRATO de la forma INTERNA del objeto.
    ///
    /// El snapshot de `/requests` (`telemetry::recent`) recorre claves de forma
    /// recursiva, pero su fila de prueba lleva `cache_by_section: None`, así que
    /// las claves de dentro NO quedan cubiertas allí: renombrar
    /// `tools_cached_bytes` no rompería nada. Este test tapa ese hueco, porque
    /// lo que consume una lente es justamente el nombre de dentro.
    #[test]
    fn el_json_publicado_conserva_method_y_las_cinco_secciones() {
        let c = breakdown();
        let got = attribute_cache("anthropic", Some(&c), Some(100), Some(900), Some(0))
            .expect("atribuible");
        let json = serde_json::to_value(got).expect("serializa");

        assert_eq!(json["method"], METHOD, "el método va DENTRO del objeto");
        let claves: std::collections::BTreeSet<&str> =
            json.as_object().expect("objeto").keys().map(String::as_str).collect();
        assert_eq!(
            claves,
            [
                "history_cached_bytes",
                "last_turn_cached_bytes",
                "method",
                "other_cached_bytes",
                "system_cached_bytes",
                "tools_cached_bytes",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
            "cambió la forma de cache_by_section: si es ADITIVO actualiza esta lista; \
             si RENOMBRA o QUITA, sube CONTRACT_VERSION y cambia el sufijo de METHOD"
        );
        assert_eq!(json["history_cached_bytes"], 400);
        assert_eq!(json["last_turn_cached_bytes"], 0);
    }

    /// Invariante estructural: lo atribuido a una sección jamás supera lo
    /// medido en esa sección, sea cual sea la fracción cacheada.
    #[test]
    fn ninguna_seccion_recibe_mas_de_lo_que_mide() {
        let c = breakdown();
        for read in [0u64, 1, 50, 137, 400, 613, 999, 1000] {
            let got = attribute_cache("openai", Some(&c), Some(1000), Some(read), Some(0))
                .expect("atribuible");
            assert!(got.tools_cached_bytes <= c.tools_bytes);
            assert!(got.system_cached_bytes <= c.system_bytes);
            assert!(got.history_cached_bytes <= c.history_bytes);
            assert!(got.last_turn_cached_bytes <= c.last_turn_bytes);
            assert!(got.other_cached_bytes <= c.other_bytes);
        }
    }
}
