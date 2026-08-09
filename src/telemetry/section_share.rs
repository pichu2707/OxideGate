//! Reparto ESTIMADO del input pagado entre las secciones del contexto.
//!
//! # Qué contesta, y por qué no lo puede contestar el consumidor
//!
//! `context_*_bytes` dice cuánto PESA cada sección. No dice cuánto CUESTA, y
//! medido son cosas muy distintas: en `claude-opus-4-8`, `tools` es el 56,1% de
//! los bytes y el 22,5% de lo que se paga, mientras el turno nuevo pasa del
//! 7,8% al 31,1%. Medir en bytes subestima ~4x lo que cuesta la pregunta real
//! del usuario.
//!
//! La diferencia la hace la caché: el prefijo estable se lee a tarifa reducida
//! y el turno nuevo se paga entero. Para repartir hacen falta tres cosas —los
//! bytes por sección, los bytes cacheados por sección y el **multiplicador de
//! lectura de caché del modelo**— y **la tercera no se publica**. Un consumidor
//! con `/requests` delante no puede calcular esto por su cuenta, y publicar la
//! tabla de precios entera para que pudiera sería peor.
//!
//! # Esto es una ESTIMACIÓN sobre otra ESTIMACIÓN
//!
//! Se apoya en [`CacheBySection`], que ya es estimado (§4.11). Por eso hereda
//! su misma disciplina: objeto anidado, `method` versionado dentro, y `None`
//! honesto en cuanto falta cualquier pieza.
//!
//! # El campo NO se llama `cost`, y es deliberado
//!
//! El issue #50 lo marca como el único error irreversible del contrato:
//! *«en cuanto una lente lo pinte en euros, nadie vuelve a leer la letra
//! chica»*. Son **fracciones de 0 a 1** que suman 1: no llevan moneda, no se
//! pueden pintar como euros sin multiplicarlas por algo, y ese algo obliga a
//! ir a buscar `cost_estimate_usd` y a leer qué es.
//!
//! # Qué NO incluye
//!
//! Solo el **input**. El output no se reparte porque no pertenece a ninguna
//! sección del contexto: es lo que el modelo generó, no lo que se le mandó.
//! Un reparto que lo incluyera estaría atribuyendo a `tools` una parte de algo
//! que `tools` no causó.

use serde::Serialize;

use crate::provider::ContextBreakdown;
use crate::telemetry::cache_attribution::CacheBySection;
use crate::telemetry::pricing::{CacheAccounting, model_pricing};

/// Identificador del algoritmo, publicado dentro del propio objeto.
///
/// Mismo criterio que `cache_attribution::METHOD`: permite cambiar el reparto
/// sin romper a un consumidor, que puede decidir si entiende esta versión antes
/// de pintar nada.
pub const METHOD: &str = "cache_weighted_v1";

/// Fracción del input PAGADO que corresponde a cada sección, ESTIMADA.
///
/// Las cinco fracciones suman 1,0 (salvo error de coma flotante). Son
/// proporciones, nunca dinero: para convertirlas en euros hay que multiplicar
/// por `cost_estimate_usd`, y quien lo haga tiene que pasar por leer qué es ese
/// campo.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SectionShare {
    /// Algoritmo con el que se calculó. Ver [`METHOD`].
    pub method: &'static str,
    pub tools_share: f64,
    pub system_share: f64,
    pub history_share: f64,
    /// Fracción del turno NUEVO. Es la que la medición desmiente cuando se mira
    /// en bytes: pequeña en peso y grande en factura.
    pub last_turn_share: f64,
    pub other_share: f64,
}

/// Reparte el input pagado entre las cinco secciones.
///
/// Devuelve `None` —«no repartible»— si falta cualquier pieza:
///
/// - sin desglose de contexto no hay secciones,
/// - sin [`CacheBySection`] no se sabe qué se cacheó, y repartir ignorando la
///   caché daría el reparto por bytes, que es justo el que la medición
///   desmiente,
/// - sin modelo en la tabla de precios no se conoce el multiplicador de
///   lectura, y **cada familia usa el suyo** (0,5 en la familia 4o de OpenAI,
///   0,1 en la 5): elegir uno por defecto sería inventar el resultado,
/// - con `measured_bytes` a cero no hay nada que repartir.
///
/// Función PURA y total: se llama al emitir la métrica, con la respuesta ya
/// cerrada, y no puede entrar en pánico.
pub fn attribute_share(
    model: Option<&str>,
    context: Option<&ContextBreakdown>,
    cache: Option<&CacheBySection>,
) -> Option<SectionShare> {
    let c = context?;
    let cached = cache?;
    let pricing = model_pricing(model?)?;
    if c.measured_bytes == 0 {
        return None;
    }

    // El multiplicador de LECTURA es el único que interviene: el reparto
    // pondera bytes ya presentes en el prompt, y la escritura de caché no es
    // una sección — es un cargo aparte sobre el mismo contenido.
    let m = match pricing.cache {
        CacheAccounting::Separate {
            read_multiplier, ..
        } => read_multiplier,
        CacheAccounting::Subset { read_multiplier } => read_multiplier,
    };

    // Peso pagado de una sección: lo no cacheado a tarifa plena más lo cacheado
    // al multiplicador. La TARIFA del modelo se cancela al dividir por el
    // total, así que no hace falta y el reparto no depende del precio absoluto
    // — solo de la proporción entre cacheado y no cacheado.
    let peso = |bytes: usize, cach: usize| {
        let cach = cach.min(bytes) as f64;
        (bytes as f64 - cach) + cach * m
    };

    let tools = peso(c.tools_bytes, cached.tools_cached_bytes);
    let system = peso(c.system_bytes, cached.system_cached_bytes);
    let history = peso(c.history_bytes, cached.history_cached_bytes);
    let last_turn = peso(c.last_turn_bytes, cached.last_turn_cached_bytes);
    let other = peso(c.other_bytes, cached.other_cached_bytes);

    let total = tools + system + history + last_turn + other;
    if total <= 0.0 {
        // Todo cacheado con multiplicador cero, o todas las secciones vacías.
        // No hay reparto que hacer y cinco ceros se leerían como «no costó
        // nada», que es una afirmación distinta de «no se puede repartir».
        return None;
    }

    Some(SectionShare {
        method: METHOD,
        tools_share: tools / total,
        system_share: system / total,
        history_share: history / total,
        last_turn_share: last_turn / total,
        other_share: other / total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::cache_attribution::METHOD as CACHE_METHOD;

    fn contexto() -> ContextBreakdown {
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

    fn cache(tools: usize, system: usize, history: usize, last: usize) -> CacheBySection {
        CacheBySection {
            method: CACHE_METHOD,
            tools_cached_bytes: tools,
            system_cached_bytes: system,
            history_cached_bytes: history,
            last_turn_cached_bytes: last,
            other_cached_bytes: 0,
        }
    }

    /// Sin nada cacheado, el reparto es el de BYTES: es el caso degenerado y
    /// sirve de control de que la fórmula no introduce sesgo por sí sola.
    #[test]
    fn sin_cache_el_reparto_coincide_con_el_de_bytes() {
        let s = attribute_share(
            Some("claude-opus-4-8"),
            Some(&contexto()),
            Some(&cache(0, 0, 0, 0)),
        )
        .expect("repartible");

        assert!((s.tools_share - 0.400).abs() < 1e-9);
        assert!((s.history_share - 0.400).abs() < 1e-9);
        assert!((s.last_turn_share - 0.080).abs() < 1e-9);
    }

    /// EL CASO QUE JUSTIFICA EL CAMPO. Con el prefijo estable cacheado, `tools`
    /// pesa mucho menos de lo que sugieren sus bytes y el turno nuevo mucho
    /// más. Es el efecto medido en tráfico real, reproducido aquí.
    #[test]
    fn con_el_prefijo_cacheado_tools_pesa_menos_y_el_turno_nuevo_mucho_mas() {
        let s = attribute_share(
            Some("claude-opus-4-8"),
            Some(&contexto()),
            // tools+system+history enteros dentro del prefijo; el turno nuevo no
            Some(&cache(400, 100, 400, 0)),
        )
        .expect("repartible");

        assert!(
            s.tools_share < 0.40,
            "tools deberia bajar de su 40% en bytes: {}",
            s.tools_share
        );
        assert!(
            s.last_turn_share > 0.08,
            "el turno nuevo deberia subir de su 8% en bytes: {}",
            s.last_turn_share
        );
        // Con multiplicador 0,1: pesos 40, 10, 40, 80, 20 -> total 190
        assert!((s.last_turn_share - 80.0 / 190.0).abs() < 1e-9);
    }

    /// Las cinco fracciones suman 1. Si no sumaran, no serían un reparto.
    #[test]
    fn las_cinco_fracciones_suman_uno() {
        for c in [
            cache(0, 0, 0, 0),
            cache(400, 100, 200, 0),
            cache(400, 100, 400, 80),
        ] {
            let s = attribute_share(Some("claude-opus-4-8"), Some(&contexto()), Some(&c))
                .expect("repartible");
            let suma = s.tools_share
                + s.system_share
                + s.history_share
                + s.last_turn_share
                + s.other_share;
            assert!((suma - 1.0).abs() < 1e-9, "suman {suma}");
        }
    }

    /// El multiplicador ES del modelo, no del proveedor: la familia 4o de
    /// OpenAI lee caché al 0,5 y la 5 al 0,1, así que el MISMO reparto de bytes
    /// y de caché da resultados distintos. Elegir uno por defecto sería
    /// inventar el número.
    #[test]
    fn el_reparto_depende_del_multiplicador_del_modelo() {
        let ctx = contexto();
        let cch = cache(400, 100, 400, 0);

        let cuatro_o = attribute_share(Some("gpt-4o"), Some(&ctx), Some(&cch)).expect("4o");
        let cinco = attribute_share(Some("gpt-5.5"), Some(&ctx), Some(&cch)).expect("5");

        assert!(
            cinco.last_turn_share > cuatro_o.last_turn_share,
            "con caché más barata (0,1 vs 0,5) el turno nuevo pesa más: {} vs {}",
            cinco.last_turn_share,
            cuatro_o.last_turn_share
        );
    }

    /// Sin atribución de caché NO se reparte por bytes como consuelo: ese es
    /// justo el reparto que la medición desmiente. Hueco honesto.
    #[test]
    fn sin_atribucion_de_cache_no_hay_reparto() {
        assert!(attribute_share(Some("claude-opus-4-8"), Some(&contexto()), None).is_none());
    }

    /// Un modelo sin tarifa no tiene multiplicador conocido, y cada familia usa
    /// el suyo. Antes que elegir uno, `None`.
    #[test]
    fn un_modelo_sin_tarifa_no_es_repartible() {
        assert!(
            attribute_share(
                Some("modelo-que-no-existe"),
                Some(&contexto()),
                Some(&cache(0, 0, 0, 0))
            )
            .is_none()
        );
    }

    /// GUARDA DE CONTRATO de la forma publicada, por la misma razón que la de
    /// `cache_by_section`: el snapshot de `/requests` solo congela el primer
    /// nivel y no mira dentro de los objetos anidados.
    #[test]
    fn el_json_publicado_conserva_method_y_las_cinco_fracciones() {
        let s = attribute_share(
            Some("claude-opus-4-8"),
            Some(&contexto()),
            Some(&cache(400, 100, 400, 0)),
        )
        .expect("repartible");
        let json = serde_json::to_value(s).expect("serializa");

        assert_eq!(json["method"], METHOD);
        let claves: std::collections::BTreeSet<&str> = json
            .as_object()
            .expect("objeto")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            claves,
            [
                "history_share",
                "last_turn_share",
                "method",
                "other_share",
                "system_share",
                "tools_share",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
            "cambió la forma de input_share_by_section: si es ADITIVO actualiza \
             esta lista; si RENOMBRA o QUITA, sube CONTRACT_VERSION y cambia el \
             sufijo de METHOD"
        );
    }

    /// NINGUNA clave lleva la palabra `cost`. Es el único error que el issue
    /// #50 marca como irreversible: en cuanto una lente lo pinte en euros,
    /// nadie vuelve a leer la letra chica.
    #[test]
    fn ninguna_clave_publicada_se_llama_cost() {
        let s = attribute_share(
            Some("claude-opus-4-8"),
            Some(&contexto()),
            Some(&cache(0, 0, 0, 0)),
        )
        .expect("repartible");
        let json = serde_json::to_value(s).expect("serializa");

        for k in json.as_object().expect("objeto").keys() {
            assert!(
                !k.to_lowercase().contains("cost") && !k.to_lowercase().contains("usd"),
                "la clave `{k}` sugiere dinero: son fracciones, no euros"
            );
        }
    }
}
