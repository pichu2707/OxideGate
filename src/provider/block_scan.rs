//! El recorrido compartido por los detectores de bloques del body.
//!
//! [`skills`](super::skills) e [`instructions`](super::instructions) buscan
//! cosas distintas —un listado de capacidades y el fichero de instrucciones del
//! usuario— pero tropiezan con **el mismo problema**, y por la misma razón: las
//! marcas que delimitan los bloques son cadenas en inglés que aparecen también
//! en el texto que el usuario escribe.
//!
//! Los dos casos, los dos medidos en tráfico real:
//!
//! - `<available_skills>` aparece **cinco veces** en un body de opencode y sólo
//!   UNA es el listado; las otras cuatro son el `AGENTS.md` del usuario hablando
//!   del bloque entre comillas.
//! - `<system-reminder>` aparece **tres veces** en un body de Claude Code y sólo
//!   una es el bloque de instrucciones; otra queda abierta y sin cerrar en
//!   `$.system[2].text`, y otra vive dentro de la descripción de una herramienta.
//!
//! De ahí la regla que este módulo encapsula, y que no se puede relajar en uno
//! de los dos detectores sin que el otro se vuelva mentiroso: **un bloque sólo
//! cuenta si contiene la marca interna que lo hace ser lo que dice ser**, y si
//! el primero no la tiene, se sigue buscando en vez de rendirse.

/// Primer bloque delimitado por `abre`/`cierra` que contenga al menos una
/// `requerido`, junto con cuántas contiene.
///
/// Buscar sólo la primera apertura no sirve: en tráfico real la primera suele
/// ser una mención dentro del texto del usuario. Y quedarse con cualquier
/// bloque tampoco: sin la marca interna no es un bloque, es alguien hablando de
/// uno.
///
/// # Una apertura sólo cuenta si el cierre es SUYO
///
/// El cierre que se toma es el primero que caiga tras la etiqueta de apertura,
/// **y sólo si entre ambos no aparece OTRA apertura**. Si aparece, la actual se
/// descarta como señuelo sin cerrar y el recorrido sigue después de ella.
///
/// Sin esa comprobación, una apertura señuelo sin cierre propio seguida de un
/// bloque real y bien cerrado se fusionaban en uno: medido, 123 B donde el
/// bloque real eran 67. Sobremedir en silencio es el defecto que esta función
/// existe para evitar, así que la comprobación no es un adorno.
///
/// # Límites conocidos, y los dos van en la misma dirección
///
/// 1. Si el CONTENIDO de un bloque con marca menciona literalmente la etiqueta
///    de APERTURA, ese bloque se SALTA: sus dos aperturas hacen que ninguna
///    parezca cerrada por el cierre final. Puede acabar en `None`.
/// 2. Si el contenido escribe literalmente la cadena de CIERRE —posible, porque
///    en `instructions` ese contenido es un fichero markdown del usuario— la
///    medida sale CORTA: se toma el primer cierre, no el último.
///
/// Las dos miden de MENOS o declaran ausencia. Nunca de más. Es deliberado:
/// medir de menos en un caso raro y declararlo es honesto; medir de más en
/// silencio, no. Ambos límites están publicados en
/// `docs/telemetry-per-request.md` §4.8 y §4.13, no sólo aquí.
///
/// # Por qué los dos cursores no retroceden
///
/// El coste es LINEAL sobre el texto, y hace falta que lo sea: esto corre
/// dentro de `Provider::prepare`, en cada petición, sobre cuerpos de ~190 kB.
///
/// La versión ingenua —buscar el cierre desde cada apertura— es cuadrática en
/// cuanto hay muchas aperturas señuelo: medido, 4.000 señuelos en 204 kB
/// costaban 93 ms, y el coste se cuadruplicaba al doblar la entrada. Aquí el
/// cursor del cierre sólo avanza, así que cada cierre del texto se busca una
/// vez EN TOTAL y no una vez por apertura.
///
/// Devuelve `None` si no hay ninguna apertura, si ninguna llega a cerrarse, o
/// si ningún bloque bien cerrado contiene la marca.
///
/// Una `abre` vacía devuelve `None` de entrada. No es paranoia de firma: con la
/// cadena vacía `find` acierta siempre en el sitio, el cursor no avanza y el
/// bucle **no termina** — comprobado. Los tres llamadores de hoy pasan
/// literales, pero un `debug_assert` desaparece en release y lo que se colgaría
/// es el hilo que atiende la petición. Un guardián que sí viaja en el binario
/// cuesta una comparación por llamada.
pub(super) fn primer_bloque_con<'a>(
    texto: &'a str,
    abre: &str,
    cierra: &str,
    requerido: &str,
) -> Option<(&'a str, usize)> {
    if abre.is_empty() {
        return None;
    }

    // Cursor del cierre. Sólo avanza, nunca vuelve atrás: es lo que mantiene
    // el recorrido lineal.
    let mut cierre = texto.find(cierra);
    let mut desde = 0usize;

    while let Some(rel) = texto[desde..].find(abre) {
        let i = desde + rel;
        let contenido = i + abre.len();

        // El cierre vigente tiene que caer DESPUÉS de la etiqueta de apertura.
        // Exigirlo aquí también resuelve los pares de marcadores que se SOLAPAN
        // —el cierre empieza dentro de la apertura, p. ej. `abre = "<X"` con
        // `cierra = "X>"`—: en vez de rebanar un rango invertido, que sería un
        // panic, se busca el siguiente cierre de verdad. Los tres pares de hoy
        // no se solapan, pero esta función existe para que añadir un harness
        // sea añadir un par de marcadores, y el cuarto no tiene por qué ser tan
        // amable.
        while cierre.is_some_and(|c| c < contenido) {
            let tras = cierre.unwrap_or_default() + cierra.len();
            cierre = texto
                .get(tras..)
                .and_then(|resto| resto.find(cierra))
                .map(|rel| tras + rel);
        }
        let fin_cierra = cierre?;

        // ¿Otra apertura entre medias? Entonces ese cierre no es el de esta
        // apertura, sino el de un bloque posterior.
        let siguiente_abre = texto[contenido..].find(abre).map(|rel| contenido + rel);
        if siguiente_abre.is_some_and(|a| a < fin_cierra) {
            desde = contenido;
            continue;
        }

        let bloque = &texto[i..fin_cierra + cierra.len()];
        let n = bloque.matches(requerido).count();
        if n > 0 {
            return Some((bloque, n));
        }
        desde = contenido;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El caso feliz: un bloque con la marca dentro, medido de marca a marca.
    #[test]
    fn mide_el_bloque_entero_incluidas_las_marcas() {
        let texto = "ruido <a>xx MARCA xx</a> ruido";

        let (bloque, n) = primer_bloque_con(texto, "<a>", "</a>", "MARCA").expect("hay bloque");

        assert_eq!(bloque.len(), "<a>xx MARCA xx</a>".len());
        assert_eq!(n, 1);
    }

    /// La razón de ser del módulo: la primera apertura es una mención sin
    /// marca, y hay que seguir buscando en vez de devolver `None`.
    #[test]
    fn se_salta_las_menciones_hasta_dar_con_el_bloque_bueno() {
        let texto = "habla de <a>nada</a> y luego <a>MARCA</a>";

        let (bloque, n) = primer_bloque_con(texto, "<a>", "</a>", "MARCA").expect("hay bloque");

        assert_eq!(bloque.len(), "<a>MARCA</a>".len());
        assert_eq!(n, 1);
    }

    /// Una apertura sin cierre no puede tumbar el proxy ni fabricar un bloque.
    #[test]
    fn una_apertura_sin_cierre_no_hace_panic_ni_fabrica_nada() {
        assert!(primer_bloque_con("<a>MARCA sin cerrar", "<a>", "</a>", "MARCA").is_none());
    }

    /// Sin marca interna no hay bloque, por muchos envoltorios que haya.
    #[test]
    fn sin_la_marca_interna_no_hay_bloque() {
        assert!(primer_bloque_con("<a>uno</a><a>dos</a>", "<a>", "</a>", "MARCA").is_none());
        assert!(primer_bloque_con("", "<a>", "</a>", "MARCA").is_none());
    }

    /// Sobremedida encontrada por doble revisión: una apertura señuelo sin su
    /// propio cierre, seguida de un bloque real y bien cerrado en la MISMA
    /// cadena, se fusionaba en un solo bloque de 123 B en vez de los 67 B del
    /// bloque real (+56 B, 1.8x). El código medía de más en silencio,
    /// contradiciendo su propia doc.
    #[test]
    fn una_apertura_sin_cerrar_no_se_traga_el_bloque_siguiente() {
        let texto = "<system-reminder>\nrecordatorio abierto que nunca cierra\n".to_string()
            + "<system-reminder>\n# claudeMd\ncontenido de verdad\n</system-reminder>";

        let (bloque, n) = primer_bloque_con(
            &texto,
            "<system-reminder>",
            "</system-reminder>",
            "# claudeMd",
        )
        .expect("hay bloque real tras el señuelo");

        assert_eq!(
            bloque.len(),
            67,
            "debe medir sólo el bloque real, no el señuelo fusionado"
        );
        assert_eq!(n, 1);
    }

    /// Un par de marcadores que se SOLAPA —el cierre empieza dentro de la
    /// apertura— hacía que el primer cierre cayera ANTES del final de la
    /// apertura. Rebanar ese rango invertido es un panic, y este es un helper
    /// genérico: los tres pares de hoy no se solapan, pero el cuarto harness
    /// que entre no tiene por qué avisar.
    ///
    /// Exigir que el cierre caiga tras la apertura no sólo evita la caída:
    /// encuentra el cierre BUENO y mide el bloque entero.
    #[test]
    fn un_par_de_marcadores_solapado_no_hace_panic_y_mide_bien() {
        let texto = "<X>MARCA</X>";

        let (bloque, n) = primer_bloque_con(texto, "<X", "X>", "MARCA").expect("hay bloque");

        assert_eq!(
            bloque.len(),
            texto.len(),
            "mide el bloque entero, no un trozo"
        );
        assert_eq!(n, 1);
    }

    /// **Fija la frontera `<` del cursor del cierre**, que ningún otro test
    /// tocaba: una revisión adversarial mutó `c < contenido` a `c <= contenido`
    /// y los ocho tests seguían en verde.
    ///
    /// Aquí `<a></a>` se cierra a sí mismo sin contenido, y después hay texto
    /// con la marca y un cierre huérfano. NO hay ningún bloque real con la
    /// marca dentro, así que la respuesta es `None`. Con la mutación salían
    /// 15 B fabricados de la nada — la sobremedida silenciosa otra vez.
    #[test]
    fn un_bloque_vacio_no_se_cierra_con_un_cierre_huerfano_posterior() {
        assert!(primer_bloque_con("<a></a>zzzz</a>", "<a>", "</a>", "zzzz").is_none());
    }

    /// Una apertura vacía colgaba el recorrido para siempre: `find` acierta
    /// siempre en el sitio y el cursor no avanza. El guardián viaja en el
    /// binario de release, no en un `debug_assert`, porque lo que se colgaría
    /// es el hilo que atiende la petición.
    #[test]
    fn una_apertura_vacia_no_cuelga_el_recorrido() {
        assert!(primer_bloque_con("MARCA X", "", "X", "MARCA").is_none());
    }

    /// El límite 1 documentado: un bloque bien cerrado cuyo CONTENIDO menciona
    /// la etiqueta de APERTURA se salta. Sale `None`, no una cifra inflada.
    ///
    /// Es la contrapartida del arreglo del señuelo: el recorrido no puede
    /// distinguir "apertura mencionada" de "apertura sin cerrar" sin inventarse
    /// una gramática, así que se queda del lado que mide de menos.
    #[test]
    fn una_apertura_mencionada_en_el_contenido_salta_el_bloque() {
        let texto = "<a>MARCA y aquí hablo de <a> sin abrir nada</a>";

        assert!(
            primer_bloque_con(texto, "<a>", "</a>", "MARCA").is_none(),
            "se salta el bloque: mide de menos, nunca de más"
        );
    }

    /// Cuenta cuántas veces aparece la marca: `skills` lo usa para `declared`.
    #[test]
    fn cuenta_todas_las_apariciones_de_la_marca() {
        let (_, n) = primer_bloque_con("<a>MARCA MARCA MARCA</a>", "<a>", "</a>", "MARCA")
            .expect("hay bloque");

        assert_eq!(n, 3);
    }
}
