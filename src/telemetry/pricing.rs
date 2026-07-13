//! Tabla de precios por modelo y cálculo de coste estimado.
//!
//! Convierte tokens (dato exacto que sacamos del `usage` del proveedor) en un
//! coste en USD. Los precios son valores POR DEFECTO editables: hay que
//! mantenerlos sincronizados con la tarifa pública de cada proveedor. Si un
//! modelo no está en la tabla devolvemos `None` — preferimos "coste desconocido"
//! antes que un número inventado que ensucie la telemetría.

/// Cómo una familia de modelos contabiliza los tokens de caché en su factura.
///
/// Vive JUNTO al precio (dentro de [`ModelPricing`]) a propósito: así es
/// imposible que un modelo tenga precio pero caiga en la fórmula de caché
/// equivocada. Antes la semántica se decidía en `estimate_cost_usd` con un
/// `if/else` separado de la tabla de precios, y una familia nueva podía
/// facturar mal en silencio si no se actualizaban ambos sitios. Ahora el
/// compilador obliga a declarar la contabilidad en el mismo lugar que el precio.
#[derive(Debug, Clone, Copy)]
pub enum CacheAccounting {
    /// Los tokens de caché van APARTE del input medido (Anthropic): se suman
    /// al input a sus multiplicadores, sin restar nada.
    Separate {
        read_multiplier: f64,
        write_multiplier: f64,
    },
    /// `cache_read` es SUBCONJUNTO del input (OpenAI, Gemini): la porción no
    /// cacheada se factura a tarifa plena y la cacheada al multiplicador dado.
    /// La Responses API de OpenAI SÍ reporta `cache_write_tokens`
    /// (`input_tokens_details.cache_write_tokens`), pero este arm lo ignora a
    /// propósito: no lo factura aparte (en la práctica llega en `0`). Si algún
    /// día OpenAI cobra la escritura de caché, se cablea aquí.
    Subset { read_multiplier: f64 },
}

impl CacheAccounting {
    /// Coste de input (aún sin dividir por 1M) según la contabilidad de caché.
    ///
    /// La `Separate` suma la caché al input; la `Subset` la descuenta del input
    /// y la recobra al multiplicador reducido, con clamp a cero ante datos
    /// inconsistentes (`cache_read > input`) para no dar un coste negativo.
    fn input_cost_per_mtok(self, input: f64, cache_read: f64, cache_write: f64, price_in: f64) -> f64 {
        match self {
            CacheAccounting::Separate {
                read_multiplier,
                write_multiplier,
            } => (input + cache_read * read_multiplier + cache_write * write_multiplier) * price_in,
            CacheAccounting::Subset { read_multiplier } => {
                let billable_full_rate = (input - cache_read).max(0.0);
                (billable_full_rate + cache_read * read_multiplier) * price_in
            }
        }
    }
}

/// Precio y semántica de caché de un modelo, en una sola fuente de verdad.
///
/// Que precio y contabilidad de caché viajen juntos es la garantía estructural:
/// agregar un modelo obliga a declarar ambos en el mismo `arm` de
/// [`model_pricing`], sin posibilidad de que diverjan.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    /// Precio de input, USD por millón de tokens.
    pub price_in: f64,
    /// Precio de output, USD por millón de tokens.
    pub price_out: f64,
    /// Cómo se contabiliza la caché de este modelo.
    pub cache: CacheAccounting,
}

/// Precio y contabilidad de caché de un modelo dado, o `None` si no lo
/// reconocemos.
///
/// El emparejamiento es por subcadena (familia de modelo) para tolerar sufijos
/// de versión y fecha (`claude-sonnet-4-5-20250929`, `gpt-4o-2024-08-06`, …).
/// Cada `arm` declara precio Y semántica de caché: los defaults de
/// multiplicador son editables (ver constantes) y hay que mantenerlos
/// sincronizados con la tarifa pública.
pub fn model_pricing(model: &str) -> Option<ModelPricing> {
    let m = model.to_ascii_lowercase();

    // Anthropic (Claude): la caché va APARTE del input medido.
    // Orden importa: comprobamos lo más específico primero.
    if m.contains("claude") {
        let cache = CacheAccounting::Separate {
            read_multiplier: ANTHROPIC_CACHE_READ_MULTIPLIER,
            write_multiplier: ANTHROPIC_CACHE_WRITE_MULTIPLIER,
        };
        if m.contains("opus") {
            return Some(ModelPricing { price_in: 15.0, price_out: 75.0, cache });
        }
        if m.contains("haiku") {
            return Some(ModelPricing { price_in: 0.80, price_out: 4.0, cache });
        }
        if m.contains("sonnet") {
            return Some(ModelPricing { price_in: 3.0, price_out: 15.0, cache });
        }
    }

    // OpenAI (GPT / o-series): `cache_read` es subconjunto del input.
    let openai_cache = CacheAccounting::Subset {
        read_multiplier: OPENAI_CACHE_READ_MULTIPLIER,
    };
    if m.contains("gpt-4o-mini") {
        return Some(ModelPricing { price_in: 0.15, price_out: 0.60, cache: openai_cache });
    }
    if m.contains("gpt-4o") {
        return Some(ModelPricing { price_in: 2.50, price_out: 10.0, cache: openai_cache });
    }
    if m.contains("gpt-4-turbo") {
        return Some(ModelPricing { price_in: 10.0, price_out: 30.0, cache: openai_cache });
    }

    // Google (Gemini): `cachedContentTokenCount` es subconjunto del input. El
    // output que facturamos es `candidatesTokenCount`; los tokens de "thinking"
    // (`thoughtsTokenCount`) aún no se itemizan.
    if m.contains("gemini") {
        let cache = CacheAccounting::Subset {
            read_multiplier: GEMINI_CACHE_READ_MULTIPLIER,
        };
        if m.contains("2.5-pro") {
            return Some(ModelPricing { price_in: 1.25, price_out: 10.0, cache });
        }
        if m.contains("2.5-flash") {
            return Some(ModelPricing { price_in: 0.30, price_out: 2.50, cache });
        }
        if m.contains("1.5-pro") || m.contains("pro") {
            return Some(ModelPricing { price_in: 1.25, price_out: 5.0, cache });
        }
        // Familia flash (2.0-flash y genéricos): la opción barata por defecto.
        if m.contains("flash") {
            return Some(ModelPricing { price_in: 0.10, price_out: 0.40, cache });
        }
    }

    None
}

/// Multiplicador de Anthropic para tokens leídos desde caché, relativo al
/// precio de input publicado del modelo (lectura de caché: la porción más
/// barata). DEFAULT editable — mantener sincronizado con la tarifa pública.
const ANTHROPIC_CACHE_READ_MULTIPLIER: f64 = 0.1;

/// Multiplicador de Anthropic para tokens escritos a caché (creación, ventana
/// de 5 minutos), relativo al precio de input. DEFAULT editable.
const ANTHROPIC_CACHE_WRITE_MULTIPLIER: f64 = 1.25;

/// Multiplicador de Gemini para la porción de input servida desde caché,
/// relativo al precio de input. DEFAULT editable.
const GEMINI_CACHE_READ_MULTIPLIER: f64 = 0.25;

/// Multiplicador de OpenAI para la porción de input servida desde caché,
/// relativo al precio de input. DEFAULT editable.
const OPENAI_CACHE_READ_MULTIPLIER: f64 = 0.5;

/// Estima el coste en USD de un request a partir de los tokens medidos.
///
/// Requiere modelo conocido y al menos algún token de entrada/salida medido;
/// los que falten cuentan como cero. Devuelve `None` cuando no podemos
/// calcular con honestidad (modelo desconocido o sin datos de tokens).
///
/// Cache-aware: cada familia contabiliza la caché distinto, y este es el ÚNICO
/// lugar que lo sabe (los providers solo extraen tokens crudos, ver
/// `provider::Usage`). La semántica no se decide aquí sino que viene de
/// [`model_pricing`] junto al precio, vía [`CacheAccounting`], para que precio
/// y contabilidad no puedan divergir.
///
/// Retrocompatibilidad: con `cache_read_tokens`/`cache_write_tokens` en
/// `None` (tratados como cero), el resultado es IDÉNTICO al cálculo sin
/// caché.
pub fn estimate_cost_usd(
    model: Option<&str>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
) -> Option<f64> {
    let model = model?;
    let pricing = model_pricing(model)?;

    // Sin ningún token medido no hay nada que valorar.
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }

    let input = input_tokens.unwrap_or(0) as f64;
    let output = output_tokens.unwrap_or(0) as f64;
    let cache_read = cache_read_tokens.unwrap_or(0) as f64;
    let cache_write = cache_write_tokens.unwrap_or(0) as f64;

    let input_cost_per_mtok =
        pricing
            .cache
            .input_cost_per_mtok(input, cache_read, cache_write, pricing.price_in);

    Some(input_cost_per_mtok / 1_000_000.0 + output / 1_000_000.0 * pricing.price_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerancia para comparar `f64` de coste (evita falsos negativos por
    /// redondeo de punto flotante).
    const EPS: f64 = 1e-9;

    /// Anthropic contabiliza la caché APARTE del input: `cache_read` a 0.1x
    /// y `cache_write` a 1.25x el precio de input, sumados al input crudo.
    #[test]
    fn anthropic_cache_cost_is_additive() {
        // claude-sonnet: price_in = 3.0, price_out = 15.0 USD/MTok.
        let cost = estimate_cost_usd(
            Some("claude-sonnet-4-5"),
            Some(1000),
            Some(500),
            Some(2000),
            Some(300),
        )
        .unwrap();

        // (1000 + 2000*0.1 + 300*1.25) * 3.0/1e6 + 500 * 15.0/1e6
        let expected = (1000.0 + 2000.0 * 0.1 + 300.0 * 1.25) * 3.0 / 1_000_000.0
            + 500.0 * 15.0 / 1_000_000.0;
        assert!((cost - expected).abs() < EPS, "cost={cost} expected={expected}");
    }

    /// Gemini contabiliza `cache_read` como SUBCONJUNTO del input: la
    /// porción no cacheada va a tarifa plena, la cacheada a 0.25x. No debe
    /// doble-contar los tokens cacheados.
    #[test]
    fn gemini_cache_cost_is_subset_of_input() {
        // gemini-2.5-flash: price_in = 0.30, price_out = 2.50 USD/MTok.
        let cost =
            estimate_cost_usd(Some("gemini-2.5-flash"), Some(1000), Some(200), Some(400), None)
                .unwrap();

        // (1000 - 400 + 400*0.25) * 0.30/1e6 + 200 * 2.50/1e6
        let expected = (1000.0 - 400.0 + 400.0 * 0.25) * 0.30 / 1_000_000.0
            + 200.0 * 2.50 / 1_000_000.0;
        assert!((cost - expected).abs() < EPS, "cost={cost} expected={expected}");
    }

    /// OpenAI contabiliza `cache_read` como SUBCONJUNTO del input, igual que
    /// Gemini pero con multiplicador 0.5x.
    #[test]
    fn openai_cache_cost_is_subset_of_input() {
        // gpt-4o: price_in = 2.50, price_out = 10.0 USD/MTok.
        let cost = estimate_cost_usd(Some("gpt-4o"), Some(1000), Some(200), Some(400), None)
            .unwrap();

        // (1000 - 400 + 400*0.5) * 2.50/1e6 + 200 * 10.0/1e6
        let expected = (1000.0 - 400.0 + 400.0 * 0.5) * 2.50 / 1_000_000.0
            + 200.0 * 10.0 / 1_000_000.0;
        assert!((cost - expected).abs() < EPS, "cost={cost} expected={expected}");
    }

    /// Retrocompatibilidad: sin datos de caché (`None`), el resultado debe
    /// ser IDÉNTICO al cálculo previo a esta migración.
    #[test]
    fn no_cache_tokens_matches_pre_cache_calculation() {
        let cost = estimate_cost_usd(Some("gpt-4o"), Some(1000), Some(500), None, None).unwrap();

        let expected = 1000.0 / 1_000_000.0 * 2.50 + 500.0 / 1_000_000.0 * 10.0;
        assert!((cost - expected).abs() < EPS, "cost={cost} expected={expected}");
    }

    /// Garantía estructural del endurecimiento: cada familia declara su
    /// contabilidad de caché JUNTO al precio, así que precio y semántica no
    /// pueden divergir. Anthropic es `Separate`; OpenAI y Gemini, `Subset`.
    #[test]
    fn cache_accounting_matches_family() {
        assert!(matches!(
            model_pricing("claude-opus-4-5").unwrap().cache,
            CacheAccounting::Separate { .. }
        ));
        assert!(matches!(
            model_pricing("gpt-4o").unwrap().cache,
            CacheAccounting::Subset { .. }
        ));
        assert!(matches!(
            model_pricing("gemini-2.5-flash").unwrap().cache,
            CacheAccounting::Subset { .. }
        ));
        assert!(model_pricing("modelo-desconocido").is_none());
    }

    /// Datos inconsistentes (`cache_read` > `input`) no deben producir un
    /// coste negativo: la porción a tarifa plena se clampa a cero.
    #[test]
    fn subset_cache_clamps_underflow_to_zero() {
        // cache_read (2000) > input (1000): la resta subyacente sería
        // negativa; el clamp debe evitarlo.
        let cost = estimate_cost_usd(Some("gpt-4o"), Some(1000), Some(0), Some(2000), None)
            .unwrap();

        // billable_full_rate se clampa a 0.0: cost_in = 2000*0.5*2.50/1e6.
        let expected = 2000.0 * 0.5 * 2.50 / 1_000_000.0;
        assert!((cost - expected).abs() < EPS, "cost={cost} expected={expected}");
        assert!(cost >= 0.0);
    }
}
