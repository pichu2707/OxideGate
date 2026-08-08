//! Capa de reenvío: recibe la petición de gentle-ai y la espeja al proveedor.
pub mod health;
pub mod history;
pub mod mcp;
pub mod proxy;
pub mod requests;
pub mod sessions;
pub mod stats;
pub mod version;

/// Parámetro `?since=` de las rutas de agregado.
///
/// Acepta una fecha ISO (`2026-07-24`) o un número de días hacia atrás
/// (`7d`). Las dos formas existen porque contestan preguntas distintas: «desde
/// el lunes» es una fecha, «la última semana» es una duración, y obligar a
/// traducir una en la otra desde fuera es trabajo que el servidor puede hacer.
///
/// Un valor ilegible devuelve `400` en vez de caer a «todo el histórico».
/// Servir un rango distinto del que se pidió, en silencio, es peor que fallar:
/// el consumidor creería estar mirando una ventana y estaría mirando otra.
#[derive(Debug, serde::Deserialize, Default)]
pub struct SinceQuery {
    pub since: Option<String>,
}

/// Traduce `?since=` a la fecha desde la que hay que agregar.
///
/// `Ok(None)` = sin filtro (todo el histórico). `Err` = el valor no se
/// entendió, y el llamador debe devolver `400`.
pub fn parse_since(
    raw: Option<&str>,
    hoy: chrono::NaiveDate,
) -> Result<Option<chrono::NaiveDate>, String> {
    let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if let Some(dias) = v.strip_suffix('d') {
        return dias
            .parse::<i64>()
            .ok()
            .and_then(|d| hoy.checked_sub_days(chrono::Days::new(d.max(0) as u64)))
            .map(Some)
            .ok_or_else(|| format!("`since={v}`: número de días no válido"));
    }
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| format!("`since={v}`: use una fecha YYYY-MM-DD o un número de días como `7d`"))
}

#[cfg(test)]
mod tests_since {
    use super::*;

    fn hoy() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap()
    }

    /// Sin parámetro, sin filtro: la ruta se comporta como siempre.
    #[test]
    fn sin_parametro_no_filtra() {
        assert_eq!(parse_since(None, hoy()), Ok(None));
        assert_eq!(parse_since(Some(""), hoy()), Ok(None));
        assert_eq!(parse_since(Some("   "), hoy()), Ok(None));
    }

    /// Las dos formas contestan preguntas distintas y las dos deben valer.
    #[test]
    fn acepta_fecha_y_dias_hacia_atras() {
        assert_eq!(
            parse_since(Some("2026-07-24"), hoy()),
            Ok(chrono::NaiveDate::from_ymd_opt(2026, 7, 24))
        );
        assert_eq!(
            parse_since(Some("7d"), hoy()),
            Ok(chrono::NaiveDate::from_ymd_opt(2026, 7, 24))
        );
        assert_eq!(parse_since(Some("0d"), hoy()), Ok(Some(hoy())));
    }

    /// UN VALOR ILEGIBLE FALLA, no cae a «todo el histórico».
    ///
    /// Servir un rango distinto del pedido en silencio dejaría al consumidor
    /// mirando una ventana que no es la suya y creyendo que sí. Es la misma
    /// regla que `OXIDEGATE_HISTORY_DAYS`, que tampoco se traga un valor malo.
    #[test]
    fn un_valor_ilegible_falla_en_vez_de_ignorarse() {
        for malo in ["ayer", "2026-13-45", "semana", "-", "d"] {
            assert!(
                parse_since(Some(malo), hoy()).is_err(),
                "`{malo}` deberia fallar y no ignorarse"
            );
        }
    }
}
