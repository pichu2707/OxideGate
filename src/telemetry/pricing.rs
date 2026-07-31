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
    //
    // OJO: el descuento de caché NO es uniforme dentro de OpenAI. La familia 4o
    // cobra la lectura al 50% del input y la familia 5 al 10%. Por eso hay dos
    // multiplicadores y no uno — ver las constantes para la verificación.
    let openai_4o_cache = CacheAccounting::Subset {
        read_multiplier: OPENAI_4O_CACHE_READ_MULTIPLIER,
    };
    let openai_5_cache = CacheAccounting::Subset {
        read_multiplier: OPENAI_5_CACHE_READ_MULTIPLIER,
    };

    // Familia 5. El orden importa: `gpt-5.6-sol` y `gpt-5.5` antes que `gpt-5`,
    // porque el emparejamiento es por subcadena y `gpt-5` los tragaría a los dos
    // con un precio cuatro veces menor.
    if m.contains("gpt-5.6-sol") || m.contains("gpt-5.5") {
        return Some(ModelPricing { price_in: 5.0, price_out: 30.0, cache: openai_5_cache });
    }
    if m.contains("gpt-5") {
        return Some(ModelPricing { price_in: 1.25, price_out: 10.0, cache: openai_5_cache });
    }

    if m.contains("gpt-4o-mini") {
        return Some(ModelPricing { price_in: 0.15, price_out: 0.60, cache: openai_4o_cache });
    }
    if m.contains("gpt-4o") {
        return Some(ModelPricing { price_in: 2.50, price_out: 10.0, cache: openai_4o_cache });
    }
    if m.contains("gpt-4-turbo") {
        return Some(ModelPricing { price_in: 10.0, price_out: 30.0, cache: openai_4o_cache });
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

/// Multiplicador de la familia **4o** de OpenAI para la porción de input
/// servida desde caché, relativo al precio de input. DEFAULT editable.
///
/// Verificado contra la tarifa pública (2026-07-31): `gpt-4o` cobra $2,50 el
/// input y $1,25 la lectura de caché; `gpt-4o-mini`, $0,15 y $0,075. Las dos
/// dan exactamente 0,5.
const OPENAI_4O_CACHE_READ_MULTIPLIER: f64 = 0.5;

/// Multiplicador de la familia **5** de OpenAI. DEFAULT editable.
///
/// **No es el mismo que el de la familia 4o, y esa es la razón de que existan
/// dos constantes.** Verificado contra la tarifa pública (2026-07-31):
/// `gpt-5.5` y `gpt-5.6-sol` cobran $5,00 el input y $0,50 la lectura de caché;
/// `gpt-5`, $1,25 y $0,125. Las tres dan 0,1 — no 0,5.
///
/// Usar aquí el multiplicador de 4o inflaría el coste de la porción cacheada
/// por cinco, y en el tráfico medido de este proyecto más de la mitad del
/// volumen llega cacheado: el error no sería marginal.
const OPENAI_5_CACHE_READ_MULTIPLIER: f64 = 0.1;

/// FORMA de la contabilidad de caché, sin los multiplicadores.
///
/// Separar la forma del precio no es cosmético: la forma («¿los tokens de caché
/// van aparte del input o ya están dentro?») es estable dentro de una familia de
/// proveedor, pero **los multiplicadores NO**. Medido contra la tarifa pública:
/// dentro de OpenAI, la familia 4o cobra la lectura de caché al 0,5 y la familia
/// 5 al 0,1. Una función que devolviera «la contabilidad de OpenAI» tendría que
/// elegir uno de los dos y acertaría solo la mitad de las veces.
///
/// Quien necesita solo la forma —[`telemetry::cache_attribution`]— se lleva esto
/// y no puede leer por accidente un multiplicador que no le corresponde. Quien
/// necesita facturar usa [`model_pricing`], que trae precio y multiplicadores
/// juntos y por modelo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheShape {
    /// Los tokens de caché van APARTE del input reportado (Anthropic): para
    /// tener el prompt completo hay que sumarlos.
    Separate,
    /// Los tokens de caché ya están DENTRO del input reportado (OpenAI, Gemini).
    Subset,
}

impl CacheAccounting {
    /// La forma de esta contabilidad, descartando los multiplicadores.
    ///
    /// **Solo existe para la guarda de divergencia** (ver el test
    /// `la_forma_por_upstream_coincide_con_la_de_la_tabla_de_precios`): en
    /// producción nadie necesita degradar una contabilidad completa a su forma,
    /// porque quien factura ya tiene los multiplicadores y quien atribuye pide
    /// la forma directamente a [`cache_shape_for_upstream`]. Va bajo
    /// `cfg(test)` para que eso quede dicho y no como código muerto silencioso.
    #[cfg(test)]
    pub fn shape(self) -> CacheShape {
        match self {
            CacheAccounting::Separate { .. } => CacheShape::Separate,
            CacheAccounting::Subset { .. } => CacheShape::Subset,
        }
    }
}

/// Forma de la contabilidad de caché de una FAMILIA de proveedor, sin pasar por
/// el precio.
///
/// Existe porque hay una pregunta —«¿los tokens de caché van aparte del input o
/// dentro?»— que NO depende de la tarifa del modelo, sino de qué proveedor lo
/// sirve. Atarla a [`model_pricing`] dejaría sin atribuir a cualquier modelo que
/// todavía no esté en la tabla de precios.
///
/// **Invariante**: para un modelo que SÍ esté en [`model_pricing`], su `arm`
/// debe declarar esta misma FORMA. Los multiplicadores quedan deliberadamente
/// fuera del contrato porque divergen dentro de una familia. Un test lo
/// comprueba para los modelos conocidos.
///
/// `None` para un upstream que no reconocemos: preferimos no atribuir a
/// atribuir con la fórmula equivocada, que desplazaría la frontera del prefijo.
pub fn cache_shape_for_upstream(upstream: &str) -> Option<CacheShape> {
    match upstream {
        "anthropic" => Some(CacheShape::Separate),
        // `codex` es la ruta de Codex/Responses de OpenAI: misma forma.
        "openai" | "codex" | "gemini" => Some(CacheShape::Subset),
        _ => None,
    }
}

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

    /// GUARDA DE DIVERGENCIA.
    ///
    /// [`cache_shape_for_upstream`] y [`model_pricing`] responden a la misma
    /// pregunta desde dos claves distintas (familia de proveedor vs modelo
    /// concreto). Mientras existan las dos, pueden separarse en silencio. Este
    /// test cierra ese hueco: para cada modelo que SÍ está en la tabla de
    /// precios, ambas rutas tienen que declarar la misma FORMA.
    ///
    /// **Compara formas, NO multiplicadores, y eso es deliberado.** Los
    /// multiplicadores divergen legítimamente dentro de una familia (4o cobra
    /// la lectura de caché al 0,5 y la familia 5 al 0,1, verificado contra la
    /// tarifa pública). Exigir que coincidieran obligaría a mentir en uno de
    /// los dos sitios. La forma sí es estable, y es lo único que consume la
    /// atribución.
    #[test]
    fn la_forma_por_upstream_coincide_con_la_de_la_tabla_de_precios() {
        // (modelo tal como llega en el body, upstream que lo sirve)
        let casos = [
            ("claude-opus-4-8", "anthropic"),
            ("claude-haiku-4-5", "anthropic"),
            ("claude-sonnet-4-5-20250929", "anthropic"),
            ("gpt-5.6-sol", "codex"),
            ("gpt-5.5", "openai"),
            ("gpt-5", "openai"),
            ("gpt-4o", "openai"),
            ("gpt-4o-mini", "openai"),
            ("gpt-4-turbo", "openai"),
            ("gemini-2.5-flash", "gemini"),
            ("gemini-2.5-pro", "gemini"),
        ];

        for (modelo, upstream) in casos {
            let por_modelo = model_pricing(modelo)
                .unwrap_or_else(|| panic!("{modelo} debería estar en la tabla de precios"))
                .cache
                .shape();
            let por_upstream = cache_shape_for_upstream(upstream)
                .unwrap_or_else(|| panic!("{upstream} debería tener forma de familia"));
            assert_eq!(
                por_modelo, por_upstream,
                "divergencia de FORMA para {modelo} vía {upstream}"
            );
        }
    }

    /// La ruta de Codex es OpenAI por debajo: misma forma, o la atribución de
    /// caché colocaría la frontera del prefijo con la fórmula equivocada en el
    /// 74% del tráfico medido.
    #[test]
    fn codex_tiene_la_misma_forma_que_openai() {
        assert_eq!(
            cache_shape_for_upstream("codex").expect("codex reconocido"),
            cache_shape_for_upstream("openai").expect("openai reconocido"),
        );
    }

    /// Un upstream que no conocemos no puede caer en una forma por defecto:
    /// elegir mal desplaza la frontera del prefijo. `None` explícito.
    #[test]
    fn upstream_desconocido_no_tiene_forma() {
        assert!(cache_shape_for_upstream("proveedor-nuevo").is_none());
        assert!(cache_shape_for_upstream("").is_none());
    }

    /// LA FAMILIA 5 NO COBRA LA CACHE COMO LA 4o.
    ///
    /// Verificado contra la tarifa publica (2026-07-31). Este test existe para
    /// que nadie "simplifique" las dos constantes en una: hacerlo inflaria por
    /// cinco el coste de la porcion cacheada de gpt-5.5/gpt-5.6-sol, que son el
    /// 86% del trafico medido y donde mas de la mitad del volumen va cacheado.
    #[test]
    fn la_familia_5_y_la_4o_tienen_multiplicadores_de_cache_distintos() {
        let mult = |modelo: &str| match model_pricing(modelo).unwrap().cache {
            CacheAccounting::Subset { read_multiplier } => read_multiplier,
            CacheAccounting::Separate { .. } => panic!("{modelo} deberia ser Subset"),
        };
        for m in ["gpt-5.5", "gpt-5.6-sol", "gpt-5"] {
            assert!((mult(m) - 0.1).abs() < EPS, "{m}: la familia 5 lee cache al 0,1");
        }
        for m in ["gpt-4o", "gpt-4o-mini"] {
            assert!((mult(m) - 0.5).abs() < EPS, "{m}: la familia 4o lee cache al 0,5");
        }
    }

    /// `gpt-5` NO puede tragarse a `gpt-5.5` ni a `gpt-5.6-sol` por subcadena:
    /// su input cuesta cuatro veces menos y el emparejamiento es por `contains`.
    #[test]
    fn los_modelos_de_la_familia_5_no_se_solapan_por_subcadena() {
        assert_eq!(model_pricing("gpt-5.5").unwrap().price_in, 5.0);
        assert_eq!(model_pricing("gpt-5.6-sol").unwrap().price_in, 5.0);
        assert_eq!(model_pricing("gpt-5").unwrap().price_in, 1.25);
    }
}
