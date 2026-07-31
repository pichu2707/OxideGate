# Rehidratación del histórico — `GET /history`

> Estado: implementado, con tests unitarios (`src/telemetry/rehydrate.rs`,
> `src/middleware/history.rs`) y verificado contra 2.960 filas reales.

---

## 1. El problema que resuelve

`telemetry.jsonl` se escribe desde siempre. Nadie lo leía.

Los tres consumidores viven en RAM y arrancaban vacíos, así que reiniciar el
proxy —tras un `brew upgrade`, tras cerrar el portátil— borraba la única
pregunta que el usuario quiere contestar de verdad: **¿estoy gastando más o
menos que la semana pasada?**

| Superficie | Antes de reiniciar | Después |
|---|---|---|
| `GET /stats` | agregado en RAM | **se rehidrata** |
| `GET /sessions` | agregado en RAM | **se rehidrata** |
| `GET /requests` | últimas 200 en vivo | vacío, **a propósito** |

---

## 2. Qué NO se rehidrata, y por qué

**`/requests` se queda vacío.** Su contrato es *«las últimas 200 EN VIVO»*, y
mezclar ahí filas de hace una semana lo rompería. Un agregado tolera histórico
—esa es su naturaleza—; una ventana de las últimas N, no.

Por la misma razón, `RequestMetric::cache_by_section` **no se deserializa
nunca**: es un campo exclusivo de `/requests`, así que saltarlo no es un atajo,
es la frontera correcta. De paso evita tener que hacer deserializable un
`&'static str`, que no lo es.

---

## 3. La ventana

Un `.jsonl` de meses no se puede releer entero en cada arranque sin retrasar el
primer request.

| Valor | Efecto |
|---|---|
| sin definir | **7 días** — una semana es el marco de la pregunta que esto contesta |
| `OXIDEGATE_HISTORY_DAYS=30` | treinta días |
| `OXIDEGATE_HISTORY_DAYS=0` | **desactiva** la rehidratación; ni siquiera abre el fichero |

Un valor no numérico **no cae al defecto en silencio**: avisa por `stderr` y
usa el defecto. Tragarse una variable mal escrita y comportarse distinto de lo
que se pidió es el fallo silencioso que este proyecto persigue en el resto del
código.

---

## 4. `GET /history`

```json
{
  "window_days": 7,
  "rows": 419,
  "oldest": "2026-07-24T21:55:03.308534836+00:00",
  "skipped_old": 2541,
  "skipped_bad": 0
}
```

| Campo | Qué es |
|---|---|
| `window_days` | La ventana que se pidió. `0` = rehidratación desactivada |
| `rows` | Filas incorporadas a los agregados |
| `oldest` | **Desde cuándo mide `/stats`.** `null` si no se rehidrató nada |
| `skipped_old` | Filas fuera de la ventana. No es un error: es la ventana haciendo su trabajo |
| `skipped_bad` | Filas ilegibles **dentro** de la ventana. Ver abajo |

### Por qué una ruta nueva y no un campo en `/stats`

Lo natural sería añadir la ventana a `/stats`. **No se puede sin romper**:
`StatsSnapshot` es un `Vec`, así que `/stats` serializa como **array**.
Convertirlo en objeto sería ruptura de contrato, subiría `CONTRACT_VERSION` y
además rompería el `brew test` de la fórmula, que afirma `[]` sobre un proxy
recién arrancado.

Una ruta nueva es aditiva: `ENDPOINTS` pasa de cuatro a cinco y nadie que ya
consumiera `/stats` se entera.

### `oldest: null` no significa «desde ahora»

Significa que no se incorporó ninguna fila: primer arranque, ventana a cero, o
nada dentro de la ventana. Los tres son estados distintos y quien quiera
distinguirlos tiene que mirar también `rows` y `window_days`.

---

## 5. Tolerancia: una fila corrupta no puede tumbar el arranque

El fichero es append-only y puede tener una última línea truncada —lo que deja
un corte a mitad de escritura— o filas de una build anterior. Ninguna de las
dos impide arrancar: se cuentan y se sigue.

### El orden de las comprobaciones importa, y se aprendió midiendo

Se comprueba **la ventana antes que el parseo completo**. No es una
optimización: es lo que hace que `skipped_bad` signifique algo.

Con el orden inverso, una fila de hace seis meses escrita por una build antigua
se contaba como ilegible aunque la ventana la fuera a descartar igualmente. En
el fichero real de desarrollo eso daba **2.387 «ilegibles» de 2.960** — ruido
suficiente para tapar las filas rotas que sí importan, las que caen dentro de la
ventana. Un aviso que siempre grita no avisa.

Con el orden correcto y los mismos datos: **419 filas, `skipped_bad: 0`**.

### La tolerancia hay que declararla también en los tipos anidados

`#[serde(default)]` en los campos de `RequestMetric` **no basta**. Una fila
antigua con `tools_by_server` pero sin `tool_names` dentro fallaba con
`missing field 'tool_names'` — y era el 80% del histórico.

Los campos internos que nacieron después necesitan su propio `default`. Hay
test de regresión (`una_fila_con_tools_by_server_sin_tool_names_se_rehidrata_igual`).

---

## 6. Consultar por rango: `?since=`

`GET /stats` y `GET /sessions` aceptan `?since=`, en dos formas:

| Forma | Ejemplo | Para |
|---|---|---|
| Fecha ISO | `?since=2026-07-24` | «desde el lunes» |
| Días atrás | `?since=7d` | «la última semana» |

Las dos existen porque contestan preguntas distintas, y obligar a traducir una
en la otra desde fuera es trabajo que el servidor puede hacer.

**Sin el parámetro, el comportamiento es el de siempre** — todo lo acumulado —,
así que las dos rutas siguen siendo compatibles.

### Un `since` ilegible devuelve 400

No cae a «todo el histórico». Servir un rango distinto del pedido, en silencio,
dejaría al consumidor mirando una ventana que no es la suya y creyendo que sí.
Es la misma regla que `OXIDEGATE_HISTORY_DAYS`, que tampoco se traga un valor
mal escrito.

### Lo que NO aparece en una ventana

Un `(upstream, modelo)` o una sesión **sin tráfico en el rango no sale en el
resultado**, en vez de salir con ceros. «No se usó» y «se usó y costó cero» son
afirmaciones distintas, y una fila a cero se lee como la segunda.

### Cómo se implementa, y qué costó

Los acumuladores no tenían **ninguna** dimensión temporal: eran sumas corridas
desde que arrancaba el proceso. `?since=` no era añadir un parámetro, era
cambiar cómo agregan.

Ahora la unidad es **el día**: `ModelAccumulator` y `SessionAccumulator`
guardan un cubo por día y el total de siempre es la fusión de todos. **Una sola
fuente de verdad** — no hay un total corriendo en paralelo a los cubos que
pudiera desviarse.

Por hora multiplicaría por 24 el número de cubos para contestar una pregunta
que nadie hace con precisión de minutos.

**La fusión de huellas es lo único que no se puede hacer sumando.** Un mismo
prompt visto dos días es UNA huella distinta, no dos: los mapas se **unen**
sumando ocurrencias. Sumar `len()` de dos cubos daría el doble de
`distinct_prompts`. Hay test.

Y el cap de huellas (`MAX_DISTINCT_PROMPTS_PER_MODEL`) es **compartido entre
cubos**, no por cubo: si fuera por cubo, la memoria se multiplicaría por los
días de ventana —que configura el usuario— y dejaría de estar acotada.

## 7. Qué queda fuera

**Rotación o agregado previo del `.jsonl`**: el fichero sigue creciendo sin
límite. La ventana acota lo que se LEE, no lo que se guarda.

---

## 8. Lo que deliberadamente NO se hizo

Sustituir el JSONL por una base de datos «porque escala». El fichero plano es
inspeccionable con `jq`, se comparte adjuntándolo y ya es la fuente de verdad
documentada en [`telemetry-per-request.md`](telemetry-per-request.md). El hueco
nunca fue el formato de almacenamiento: era que **nadie leía lo que ya se
guardaba**.

---

## 9. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/rehydrate.rs` | `rehydrate`, `Rehydrated`, `history_days_from_env` — puro, sin locks ni axum |
| `src/middleware/history.rs` | `handle_history` — la ruta HTTP y su estado congelado |
| `src/main.rs` | Llama a `rehydrate` **antes** de servir la primera petición |
| `src/telemetry/logger.rs` | `RequestMetric: Deserialize`, con la tolerancia declarada campo a campo |
| `src/telemetry/stats.rs` | `DayBucket`, `ModelAccumulator::merge`, `snapshot_since` — la agregación por día |
| `src/middleware/mod.rs` | `parse_since` — el contrato de `?since=`, compartido por las dos rutas |
