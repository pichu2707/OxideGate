//! Tabla de precios por modelo y cálculo de coste estimado.
//!
//! Convierte tokens (dato exacto que sacamos del `usage` del proveedor) en un
//! coste en USD. Los precios son valores POR DEFECTO editables: hay que
//! mantenerlos sincronizados con la tarifa pública de cada proveedor. Si un
//! modelo no está en la tabla devolvemos `None` — preferimos "coste desconocido"
//! antes que un número inventado que ensucie la telemetría.

/// Precio por millón de tokens (input, output) en USD para un modelo dado.
///
/// El emparejamiento es por subcadena (familia de modelo) para tolerar sufijos
/// de versión y fecha (`claude-sonnet-4-5-20250929`, `gpt-4o-2024-08-06`, …).
/// Devuelve `None` si no reconocemos el modelo.
pub fn price_per_mtok(model: &str) -> Option<(f64, f64)> {
    let m = model.to_ascii_lowercase();

    // Anthropic (Claude). Orden importa: comprobamos lo más específico primero.
    if m.contains("claude") {
        if m.contains("opus") {
            return Some((15.0, 75.0));
        }
        if m.contains("haiku") {
            return Some((0.80, 4.0));
        }
        if m.contains("sonnet") {
            return Some((3.0, 15.0));
        }
    }

    // OpenAI (GPT / o-series).
    if m.contains("gpt-4o-mini") {
        return Some((0.15, 0.60));
    }
    if m.contains("gpt-4o") {
        return Some((2.50, 10.0));
    }
    if m.contains("gpt-4-turbo") {
        return Some((10.0, 30.0));
    }

    None
}

/// Estima el coste en USD de un request a partir de los tokens medidos.
///
/// Requiere modelo conocido y al menos los tokens de salida; los de entrada,
/// si faltan, cuentan como cero. Devuelve `None` cuando no podemos calcular con
/// honestidad (modelo desconocido o sin datos de tokens).
pub fn estimate_cost_usd(
    model: Option<&str>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> Option<f64> {
    let model = model?;
    let (price_in, price_out) = price_per_mtok(model)?;

    // Sin ningún token medido no hay nada que valorar.
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }

    let input = input_tokens.unwrap_or(0) as f64;
    let output = output_tokens.unwrap_or(0) as f64;

    Some(input / 1_000_000.0 * price_in + output / 1_000_000.0 * price_out)
}
