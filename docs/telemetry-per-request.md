# Detalle en vivo por petición — `GET /requests`

> Estado: implementado y con tests unitarios (`src/telemetry/recent.rs`).
> Proyecta en memoria lo que el Nivel 1 (`docs/telemetry-level-1.md`) ya mide
> por fila; no cambia la captura ni agrega ningún campo nuevo a
> `RequestMetric`.

---

## 1. Qué es y para qué sirve

`GET /stats` (`docs/telemetry-by-model.md`) agrega por `(proveedor, modelo)`:
sirve para responder "¿qué modelo conviene optimizar?", pero un promedio
esconde la petición puntual que se disparó, la que tuvo un cache-miss aislado
o la que tardó 8 segundos en el primer token. Para ver **esa** fila en vivo,
hasta ahora había que abrir `telemetry.jsonl` a mano.

`GET /requests` responde eso: las últimas peticiones individuales,
proyectadas para lectura rápida, en orden cronológico (más vieja primero).

```
gentle-ai ──▶ OxideGate ──▶ proveedor
                 │
                 ├──▶ telemetry.jsonl        (fila a fila, persistente)
                 └──▶ RecentRequests (RAM)   (últimas 200 filas, en vivo)
                            │
                            ▼
                      GET /requests  (snapshot JSON)
```

---

## 2. Cómo consultarlo

```sh
curl localhost:8899/requests
```

(ajustar el puerto al que use la instancia). Devuelve `200 OK` con
`content-type: application/json`, sin autenticación: el proxy bindea en
`127.0.0.1`, igual que `/stats`.

### Ejemplo de salida

```json
[
  {
    "timestamp": "2026-07-09T14:02:11.483Z",
    "route": "/v1/messages",
    "upstream": "anthropic",
    "model": "claude-opus-4-1",
    "stream": true,
    "status": 200,
    "input_tokens": 5000,
    "output_tokens": 412,
    "cache_read_tokens": 4200,
    "cache_write_tokens": 0,
    "cost_estimate_usd": 0.0891,
    "cache_control_forced": false,
    "ttft_ms": 780.4,
    "total_ms": 3210.9,
    "session": { "source": "native", "key": "sess-abc123" }
  },
  {
    "timestamp": "2026-07-09T14:02:14.117Z",
    "route": "/v1/messages",
    "upstream": "anthropic",
    "model": "claude-opus-4-1",
    "stream": true,
    "status": 200,
    "input_tokens": 5000,
    "output_tokens": 398,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0,
    "cost_estimate_usd": 0.1620,
    "cache_control_forced": false,
    "ttft_ms": 2450.7,
    "total_ms": 5980.2
  }
]
```

La segunda fila del ejemplo es exactamente el tipo de anomalía que `/stats`
no puede mostrar: mismo modelo, mismo tamaño de input, pero sin
`cache_read_tokens` y con TTFT casi 3 veces más alto. Vista sola en un
promedio, se diluye entre el resto del tráfico.

Filas ausentes de dato (p. ej. `ttft_ms` en una petición sin streaming) se
serializan como `null`, nunca como `0`: un dato ausente y un cero real son
cosas distintas (ver `docs/telemetry-level-1.md`).

---

## 3. La invariante de privacidad (léase antes de exponer este endpoint)

`RecentRequest` — el tipo que serializa cada fila — **no tiene el campo
`prompt_hash`**. No es un filtro en tiempo de ejecución que alguien pueda
desactivar por error: es una garantía de compilación, porque el campo
directamente no existe en el struct. `telemetry.jsonl` sí guarda `prompt_hash`
por fila (para poder correlacionar redundancia offline), pero esa huella nunca
llega a la API HTTP.

> **`prompt_bytes` SÍ se publica**, desde #51 (§4.10). Hasta el 2026-08-08 esta
> sección decía que tampoco existía, y era falso: el struct lo lleva y hay un
> test que **exige** que la fila lo exponga. La invariante nunca fue "no salen
> datos del prompt" — es **"no sale ninguna HUELLA del prompt"**, y un contador
> de bytes no lo es: no permite reconstruir nada ni correlacionar dos peticiones
> con el mismo contenido, que es justo lo que `prompt_hash` sí permite. La misma
> distinción por la que los cinco `context_*_bytes` llevan publicándose desde el
> principio.

Esto mirroriza la misma invariante que ya documenta
`docs/telemetry-by-model.md` para los agregados de `/stats` y que impone
`src/middleware/stats.rs`: el proxy no expone huellas de prompt por HTTP,
haya o no autenticación de por medio.

**Los NOMBRES de herramienta sí viajan; el CONTENIDO no.** `tool_names`
(declaradas, §4.2) y `tool_calls` (invocadas, §4.15)
publican identificadores de herramienta por HTTP. No son huellas de prompt —
los eligió el cliente y ya se los declaró al proveedor en texto plano— pero
describen qué integraciones tiene montadas quien usa el proxy, así que
conviene saberlo antes de exponer el endpoint. Lo que **nunca** sale es el
contenido: ni un fragmento del `input_schema`/`description` que compone una
herramienta, ni el `input` con el que se la invocó. El doc de módulo de
`src/telemetry/recent.rs` afirmaba lo contrario —que los nombres individuales
nunca se publicaban— desde que `tool_names` entró en el contrato; se corrigió
junto con este slice.

**Esta invariante cubre huellas de prompt, no el campo `client`.** El campo
`client` (§4, §4.5) es un caso aparte: no es una huella de prompt, pero
tampoco es un dato que el proxy calcule — es el `User-Agent` del cliente,
reenviado crudo. Ver §4.5 antes de exponer este endpoint fuera de
`127.0.0.1` o de compartir `telemetry.jsonl`.

---

## 4. Qué señala cada campo

| Campo | Qué es | Cómo leerlo |
|---|---|---|
| `timestamp` | Instante en que se emitió la métrica (RFC 3339, UTC) | Ordena el buffer; el consumidor decide si invierte para "más nuevo arriba" |
| `route` | Ruta local que atendió el request (`/v1/messages`, …) | Distingue el dialecto de proveedor cuando hay varias rutas activas |
| `upstream` | Proveedor destino (`anthropic`, `openai`, …) | Junto con `model`, la clave de agrupación para comparar contra pares |
| `model` | Modelo solicitado, o `null` si no venía en el body | Un `null` sostenido en el tiempo suele indicar clientes mal configurados |
| `stream` | `true` si el cliente pidió SSE | Sin streaming, `ttft_ms` no aplica — ver `total_ms` en su lugar |
| `client` | `User-Agent` del request entrante, CRUDO (sin normalizar), topeado a 200 caracteres. `null` si el header no vino o no era UTF-8 válido | Distingue un harness que YA difiere tools MCP por su cuenta (Claude Code sin caer al fallback de carga upfront) de uno genuinamente eager — ver `docs/optimizer-tool-search.md` §3. **Léase §4.5 antes de exponer este campo**: a diferencia del resto de esta tabla, es contenido controlado por el cliente, no una propiedad que el proxy calcula |
| `status` | Código HTTP devuelto al cliente | `>= 400` es la señal de error más barata de todas: no necesita comparación con nada |
| `input_tokens` / `output_tokens` | Tokens exactos reportados por el proveedor | `null` si el proveedor no los reportó (p. ej. request fallido antes de leer `usage`) |
| `cache_read_tokens` / `cache_write_tokens` | Tokens servidos o escritos a caché | Una fila con `cache_read_tokens` en `0`/`null` en medio de una conversación larga que sí cachea es un miss caro y aislado |
| `cost_estimate_usd` | Coste estimado en USD según `pricing.rs` | `null` si no fue calculable |
| `cache_control_forced` | `true` si OxideGate inyectó el breakpoint de `cache_control` (Palanca A) | Sirve para correlacionar si la palanca estaba activa en esa fila puntual |
| `ttft_ms` | Time To First Token en ms | `null` si no aplica (sin streaming); un valor mucho más alto que el resto del mismo modelo es la señal de latencia percibida |
| `total_ms` | Latencia total, del request al cierre de la respuesta | Junto con `ttft_ms`, permite derivar el tiempo de generación (`total_ms - ttft_ms`) fuera del endpoint |
| `context_system_bytes` | Bytes del prompt de sistema | `null` si no se pudo calcular el desglose (ver `provider::ContextBreakdown`) |
| `context_tools_bytes` | Bytes del esquema de herramientas (tool definitions) | En tráfico real medido, esta fue la porción más grande del body (~71%) — un valor alto y estable en todas las filas es candidato a desconectar servidores MCP sin uso |
| `context_history_bytes` | Bytes de todos los mensajes del historial menos el último | Crece con la conversación; junto con `context_tools_bytes`, compite por ser la porción dominante del body |
| `context_last_turn_bytes` | Bytes del último mensaje — el turno genuinamente NUEVO de esta petición | Lo único que el cliente "agregó ahora"; en tráfico real medido llegó a ser tan poco como 0.06% del body |
| `context_other_bytes` | Bytes del resto de campos de control a nivel raíz del body | Normalmente chico; un salto sugiere un campo nuevo que el cliente empezó a mandar |
| `context_measured_bytes` | Suma de los cinco campos de arriba | Ver la nota sobre BYTES vs. tokens y vs. tamaño de wire, más abajo |
| `context_messages_count` | Cantidad de mensajes del historial completo (incluyendo el último) | Sube con la conversación; útil para correlacionar contra `context_history_bytes` |
| `context_tax_ratio` | `(context_system_bytes + context_tools_bytes + context_history_bytes) / context_measured_bytes` | Cercano a `1.0` (100%) ⇒ casi todo el body de esta petición es contexto YA enviado antes, no turno nuevo — la "tasa" que se paga por repetir contexto en cada request |
| `prepare_us` | Microsegundos que el proxy pasó dentro de `Provider::prepare` (parseo del body + `decompose` + mutación opcional, p. ej. inyectar `cache_control`) | Ver la nota sobre qué NO incluye, más abajo |
| `scan_us` | Microsegundos que el proxy pasó ESCANEANDO la respuesta (recorrido SSE por cada chunk, más el cierre). La otra mitad del overhead propio | Los dos juntos son el tiempo de CPU que cuesta observar — ver la nota de abajo |
| `load_us` · `prompt_eval_us` · `eval_us` | Microsegundos que el MOTOR dice haber tardado en cargar el modelo, procesar el prompt y generar. Solo los reporta un motor **local** (`ollama` nativo) | `null` en los cuatro dialectos de nube: no es que no carguen modelos, es que no lo reportan. Ver §4.18 |
| `energy_wh` · `energy_idle_wh` · `power_peak_w` · `energy_samples` | Vatios-hora que la GPU consumió **mientras la petición estuvo abierta**, el reposo equivalente, el pico y cuántas muestras reales lo sostienen | `null` con upstream **remoto**, sin `nvidia-smi`, con el muestreo apagado, o si el anillo no cubre la ventana. **Sumar sobre filas solapadas es inválido.** Ver §4.19 |
| `tools_by_server` | Desglose de `context_tools_bytes` por servidor MCP declarante: `[{server, kind, tools, bytes, deferred_tools, tool_names}, …]`, ordenado por `bytes` descendente | `null` si el body no parseó como objeto (o build anterior a este campo); `[]` si SÍ parseó pero no declaraba `tools` — son estados DISTINTOS, ver §4.2. `deferred_tools` (por elemento) es la fuente de verdad POR SERVIDOR de cuánto está diferido, ver §4.2 |
| `tools_overhead_bytes` | Bytes de `tools` no atribuidos a ningún servidor (brackets/comas del array, wrapper de Gemini, herramientas huérfanas) | `null` en los mismos casos que `tools_by_server` es `null`; `sum(tools_by_server[].bytes) + tools_overhead_bytes == context_tools_bytes` siempre que ambos sean no-nulos |
| `tool_search` | Señal de carga diferida de herramientas del dialecto Responses/Codex: `{used, deferred_loaded}`, o `null`. `used: false` ⇒ EAGER confirmado este turno; `used: true` ⇒ LAZY (el cliente cargó tools a mitad de sesión) | `null` en Anthropic/Gemini/OpenAI-Chat (no aplica) o si el body no parseó. Es el diferenciador eager-vs-lazy por cliente que `tools_by_server` NO puede dar — ver §4.3 |
| `tools_flattened` | Honestidad de la atribución de `tools_by_server`: `true` ⇒ el cliente NO usa el namespacing `mcp__`, así que su cubo `(native)` puede ocultar MCP aplanado; `false` ⇒ hay tools `mcp__`, el `(native)` es de fiar; `null` ⇒ no aplica (Anthropic/Gemini/Chat) o sin tools | Solo dialecto Responses/Codex. `pi` manda nombres crudos y `opencode` usa `<server>_<tool>` (ambiguo) — ninguno con `mcp__`. Es una advertencia estructural, NUNCA una atribución inventada: no nombra servidores. Ver §4.4 |
| `session` | Sesión resuelta por precedencia de cabeceras del request: `{source, key}`. Nunca `null` — la peor rama es un fallback honesto (`source: "unattributed"`), no una ausencia | Ver §4.6 para la tabla de precedencia completa y cómo estampar el header desde cada harness |
| `skills` | Listado de skills declarado en el body: `{declared, listing_bytes, format}`, o `null`. Se paga en CADA petición, se invoque una skill o no | `null` = no se reconoció ningún listado, **nunca "cero skills"**. `format` dice qué forma se encontró y, de paso, de qué herramienta viene el tráfico sin fiarse del `User-Agent` — ver §4.8 |
| `instructions` | Bloque de instrucciones del usuario (`CLAUDE.md`) declarado en el body: `{bytes, format}`, o `null`. Se paga en CADA petición. Medido: el 48% del peaje fijo de una sesión de Claude Code | `null` = no se reconoció ningún bloque, **nunca "el usuario no tiene instrucciones"** — Claude Code IGNORA `AGENTS.md`, así que ahí `null` es correcto. Ver §4.13 |
| `hooks` | Salida de los hooks de `SessionStart` inyectada en el body: `{bytes, declared, format}`, o `null`. Se paga en CADA petición. Medido: el 29% del peaje fijo, el segundo bloque más caro | `null` = no se reconoció el bloque, **nunca "no tienes hooks"**. Solo lo publica el dialecto de Anthropic: la marca es de Claude Code y los otros tres están SIN MEDIR. Ver §4.17 |
| `effort_forced` | Nivel de esfuerzo que IMPUSO el proxy (palanca B), o `null` si no intervino — el default | Se lee JUNTO a `requested_effort`, nunca en su lugar: es lo que impide confundir un ahorro del cliente con una intervención del medidor. Ver §4.14 |
| `tool_calls` | Invocaciones observadas en la RESPUESTA: `invoked` (cliente, MCP incluidas), `server_invoked` (`web_search`…), sus totales sin truncar, y `complete` | Contrapartida de `tool_names` (declaradas). `null` = este proveedor no tiene extractor, que NO es lo mismo que listas vacías. `complete: false` = la lista es un prefijo (turno abortado) y no sirve para concluir que un servidor no se usa. Ver §4.15 |
| `prompt_bytes` | Bytes del body que MANDÓ EL CLIENTE, en su forma lógica | **No es wire** (en `/v1/codex/responses` y `/v1beta/*` se mide descomprimido), **no es lo que subió al proveedor** (con `cache_control_forced` el body reenviado es mayor) y **no es la suma del desglose**. Ver §4.10 antes de usarlo |
| `response_bytes` | Bytes del CUERPO DE LA RESPUESTA que cruzaron el proxy. `null` si no llegó a haber respuesta | **Sin comprimir**, y por eso no es ancho de banda: el proxy descarta `Accept-Encoding` para poder leer el SSE. Ver §4.9 antes de compararlo con `prompt_bytes` |
| `codex_quota` | Estado de la cuota de suscripción de Codex (OAuth de `chatgpt.com`), parseado de las doce cabeceras `x-codex-*` de la RESPUESTA del upstream: doce campos, todos opcionales. `null` si la petición no fue a Codex vía OAuth, o si el upstream falló antes de responder | Es el ÚNICO campo de esta fila que se lee de la respuesta y no del request ni del body. Cuota NUNCA son dólares: no alimenta ni puede alimentar `cost_estimate_usd` — ver §4.7 |
| `input_share_by_section` | Fracción del input PAGADO por sección: `{method, tools_share, system_share, history_share, last_turn_share, other_share}`, o `null` | **Fracciones de 0 a 1 que suman 1 — nunca dinero.** Es una estimación SOBRE otra (se apoya en `cache_by_section`). Ver §4.12 antes de convertirlas en euros |
| `cache_by_section` | Qué cubo del contexto cayó dentro del prefijo cacheado: `{method, tools_cached_bytes, system_cached_bytes, history_cached_bytes, last_turn_cached_bytes, other_cached_bytes}`, o `null` | **El ÚNICO campo ESTIMADO de esta fila**: todos los `context_*_bytes` son medición directa, este se deduce. Por eso va anidado y no suelto. `null` = no atribuible; todo a cero = medido y nada cacheado. Ver §4.11 ANTES de pintar nada con él |

Ninguno de los campos de latencia/coste/identidad es nuevo: todos ya existían
en `RequestMetric` (Nivel 1). Los campos `context_*`, `prepare_us`,
`tools_by_server` y `tools_overhead_bytes` sí son nuevos — provienen del
desglose de contexto (`provider::ContextBreakdown`), de instrumentar
`Provider::prepare`, y del desglose de herramientas por servidor
(`provider::ToolServerBytes`) respectivamente. Este endpoint sigue sin medir
nada por su cuenta: solo expone en vivo lo que `RequestMetric` ya mide.

### 4.1. Tres precisiones que hay que leer antes de usar estos campos

- **`context_*` son BYTES, nunca tokens.** Se calculan re-serializando cada
  bucket del body a JSON canónico y midiendo su longitud en bytes — no hay
  tokenización de por medio en ningún punto de este cálculo. No los uses
  como proxy de "cuántos tokens cuesta esto"; para eso están `input_tokens` /
  `output_tokens`, que sí vienen del proveedor.
- **`context_measured_bytes` es, por diseño, distinto del tamaño de wire del
  request**, y los dos NUNCA deben combinarse en un solo ratio. El tamaño de
  wire incluye framing HTTP, y el JSON puede re-serializarse con espaciado o
  ausencia de campos ligeramente distinta a como llegó originalmente
  (canonicalización). Mezclar ambos números en una sola fracción (p. ej.
  `context_measured_bytes / tamaño_de_wire`) produciría un ratio sin
  significado estable: son dos mediciones de cosas relacionadas pero no
  idénticas, tomadas en puntos distintos del pipeline.
- **`prepare_us` mide ÚNICAMENTE el tiempo dentro de `prepare`** (parseo del
  body, `decompose`, y la mutación opcional del body si aplica). NO incluye:
  leer el body completo desde el socket del cliente, ni el round-trip al
  proveedor upstream. Es el overhead propio de OxideGate en esa fase
  puntual, no la latencia total de la petición — para eso está `total_ms`.
- **`scan_us` mide la OTRA fase propia: el escaneo de la respuesta.** El
  recorrido SSE corre por cada chunk buscando el `usage`, y hasta que existió
  este campo no lo medía nadie. Con solo `prepare_us` no se podía afirmar que
  observar saliera barato, únicamente suponerlo.

  Medido en una petición en streaming real: **`prepare_us` 259 µs contra
  `scan_us` 3.534 µs**. El escaneo cuesta unas **13 veces más** que la
  preparación — la mitad que sí se medía era la barata.

  `prepare_us + scan_us` es el tiempo de CPU que cuesta observar. Sobre esa
  misma petición: **0,15% de `total_ms`**. La premisa del proyecto se sostiene,
  pero ahora es un hecho auditable y no una creencia.

  NO son `Option`: el escaneo siempre ocurre. Un cero es un cero MEDIDO —una
  respuesta sin chunks, o un upstream que nunca contestó— no un dato ausente.
  Y el número **incluye su propio coste de medición** (dos `Instant::now()` por
  chunk): decenas de nanosegundos frente a un parseo de JSON, irrelevante para
  lo que se decide con él, pero se dice en vez de fingir que medir es gratis.

Ver `docs/monitor-tui.md` §7.3 para cómo el monitor presenta estos campos en
la vista `Context` del panel de requests recientes.

### 4.2. `tools_by_server`: el único campo no-plano de esta fila, y por qué

Todos los demás campos de `/requests` son escalares (`number`, `string`,
`boolean`, o `null`). `tools_by_server` es la excepción: un array de objetos,
uno por servidor MCP que declaró herramientas en el body. La razón es que su
cardinalidad depende enteramente del cliente que hizo el request — uno sin
ningún MCP conectado declara cero servidores; uno con cuatro conectados (como
en el tráfico real medido en `docs/monitor-tui.md` §8) declara cuatro filas.
Aplanar esto a columnas fijas (`server_1`, `server_2`, …) no es viable
porque no hay un tope fijo de servidores por request — `provider::MAX_TOOL_SERVERS`
(32) es un límite de trackeo interno, no un contrato de forma para este
endpoint.

Cada elemento trae:

| Campo | Qué es |
|---|---|
| `server` | Etiqueta de display del servidor (`(native)` para herramientas nativas, `claude_ai_Gmail`, `plugin_engram_engram`, …) |
| `kind` | `"native"` / `"mcp"` / `"others"` — el tipo de cubo, en minúsculas |
| `tools` | Cantidad de herramientas atribuidas a este servidor |
| `bytes` | Suma de bytes de las herramientas de este servidor |
| `deferred_tools` | Cuántas de `tools` traían `defer_loading: true` en su propia definición dentro del array `tools[]` del body ENTRANTE. `0` en `openai`/`gemini`: NO porque el diferido no exista en esos dialectos, sino porque en el dialecto Responses/Codex las tools diferidas NO viajan en `tools[]` (siempre eager) sino en items `tool_search_output` dentro de `input[]` — para esa señal, ver el campo `tool_search` (§4.3), no este |

**`tools` y `bytes` son conteos y BYTES, nunca tokens** — mismo contrato de
medición que `context_tools_bytes` (§4.1): se miden re-serializando el
fragmento JSON de cada herramienta, sin tokenización de por medio.

**`deferred_tools` es la fuente de verdad POR SERVIDOR de cuánto diferido
hay.** Un consumidor que lea `deferred_tools` por elemento obtiene una
afirmación exacta sobre ESE servidor, nunca sobre el body completo:

- `deferred_tools == tools` → ese servidor está totalmente diferido.
- `deferred_tools == 0` → ese servidor no difirió NADA — sus `bytes` son
  reales y desconectables.
- `0 < deferred_tools < tools` → diferido parcial, el caso que antes era
  invisible (ver `docs/optimizer-tool-search.md`, defecto de revisión
  adversarial ronda 3).

**DOMINIO: tokens de contexto, no bytes de cable.** `deferred_tools` registra
si la definición trae la marca `defer_loading` en el body ENTRANTE — nunca
cuántos bytes viajaron por el cable de ESTE request. El mecanismo de la API
de Anthropic AÑADE los esquemas descubiertos al final del prompt, no los
retiene (`docs/optimizer-tool-search.md` §2.2): una definición marcada con
`defer_loading: true` sigue viajando completa en `tools`. No mezclar este
campo con una afirmación de bytes-no-enviados.

**Nunca se exponen nombres de herramienta individuales.** Solo la etiqueta
del servidor y conteos agregados viajan por este endpoint — la misma
invariante de privacidad del §3 (`prompt_hash` nunca se expone) se extiende
aquí: el nombre de una herramienta puntual, o un fragmento de su
`input_schema`/`description`, tampoco sale por HTTP.

**`null` vs. `[]` son estados DISTINTOS, no intercambiables:**

| Valor | Qué significa |
|---|---|
| `null` | El body de esta petición no parseó como objeto JSON — no se pudo ni intentar calcular el desglose. Mismo caso que `context_tools_bytes: null` |
| `[]` | El body SÍ parseó como objeto, pero no declaraba ningún `tools` (ausente, no-array, o array vacío) — se pudo calcular, y el resultado es "cero servidores" |

Confundir ambos llevaría a leer "sin dato" donde en realidad hay un dato real
de "sin herramientas". El monitor (`docs/monitor-tui.md` §8.1) respeta esta
distinción al elegir la fila fuente del panel de tools por servidor: una fila
`[]` no califica como fuente, con el mismo criterio de "no es lo mismo que no
tener dato".

**La reconciliación siempre cierra:** `sum(tools_by_server[].bytes) +
tools_overhead_bytes == context_tools_bytes`, cuando los tres son no-nulos.
El overhead absorbe los brackets/comas del array `tools` en sí, el wrapper
`{"functionDeclarations": [...]}` que usa Gemini (sin equivalente en
Anthropic/OpenAI, donde cada herramienta ES el elemento del array, sin
wrapper), y herramientas huérfanas sin `name` válido. Ver
`provider::tools_overhead_bytes` en el proxy para el detalle completo de los
tres contribuyentes.

---

#### `tool_names`: el hecho crudo, para que atribuya quien puede

Cada fila lleva los **nombres** de las herramientas que la componen, tal como
viajaron. OxideGate **no deduce** a qué servidor pertenece cada uno cuando el
cliente aplana — no tiene con qué — pero sí puede publicar que **estos nombres
cruzaron**.

```json
{ "server": "(native)", "kind": "native", "tools": 2,
  "tool_names": ["engram_mem_search", "delegation_list"] }
```

Ahí está la asimetría que hace útil el campo: `engram_mem_search` es MCP
aplanado y `delegation_list` es nativa de verdad, y **OxideGate no puede
distinguirlas**. Partir por `_` atribuiría `delegation` como si fuera un
servidor — el error que `tools_flattened` (§4.4) existe para no cometer.

Pero un consumidor que tenga la **lista autoritativa** de tools por servidor sí
puede: cruza cada nombre contra ella. `engram_mem_search` casa con
`engram → mem_search`; `delegation_list` no casa con ninguna lista MCP y sigue
siendo nativa. Un nombre que case con dos servidores se reporta como ambiguo,
no se adivina.

> **El reparto:** OxideGate ve el cable pero no sabe de quién es cada tool.
> Quien tiene el inventario no ve el cable. Ninguno de los dos puede solo — por
> eso el proxy publica el hecho en vez de una conclusión.

**La lista está acotada, y el recorte se ve.** Máximo 64 nombres por fila, de
128 bytes cada uno. El body es entrada controlada por quien llama y estas filas
viven en el buffer de 200, así que sin cota sería un vector de crecimiento de
memoria — el mismo que ya acota el tope de servidores. **El conteo `tools` NO
se trunca**: si `tool_names.len() < tools`, la lista está recortada y el bueno
es `tools`. No hace falta un campo extra que pudiera desincronizarse.

**Privacidad**: son nombres de herramienta, no argumentos ni contenido de
prompt. Mismo nivel de exposición que el conteo y los bytes que la fila ya
publicaba (§3).

---

### 4.3. `tool_search`: eager vs. lazy en el dialecto Responses/Codex

`tools_by_server[].deferred_tools` (§4.2) mide `defer_loading` **dentro de
`tools[]`**. Para Anthropic eso alcanza: ese cliente marca las tools diferidas
en el propio array `tools[]`. Pero el dialecto **OpenAI/Codex Responses**
(clientes `pi` y `opencode`) funciona distinto, y por eso hace falta un campo
aparte.

**La verdad del terreno** (verificada contra el código de `@earendil-works/pi-ai`,
`dist/api/openai-responses.js` + `openai-responses-shared.js`): el `tools[]` de
nivel superior de una petición Responses es **siempre EAGER** — el cliente arma
`params.tools = convertResponsesTools(immediate)` SIN la marca `deferLoading`.
Las tools que sí se difieren NO aparecen en `tools[]`: se cargan a mitad de
sesión y reaparecen dentro de `input[]` como items `tool_search_output`
(precedidos de un `tool_search_call`, ambos `execution: "client"`), y ahí cada
tool sí trae `defer_loading: true`.

Consecuencia: medir `defer_loading` sobre `tools[]` da `deferred_tools == 0`
para estos clientes — y ese `0` es **correcto** (el set base ES eager), no un
bug. El señalador real de comportamiento lazy es la **presencia de items
`tool_search_*` en `input[]`**, que es exactamente lo que mide `tool_search`.

| Valor | Significado |
|---|---|
| `null` | Dialecto donde el concepto no aplica (Anthropic, Gemini, OpenAI Chat), o body que no parseó — "no se pudo ni mirar" |
| `{used: false, deferred_loaded: 0}` | Petición Responses/Codex medida, sin items `tool_search_*` este turno: **EAGER confirmado** (no ausencia de dato) |
| `{used: true, deferred_loaded: N}` | **LAZY**: el cliente ejercitó la búsqueda diferida; `N` tools con `defer_loading: true` se cargaron vía `tool_search_output` |

`tool_search` es un objeto de forma FIJA (dos campos), a diferencia del array
de longitud variable de `tools_by_server`. `used` puede ser `true` con
`deferred_loaded: 0` cuando hubo un `tool_search_call` que no llegó a cargar
ninguna tool.

**No dobla bytes.** Los bytes de esos items `tool_search_output` ya los miden
los campos `context_*` (viven en `input`, cuentan como `context_history_bytes`
/ `context_last_turn_bytes`). `tool_search` solo **cuenta y clasifica**; nunca
vuelve a sumar esos bytes ni los mezcla en `tools_by_server` — misma disciplina
de "una medición, un dueño" que el resto de la fila.

---

### 4.4. `tools_flattened`: cuándo NO fiarse del cubo `(native)`

`tools_by_server` (§4.2) atribuye cada herramienta a su servidor partiendo el
namespacing `mcp__<server>__<tool>` (separador `__`, **inequívoco**). Claude
Code lo usa, así que su cubo `(native)` es de fiar: una tool sin `mcp__` es
genuinamente nativa.

Pero **los clientes del dialecto Responses/Codex no usan `mcp__`** — medido en
tráfico real:

- **`opencode`** prefija sus tools MCP como `<server>_<tool>`:
  `context7_query-docs`, `engram_mem_search`. El separador es UN solo `_`,
  **ambiguo**: colisiona con nombres nativos (`apply_patch`, `delegation_list`,
  `read_mcp_resource`). No hay forma fiable de partirlo, y OxideGate —como
  proxy— no conoce la lista de servidores MCP del cliente para desambiguar.
- **`pi`** manda los nombres **crudos**, sin prefijo alguno (`read`, `bash`, …).

Resultado: TODAS sus tools caen en `(native)`, ocultando el peso MCP real (en
una petición medida de `opencode`, 20 de 36 tools eran de `context7`/`engram`
pero el desglose las daba como nativas).

`tools_flattened` **avisa de esa opacidad sin fabricar una atribución que no se
puede probar**:

| Valor | Significado |
|---|---|
| `null` | No aplica: Anthropic/Gemini/OpenAI-Chat (su `mcp__` es fiable), o la petición no declaró `tools` |
| `false` | Hay tools Y al menos una usa `mcp__` — el `(native)` de esta fila es de fiar |
| `true` | Hay tools pero NINGUNA usa `mcp__` — el `(native)` **no es verificable**: puede ocultar MCP aplanado |

**Es una observación estructural, no una acusación.** `true` afirma exactamente
el hecho comprobable —"ninguna tool usa el separador `mcp__`"— y NADA más: no
nombra `context7` ni `engram`, no inventa cubos, no adivina. Un consumidor
(`oxidegate-lens`) que lea `tools_flattened: true` debe tratar el `(native)` de
esa fila como "peso de herramientas sin atribuir", no como "herramientas
nativas". El porqué de no intentar la atribución por heurística de nombres está
en el issue #5: para una herramienta de honestidad, misatribuir (`delegation`
como si fuera un servidor MCP) es peor que un `(native)` opaco pero honesto.

---

### 4.5. `client`: el único campo de esta fila que NO es una medición del proxy

Todo lo demás en esta tabla es algo que OxideGate **calculó** a partir del
body (bytes, tokens, latencia) o **decidió** (`cache_control_forced`).
`client` es distinto: es el header `User-Agent`
reenviado **tal cual llegó**, con el único filtro de un tope de 200
caracteres (`middleware::proxy::MAX_CLIENT_LEN`) — sin sanitizar, sin
escapar, sin validar formato. Cualquier proceso que hable HTTP puede mandar
lo que quiera ahí.

Eso tiene dos consecuencias concretas para quien exponga este endpoint:

- **Viaja crudo hasta `GET /requests`.** Sin autenticación de por medio (el
  proxy bindea en `127.0.0.1`, igual que el resto de los endpoints), ese
  string sale exactamente como llegó.
- **Viaja crudo hasta `telemetry.jsonl`, en texto plano.** El campo se
  persiste en disco sin cifrar y sin sanitizar, línea a línea, indefinidamente
  (no hay rotación ni expiración documentada en este slice).

**La tensión con la invariante del §3.** El §3 de este documento dice que
`RecentRequest` "no expone huellas de prompt" y describe el resto de sus
campos como "públicamente inofensivos". Esa descripción es correcta para
`route`, `status`, `upstream` o los campos `context_*`: son propiedades que
el proxy DERIVA del tráfico, no contenido que el cliente eligió mandar en
texto libre. `client` no encaja en esa categoría — es la única excepción, y
este documento prefiere decirlo explícitamente en vez de dejar que la frase
"públicamente inofensivo" del §3 lo cubra por generalización implícita.

En la práctica, el riesgo es acotado (un `User-Agent` no suele llevar datos
sensibles, y el tope de 200 caracteres limita el radio de un log-injection
grosero), pero es un riesgo de una clase distinta al resto de la tabla, y
quien decida exponer `GET /requests` o compartir `telemetry.jsonl` fuera del
host donde corre el proxy debería saberlo antes de hacerlo, no después.

---

### 4.6. `session`: la clave de atribución por sesión

`GET /stats` agrega por `(proveedor, modelo)`; ninguna vista, hasta ahora,
sabía a qué SESIÓN pertenecía cada petición — solo a qué TIPO de harness
(`client`, el `User-Agent`). `session` cierra esa brecha: es la clave de
sesión resuelta por el proxy para cada request, la base de datos real que
necesitan las rebanadas futuras de este eje (agregación por sesión en
`/stats`, panel de sesión en el monitor TUI).

**Forma:**

```json
"session": { "source": "explicit", "key": "mi-sesion-1" }
```

| Campo | Qué es |
|---|---|
| `source` | `"explicit"` \| `"native"` \| `"unattributed"` — qué señal de precedencia ganó la resolución. Fija cómo interpretar `key` |
| `key` | Valor opaco resuelto: el header de atribución crudo (`explicit`/`native`) o el `User-Agent`/constante de fallback (`unattributed`) |

**`source` y `key` viajan siempre juntos, nunca por separado:** una `key` de
`claude-cli/1.2.3` significa cosas opuestas según su `source` — con
`"native"` es una sesión real atribuida por Claude Code; con
`"unattributed"` es solo el `User-Agent` del fallback, NO una identidad.
Por eso `session` nunca es `null`: la precedencia siempre resuelve a algo, y
el peor caso es el bucket `"unattributed"`, un fallback honesto, no una
ausencia.

**Precedencia (de mayor a menor):**

| Orden | Cabecera del request | `source` resultante |
|---|---|---|
| 1 | `X-OxideGate-Session` | `"explicit"` |
| 2 | `x-claude-code-session-id` | `"native"` |
| 3 | (ninguna de las anteriores) | `"unattributed"`, `key` = `User-Agent` crudo, o la constante `"unattributed"` si no hay `User-Agent` legible |

Una cabecera de atribución presente pero vacía se trata como ausente y la
resolución cae al siguiente nivel: `key` nunca es un string vacío en ninguna
rama.

**Invariante de privacidad.** El resolver (`middleware::proxy::session_of`)
lee EXCLUSIVAMENTE esas tres cabeceras. Jamás `Authorization`, `x-api-key` ni
`x-goog-api-key`: `key` es siempre una etiqueta o identificador opaco, nunca
una credencial cruda. Un request con `Authorization: Bearer …` y
`X-OxideGate-Session: mi-sesion-1` simultáneos resuelve `key: "mi-sesion-1"`
— la credencial nunca aparece en ningún campo de la respuesta.

**Cómo estampar el header desde cada harness.** `X-OxideGate-Session` es un
header custom: ningún harness lo manda por defecto, hay que configurarlo
explícitamente por proceso. `x-claude-code-session-id` sí lo manda Claude
Code nativamente, sin configuración adicional.

| Harness | Cómo estampar `X-OxideGate-Session` |
|---|---|
| Claude Code | Variable de entorno `ANTHROPIC_CUSTOM_HEADERS` (formato `Header: valor`, headers múltiples separados por `,`) |
| Gemini CLI | Variable de entorno `GEMINI_CLI_CUSTOM_HEADERS` |
| OpenCode | `options.headers` en la config del proveedor, con interpolación `{env:VAR}` para tomar el valor de una variable de entorno en vez de hardcodearlo |

Sin ninguna de estas configuraciones, el tráfico de esos harnesses cae en
`source: "native"` (Claude Code, vía `x-claude-code-session-id`) o
`source: "unattributed"` (Gemini CLI y OpenCode, que hoy no mandan ningún
header de sesión nativo — caen al `User-Agent`).

---

### 4.7. `codex_quota`: cuánta cuota queda, y por qué nunca son dólares

Todos los demás campos de esta fila salen del **request**: de su body
(`context_*`, `tools_by_server`, `tool_search`) o de sus cabeceras
(`session`, `client`). `codex_quota` es la excepción: se parsea de las
cabeceras de la **respuesta del upstream**
(`middleware::proxy::send_and_meter`, vía `CodexQuota::from_headers`). Es la
única asimetría de dirección de toda la tabla y conviene tenerla presente al
razonar sobre el resto del contrato.

Cuando una petición se enruta al backend de Codex por **OAuth** (plan de
suscripción de ChatGPT, no API key), la respuesta trae doce cabeceras
`x-codex-*` que describen el estado de la cuota. Este campo las transporta
crudas, **sin derivar nada**: ni agregación, ni delta entre filas, ni coste
nocional.

**Forma:**

```json
"codex_quota": {
  "plan_type": "pro",
  "active_limit": "primary",
  "credits_balance": "12.50",
  "primary_used_percent": 4,
  "secondary_used_percent": 12,
  "primary_window_minutes": 300,
  "secondary_window_minutes": 10080,
  "primary_reset_after_seconds": 1800,
  "primary_reset_at": 1732000000,
  "secondary_reset_at": 1732600000,
  "credits_has_credits": false,
  "credits_unlimited": false
}
```

| Campo | Tipo | Cabecera de origen |
|---|---|---|
| `plan_type` | string cruda | `x-codex-plan-type` |
| `active_limit` | string cruda — cuál de las dos ventanas limita hoy | `x-codex-active-limit` |
| `credits_balance` | **string cruda, sin parseo numérico** | `x-codex-credits-balance` |
| `primary_used_percent` | entero | `x-codex-primary-used-percent` |
| `secondary_used_percent` | entero | `x-codex-secondary-used-percent` |
| `primary_window_minutes` | entero | `x-codex-primary-window-minutes` |
| `secondary_window_minutes` | entero | `x-codex-secondary-window-minutes` |
| `primary_reset_after_seconds` | entero | `x-codex-primary-reset-after-seconds` |
| `primary_reset_at` | timestamp unix | `x-codex-primary-reset-at` |
| `secondary_reset_at` | timestamp unix | `x-codex-secondary-reset-at` |
| `credits_has_credits` | booleano | `x-codex-credits-has-credits` |
| `credits_unlimited` | booleano | `x-codex-credits-unlimited` |

`credits_balance` viaja como **string a propósito**: el upstream no garantiza
el formato exacto del saldo (unidades, notación), así que parsearlo a número
sería inventar una precisión que nadie prometió.

**Cuota y dólares no se mezclan — y la separación es estructural.**
`CodexQuota` es un tipo aparte, en un módulo aparte, sin ningún campo en USD.
Es *incapaz por construcción* de alimentar `cost_estimate_usd`. La razón no
es de estilo: la cuota es un **porcentaje de ventana consumida en un plan de
precio fijo**, no un importe. Sumar ambas monedas en un mismo número no sería
una simplificación, sería un error. Un test
(`codex_quota_no_tiene_campo_en_dolares_ni_ruta_a_cost_estimate_usd`) fija
que el JSON serializado jamás contenga `usd` ni `cost`, para que la garantía
no dependa de que nadie se despiste.

**La presencia es la única señal discriminadora.** `from_headers` se dispara
por la presencia de cabeceras `x-codex-*` en la respuesta — **nunca** por la
identidad del upstream ni por el slug del modelo. `api.openai.com` vía API
key comparte el proveedor `openai` pero jamás manda estas cabeceras;
Anthropic y Gemini tampoco. Por eso este campo distingue algo que `upstream`
no puede: **OAuth de suscripción frente a API key de pago por uso**, dentro
del mismo proveedor.

**Dos formas de "no hay dato", y significan cosas distintas:**

| Valor | Significado |
|---|---|
| `codex_quota: null` | Ninguna de las doce cabeceras vino: no es tráfico de Codex por OAuth, o el upstream falló antes de responder |
| `codex_quota: { …, "secondary_reset_at": null, … }` | SÍ es tráfico de Codex con cuota, pero ese campo puntual faltó, llegó vacío o no parseó |

Nunca se devuelve un `CodexQuota` con los doce campos en `null`: esa forma
está reservada para el segundo caso, no para el primero.

**Contrato de saneo, compartido por los doce campos** (mismo criterio que el
resto del proyecto: un dato ausente y un cero real son cosas distintas):

- Cabecera ausente → `null`.
- Cabecera presente pero **vacía** → `null`, nunca `""` ni un `0` fabricado.
  No es hipotético: `x-codex-secondary-reset-at` llega vacía en captura real.
- Valor numérico no parseable → `null`. El parseo **nunca hace `panic`**: un
  dialecto de cabecera malformado no puede tumbar el proxy.
- Booleanos: solo los literales exactos `"True"`/`"False"` (capitalizados,
  tal como los manda Codex) parsean. `"true"`, `"1"` o vacío → `null`.

**Invariante de privacidad.** `codex_quota` no compromete §3: no hay
contenido de prompt en ninguno de los doce campos — solo porcentajes,
duraciones de ventana, timestamps de reseteo y saldo de plan. Tampoco lleva
credenciales: las cabeceras `x-codex-*` describen la cuota, no autentican.

---

### 4.8. `skills`: el listado, atribuido dentro de su bucket

`context_last_turn_bytes` te dice que el último turno pesó 7.103 B.
`skills` te dice que **3.811 de esos son el listado de skills** — el 54%. Es
la misma clase de atribución que `tools_by_server` hace dentro de
`context_tools_bytes`: el total sin desglosar no acciona nada.

**Forma:**

```json
"skills": { "declared": 11, "listing_bytes": 3811, "format": "flat_list" }
```

| Campo | Qué es |
|---|---|
| `declared` | Cuántas ENTRADAS declara el listado — no cuántas skills tienes en disco. Ver el aviso de abajo |
| `listing_bytes` | Bytes del bloque completo. Se pagan en CADA petición, se invoque una skill o no |
| `format` | `flat_list` \| `available_skills_xml` \| `skills_instructions` |

> **`declared` no es "cuántas skills tengo".** El bloque enumera todo lo que el
> modelo puede invocar con la herramienta `Skill`: skills de usuario, de
> plugin, integradas del harness y **slash commands**. Medido: un comando
> aparece en el mismo bloque, con el mismo formato `- nombre: descripción` y
> **sin ninguna marca** que lo distinga.
>
> Separarlos aquí exigiría inventar la distinción — el cable no la trae. Para
> el modelo son todos lo mismo; la diferencia vive en el disco del usuario.
>
> Y en la otra dirección: una skill con `disable-model-invocation: true` **no
> se lista**, así que no cuenta aquí ni cuesta un byte
> (`docs/optimizer-skills.md` §6). Un usuario con 22 directorios de skills
> puede ver perfectamente `declared: 11`.

**`format` identifica la herramienta sin fiarse del `User-Agent`**, que es
contenido controlado por el cliente (§4.5). Cada una manda el listado a su
manera y en un sitio distinto del body — medido en las cuatro, con precios de
138 B a 390 B por skill: ver `docs/skills-across-tools.md`.

| `format` | Herramienta | Dónde cae |
|---|---|---|
| `flat_list` | Claude Code | `context_last_turn_bytes` |
| `available_skills_xml` | Gemini CLI, opencode | `context_system_bytes` / `context_history_bytes` |
| `skills_instructions` | Codex | `context_history_bytes` / `context_last_turn_bytes` |

**Un bloque sólo cuenta si contiene entradas.** No es un tecnicismo: en tráfico
real de opencode la cadena `<available_skills>` aparece **cinco veces** y sólo
UNA es el listado — las otras son el `AGENTS.md` del usuario hablando del
bloque entre comillas. Un detector que se conforme con encontrar la marca
cuenta una mención como si fuera un listado y **inventa skills que nadie
declaró**.

**`null` significa "no se pudo ver", nunca "cero skills".** Las marcas son
cadenas en inglés de cada herramienta: si cambian, el detector deja de
encontrarlas y debe declarar la ausencia. Mismo contrato que el resto de la
fila.

**Dos límites, los mismos que en §4.13** — el recorrido es compartido
(`src/provider/block_scan.rs`). Si el texto del usuario menciona literalmente
`<available_skills>` o `<skills_instructions>` **dentro** de un listado real, ese
listado se salta y sale `null`; si menciona la etiqueta de cierre, la cifra sale
corta. Los dos miden de menos, nunca de más.

**No dobla bytes.** Esos bytes ya los cuentan los campos `context_*` — viven
dentro de uno de sus buckets. `skills` sólo **atribuye**; nunca vuelve a
sumarlos.

---

#### La frontera del listado plano es la línea en blanco

No «la primera línea que no empiece por `- `», que es lo que hacía hasta el
cierre de #84. La descripción de una skill es **contenido** —texto libre del
frontmatter— y puede ocupar varias líneas: bastaba con que una sola lo hiciera
para que la continuación cortara el recorrido y se llevara por delante todas
las entradas posteriores.

Medido sobre captura del 2026-08-09: publicaba **63 entradas de 66** y
**14.902 B de 16.355 (−8,9%)**. Nadie lo notó porque 63 es un número
perfectamente plausible; salió al cruzarlo contra la frontera de `hooks`
(§4.17), que empieza justo donde este listado acaba. Con las dos corregidas,
`hooks.bytes + listado` cubre la parte que los contiene **exactamente**.

Una continuación **suma bytes pero no cuenta como entrada**: `declared` cuenta
skills, no líneas. Y la línea en blanco sigue cerrando el bloque, que es lo que
permite reconocer un listado embebido en un mensaje más largo.

### 4.9. `response_bytes`: la otra dirección, con una advertencia

Hasta ahora toda la contabilidad en bytes era de SUBIDA. `response_bytes`
cierra la otra mitad: cuántos bytes de cuerpo bajaron del proveedor al cliente.

Se acumula en el mismo recorrido que ya hace `MeteredBody` para sacar el TTFT y
el `usage` — ni un segundo pase ni bufferizar la respuesta entera.

**La advertencia, y hay que leerla antes de usar el campo.**

> **Son bytes SIN COMPRIMIR.** El proxy descarta `Accept-Encoding` a propósito
> (ver `middleware::proxy`): si dejara que el proveedor comprimiera la
> respuesta, el escáner SSE leería bytes comprimidos y no podría extraer el
> `usage`. **Sin el medidor en el camino, el cliente habría recibido esta misma
> respuesta comprimida.**

Así que `response_bytes` mide el **tamaño del contenido que bajó**, no los
bytes que se habrían pagado en la red sin proxy. Para texto SSE la diferencia
no es menor: la compresión sobre texto repetitivo es agresiva.

Es el mismo tipo de contaminación que ya documenta
[`docs/optimizer-tool-search.md`](optimizer-tool-search.md) §3 con los
esquemas MCP —*parte de lo que se ve existe porque el medidor está en el
camino*— solo que en la dirección contraria: aquí el medidor **quita** una
optimización que sin él estaría, y la bajada se ve más grande de lo que sería.

#### La asimetría de las dos direcciones

Las dos direcciones están medidas y publicadas —`prompt_bytes` desde #51,
`response_bytes` desde #22— pero **no miden lo mismo**, y el parecido de los
nombres invita a tratarlas como si sí:

| | Qué mide de verdad |
|---|---|
| `prompt_bytes` | El body **lógico** que compuso el cliente. Excluye el framing HTTP. En `/v1/codex/responses` y `/v1beta/*` se mide **descomprimido**: si el cliente comprimió, por el cable subió menos (en `pi` con zstd, 3x menos). Y se toma **antes** de mutar, así que con `cache_control_forced` al proveedor sube más que este número |
| `response_bytes` | Los bytes de cuerpo que **cruzaron** el proxy, **sin comprimir** porque el medidor quitó la compresión que sin él habría estado |

Una es *«lo que el cliente compuso»*; la otra, *«lo que pasó por el cable con
la compresión desactivada por nuestra culpa»*. **Ninguna de las dos es wire.**
Y las dos se desvían del wire **en direcciones opuestas**: la subida real por
el cable es MENOR que `prompt_bytes` cuando el cliente comprime, y la bajada
real sería MENOR que `response_bytes` si no hubiéramos quitado la compresión.

**Por eso el cociente subida/bajada no es una ratio de ancho de banda.** No lo
publiques sin declarar sus dos términos. Cuando `prompt_bytes` solo existía en
el JSONL esto era una recomendación; con los dos campos en la misma fila de la
API pública, es una obligación.

#### Y una razón más fuerte, medida: la caché los desacopla

Aunque se declaren los términos, hay un cociente que deja de significar lo que
aparenta en cuanto la caché entra en juego: **euros por byte de subida**.

Medido sobre 904 peticiones reales, con caché activa los **bytes subidos y los
tokens que se pagan están DESACOPLADOS**: el `input_tokens` no cacheado es
esencialmente impredecible desde los bytes del body (APE ~100%). En el cohorte
medido, el **54,0% de 89.743.537 tokens de entrada** llegó cacheado, a tarifa
10%. Dos peticiones con los mismos `prompt_bytes` pueden costar diez veces
distinto según qué parte del prefijo estuviera caliente.

No es una advertencia sobre precisión: es que la magnitud del denominador deja
de gobernar el numerador. Para atribuir coste, la pieza que manda es qué
sección cayó dentro del prefijo cacheado —ver §4.11—, no cuántos bytes subieron.

**`null` significa "no hubo respuesta que recorrer"**, nunca "el proveedor
devolvió un cuerpo vacío". Un `0` fabricado ahí confundiría un fallo de
conexión con una respuesta vacía legítima. Si el stream se cortó a mitad,
`response_bytes` lleva lo que SÍ cruzó —una medición real de un cuerpo
parcial— y es el `status` de la fila quien dice que hubo error.

---

### 4.10. `prompt_bytes`: la otra mitad, y tres cosas que NO es

`response_bytes` (§4.9) cerró la bajada. `prompt_bytes` cierra la subida: los
bytes del body que **mandó el cliente**.

Estuvo omitido de este endpoint desde el principio, junto a `prompt_hash`,
pero por un motivo distinto: el hash por la invariante de privacidad (§3), y
`prompt_bytes` porque era *«un detalle de implementación que no aporta a
detectar outliers»*. Ese razonamiento se escribió cuando esta vista servía
solo para cazar filas atípicas. Desde que `/requests` es el contrato público
del ecosistema (§8) y publica `response_bytes`, la asimetría dejaba a un
consumidor **pudiendo responder cuántos bytes bajaron y no cuántos subieron**.

No compromete la invariante: es un entero, no identifica ningún prompt.
`prompt_hash` sigue sin salir de aquí.

**Ahora las tres advertencias, y ninguna es menor.**

#### 1. No es el tamaño de wire

Depende de la ruta:

| Ruta | Se mide sobre |
|---|---|
| `POST /v1/messages` | `incoming.body` crudo |
| `POST /v1/chat/completions`, `POST /v1/responses` | `incoming.body` crudo |
| `POST /v1/codex/responses` | body **descomprimido** (`maybe_decompress`) |
| `POST /v1beta/*` | body **descomprimido** (`maybe_decompress`) |

En las dos últimas se mide el JSON **lógico**, no el que viajó. Y no es
teórico: **`pi` manda el body en zstd**. Para ese tráfico, por el cable
subieron bastantes menos bytes de los que dice este campo.

Se mide así a propósito —medir sobre el wire comprimido daría un
`prompt_hash` y un `context` inútiles— pero significa que **no se puede
sumar tráfico de rutas distintas y llamarlo «bytes subidos»** sin decir cuál
es cuál. Además, en todas las rutas excluye el framing HTTP.

#### 2. No es lo que subió al proveedor

Se calcula sobre el body **ORIGINAL**, antes de cualquier mutación. Con la
Palanca A activa, OxideGate inyecta un breakpoint de `cache_control` y **el
body reenviado es MAYOR que este número**.

Quien delata la intervención es `cache_control_forced`: si viene `true`, esta
fila mide lo que el cliente mandó, no lo que el proxy reenvió. Una medición
sobre un body que el propio medidor alteró y no lo dijera sería el peor fallo
posible en este proyecto — por eso ese booleano ya estaba, y por eso hay que
leerlo junto a este campo.

#### 3. No es la suma del desglose

`context_measured_bytes` es JSON canónico **re-serializado**; `prompt_bytes`
es el body tal como llegó. Son dos mediciones de cosas relacionadas tomadas
en puntos distintos del pipeline, y §4.1 ya prohíbe combinarlas en un
cociente. Publicar las dos juntas hace ese error mucho más fácil de cometer,
así que conviene repetirlo: **`context_measured_bytes / prompt_bytes` no
significa nada estable.**

Un test (`los_bytes_de_subida_no_son_la_suma_del_desglose`) fija que no se
exige que coincidan: si alguien «arreglara» la diferencia igualándolas,
estaría falseando una de las dos.

#### Y tampoco lo dividas por `response_bytes`

El contrato completo de esa asimetría vive en **§4.9**, que es donde están las
dos columnas enfrentadas y la razón medida por la que la caché desacopla bytes
de tokens. En corto: un cociente subida/bajada mezcla **contenido lógico de
subida** con **contenido sin comprimir de bajada**, y las dos se desvían del
wire en direcciones opuestas. Útil si se declaran los términos, engañoso si no.

### 4.11. `cache_by_section`: el único campo estimado, y por qué va aparte

Todo lo demás en esta fila es medición. Esto no. Va en un objeto anidado
justamente para que esa frontera se vea en la ESTRUCTURA y no solo aquí.

#### Qué problema resuelve

Un token leído de caché cuesta el **10%** de la tarifa. El prefijo que se
cachea es justo el estable —`system + tools + history`—, así que un reparto de
coste por sección que ignore qué sección estaba cacheada se equivoca por un
factor cercano a diez, y se equivoca precisamente en los cubos que más pesan.
Medido: **el 54,0% de 89.743.537 tokens de entrada** del cohorte estaban
cacheados.

El efecto sobre el reparto, en `anthropic/claude-opus-4-8` (n=133):

| Sección | Cuota de BYTES | Cuota de lo PAGADO | Delta |
|---|---:|---:|---:|
| `tools` | 56,1% | 22,5% | **−33,6 pt** |
| `system` | 4,0% | 2,1% | −1,9 pt |
| `history` | 31,9% | 43,5% | +11,6 pt |
| `last_turn` | 7,8% | **31,1%** | **+23,4 pt** |
| `other` | 0,2% | 0,7% | +0,5 pt |

Leído en bytes, `tools` domina. Leído en lo que de verdad se paga, `tools` cae
a menos de la mitad y **el turno nuevo del usuario multiplica por cuatro su
peso**. Misma dirección, menor magnitud, en `codex/gpt-5.5` (`last_turn`
+5,7 pt) y `openai/gpt-5.5` (+3,8 pt).

#### Cómo se calcula: paseo por el prefijo

El caché hace *prefix match*: la región cacheada es un prefijo CONTIGUO del
prompt. `cache_read_tokens` es **autoritativo** —lo reporta el proveedor—, así
que el método no PREDICE la frontera: la convierte a una posición en bytes con
la tasa plana de la propia petición y consume secciones en orden
`tools → system → history → last_turn → other`.

La contabilidad se toma del **upstream**, no del modelo: en Anthropic la caché
va aparte del `input_tokens` reportado y hay que sumarla; en OpenAI y Gemini ya
está dentro. Usar la fórmula equivocada desplaza la frontera y atribuye a la
sección que no es.

#### El falsador, publicado a propósito

`last_turn_cached_bytes` **debería ser 0 casi siempre**: el último turno es
contenido nuevo, no puede venir de una caché anterior. Que ese campo se dispare
de forma sostenida es la señal de que el método dejó de describir el tráfico.
Está publicado para que se pueda vigilar desde fuera, no escondido.

Sobre 2.647 peticiones reales el falsador no dispara: 0,0% en `codex/gpt-5.5`
(n=356), 0,4% en `openai/gpt-5.5` (n=281) y 6,0% en `anthropic/claude-opus-4-8`
(n=133), este último con desbordamiento p95 de solo 0,051 — consistente con el
±10% de error de la tasa tokens/byte.

#### Límites declarados de `prefix_walk_v1`

- **Solo se pasea `cache_read`, no `cache_write`.** La lectura se factura al 10%
  (error de ~10x si se ignora); la escritura al 125% (error de 1,25x). El dinero
  está en la lectura. Atribuir también la escritura es la evolución natural.
- **Un solo orden de prefijo** para todos los proveedores. Anthropic lo
  documenta; en OpenAI y Gemini es una hipótesis que el falsador no ha
  conseguido tumbar, no una lectura de sus specs.
- La conversión tokens→bytes usa la **tasa plana** de la petición. Las tasas por
  sección quedan en 0,90x–1,06x dentro de un mismo modelo — pero **nunca mezcles
  modelos** para estimarlas: agregar modelos fabrica un sesgo por sección que no
  existe.
- `n=10` en `claude-haiku-4-5` es demasiado poco para concluir nada; ahí el
  falsador dispara al 20% y no se ha investigado.

#### Qué NO hacer con este campo

- **No lo pintes en la misma columna que los `context_*_bytes`.** Uno se mide,
  el otro se deduce.
- **No publiques euros por sección a partir de esto sin decir que es una
  estimación.** Es el único error de este contrato que sería irreversible: en
  cuanto una lente lo pinte en euros, nadie vuelve a leer la letra chica.
- **No ignores `method`.** Va dentro del objeto para que un consumidor pueda
  decidir si entiende el algoritmo ANTES de dibujar. Si el sufijo cambia,
  cambió cómo se calcula.

---

### 4.12. `input_share_by_section`: el reparto de lo que se paga

`context_*_bytes` dice cuánto **pesa** cada sección. No dice cuánto **cuesta**,
y medido son cosas muy distintas.

#### Por qué no lo puede calcular el consumidor

Para repartir el input pagado hacen falta tres cosas:

1. los bytes por sección — publicados,
2. los bytes cacheados por sección — publicados (§4.11),
3. **el multiplicador de lectura de caché del modelo** — *no publicado*.

El tercero no está, y no puede estarlo sin publicar la tabla de precios entera.
Por eso este campo existe: es la única forma de dar la respuesta sin dar la
tabla.

Y el multiplicador **es del modelo, no del proveedor**: dentro de OpenAI, la
familia 4o lee caché al 0,5 y la familia 5 al 0,1. El mismo reparto de bytes da
resultados distintos según el modelo, así que elegir un multiplicador por
defecto sería inventar el número.

#### Cómo se calcula

```
peso_i  = (bytes_i − cacheados_i) + cacheados_i × M
share_i = peso_i / Σ peso
```

La **tarifa** del modelo se cancela al dividir, así que el reparto no depende
del precio absoluto — solo de la proporción entre cacheado y no cacheado. Eso
lo hace estable frente a un cambio de tarifa.

#### No lleva `cost` en ningún sitio, y es deliberado

El [issue #50](https://github.com/pichu2707/OxideGate/issues/50) marca esto como
**el único error irreversible del contrato**: *«en cuanto una lente lo pinte en
euros, nadie vuelve a leer la letra chica»*.

Son fracciones de 0 a 1. No llevan moneda y no se pueden pintar como euros sin
multiplicarlas por algo — y ese algo obliga a ir a buscar `cost_estimate_usd` y
a leer qué es. Hay un test que falla si alguna clave publicada contiene `cost` o
`usd`.

#### Qué NO incluye

**Solo el input.** El output no se reparte porque no pertenece a ninguna sección
del contexto: es lo que el modelo generó, no lo que se le mandó. Un reparto que
lo incluyera atribuiría a `tools` una parte de algo que `tools` no causó.

#### Cuándo es `null`

Es una estimación **sobre otra estimación**, así que hereda todos los huecos de
§4.11 y añade uno propio:

- sin desglose de contexto,
- **sin `cache_by_section`** — y aquí no se reparte por bytes como consuelo: ese
  es justo el reparto que la medición desmiente,
- **sin el modelo en la tabla de precios** — no se conoce el multiplicador,
- con `measured_bytes` a cero.

---

### 4.13. `instructions`: el bloque más caro, que hasta ahora no se veía

Tu `CLAUDE.md` es, medido, el **48% del peaje fijo** de una sesión de Claude
Code (`docs/fixed-toll-claude-code.md` §1) — el doble que todas las skills
juntas. Era el único de los tres bloques del peaje sin campo propio: sus bytes
viajan en `messages[0]`, y como `history = messages[:-1]`, caían dentro de
`context_history_bytes` mezclados con toda la conversación. De ahí no se podían
sacar.

Importa porque el catálogo publica una palanca **ya medida** sobre este bloque
—`CLAUDE.md` lean ⇒ −29.509 B/petición, la mayor de las cinco
(`docs/optimizer-claude-md.md`)— y el proxy no medía el objeto al que esa
palanca se aplica.

**Forma:**

```json
"instructions": { "bytes": 33716, "format": "claude_md" }
```

| Campo | Qué es |
|---|---|
| `bytes` | Bytes del bloque completo, envoltorio incluido. Se pagan en CADA petición |
| `format` | `claude_md` \| `codex_agents_md` \| `opencode_agents_md` — ver abajo |

#### El bloque se delimita por su ENVOLTORIO, nunca por una cabecera

Medido sobre una captura real a coste cero (Claude Code 2.1.220, cuerpo de
188.180 B):

| Corte | Bytes | |
|---|---:|---|
| `<system-reminder>`…`</system-reminder>` que contiene `# claudeMd` | **33.716** | lo que se publica |
| desde `# claudeMd` hasta la siguiente cabecera `# ` | 8.254 | **falso** |

El corte por cabecera se para en `# Agent Teams Lite — Orchestrator
Instructions`, que es una cabecera **del propio `CLAUDE.md` del usuario**: da el
24% de la cifra real y tiene toda la pinta de un número. El contenido del bloque
es markdown arbitrario escrito por una persona, así que **ninguna cabecera puede
servir de frontera**. Es el mismo error que `docs/fixed-toll-claude-code.md` §4
ya documentaba en prosa; ahora tiene número.

#### Y encontrar el envoltorio tampoco basta

En ese MISMO cuerpo, `<system-reminder>` aparece en otros dos sitios:
`$.system[2].text` (9.588 B, **abierto y nunca cerrado**) y
`$.tools[0].description` (1.582 B, la descripción de la herramienta `Agent`
mencionando la etiqueta). Por eso un bloque **sólo cuenta si contiene la marca
interna** — el mismo criterio que hace que una mención de `<available_skills>`
no sea un listado (§4.8), y por el mismo motivo: las marcas son cadenas en
inglés que también aparecen en lo que escribe el usuario.

Y por eso el recorrido va **cadena a cadena sin concatenar**: uniendo
`system[2]` con `messages[0]` se fabricaría un bloque cerrado que en el cable no
existe.

#### `null` no significa "el usuario no tiene instrucciones"

Significa "no se reconoció ningún bloque". Hay un caso real y perfectamente
correcto: **Claude Code ignora `AGENTS.md`** — `null` con ese fichero en el
proyecto es la respuesta buena, no un fallo del detector. El mismo fichero,
cuatro comportamientos (`docs/skills-across-tools.md` §6).

#### Tres `format`, y cada uno con su captura detrás

| `format` | Herramienta | Envoltorio | Cierre | Ruta |
|---|---|---|---|---|
| `claude_md` | Claude Code 2.1.220 | `<system-reminder>` con `# claudeMd` dentro | sí | — |
| `codex_agents_md` | Codex 0.142.5 | cabecera + `<INSTRUCTIONS>`…`</INSTRUCTIONS>` | sí | **absoluta** |
| `opencode_agents_md` | opencode 1.18.15 | `Instructions from: <ruta absoluta>` | **no** | **absoluta** |

Cada dialecto entra **cuando tiene captura propia**, nunca desde una tabla. Y el
motivo dejó de ser teórico: la marca que se documentaba para **Codex**
—`--- project-doc ---`— **no existe** en 0.142.5. `grep` devuelve cero sobre la
captura real; la de verdad es `# AGENTS.md instructions for <ruta>`. La de
opencode, en cambio, **sí sobrevivió** a diez versiones de deriva.

Una de dos. Escribir ambos detectores desde la tabla habría dejado uno roto
publicando `null` — y `null` es un valor legítimo en este campo, así que nadie
lo habría notado.

Falta **`pi`**, que además manda el cuerpo en zstd: sus cifras serán lógicas y
habrá que decirlo. Ver #66 y
[`banco-de-captura.md`](banco-de-captura.md) para el método.

#### La cabecera de Codex va FUERA del envoltorio, y se cuenta

```text
# AGENTS.md instructions for /ruta/absoluta/proyecto

<INSTRUCTIONS>
…contenido del AGENTS.md…
</INSTRUCTIONS>
```

Medir solo de `<INSTRUCTIONS>` a `</INSTRUCTIONS>` dejaría fuera la ruta, que
son **116 de los 178 B** de envoltorio. El bloque se mide **desde la cabecera**.

Y de nuevo: el «+159 B» que circulaba para Codex **no es una constante**, es
`62 B + longitud de la ruta`.

#### El envoltorio de opencode depende de DÓNDE tengas el proyecto

Medido sobre captura real de opencode 1.18.15 con un `AGENTS.md` de **202 B**:

| Parte | Bytes |
|---|---:|
| Bloque completo | 349 |
| Cabecera `Instructions from: ` | 19 |
| **Ruta absoluta** | **126** |
| **Envoltorio total** | **147** |

**El 86% del envoltorio es la ruta del proyecto.** Así que el «+160 B» que
circulaba para opencode **no es una constante**: es `~21 B + longitud de la
ruta`. Un proyecto en `/home/u/p` paga bastante menos que uno en un directorio
profundo, y ese número no se puede comparar entre máquinas sin decir dónde
estaba el proyecto. Mismo hallazgo que con Codex.

#### opencode abre el bloque y no lo cierra

A diferencia de Claude Code, opencode no envuelve: pone la marca y pega el
contenido. Lo único que hay detrás es su bloque de skills.

```text
</env>
Instructions from: /ruta/absoluta/AGENTS.md
…contenido del AGENTS.md…

Skills provide specialized instructions and workflows for specific tasks.
```

Así que la frontera final es el preámbulo de skills, y **si no se encuentra, el
campo es `null`**. Correr hasta el final del texto se tragaría el listado de
skills entero — el error que §4 de
[`fixed-toll-claude-code.md`](fixed-toll-claude-code.md) documenta tras
cometerlo dos veces.

Esa frontera es **prosa del harness**, y es una fragilidad conocida: si opencode
cambia la frase, el campo pasa a `null`. Falla honesto, no falla mintiendo.

Y por eso `claude_md` se prueba **primero**: su bloque tiene apertura y cierre
reales, así que ante un cuerpo donde los dos pudieran aparecer gana el que se
puede delimitar con certeza.

#### Dos avisos sobre la cifra

- **No es el tamaño del fichero en disco.** El harness añade su envoltorio, y el
  `CLAUDE.md` de proyecto y el global viajan concatenados en el mismo bloque.
  Esto es lo que sube por el cable, que es lo que se factura.
- **No es el 33.718 del documento.** `docs/fixed-toll-claude-code.md` §1 es de
  otra captura (2026-07-31, cuerpo de 183.861 B); esta cifra es de la captura
  del 2026-08-07 (cuerpo de 188.180 B), donde la parte de texto entera mide
  33.718 B y el bloque delimitado por las marcas mide 33.716 B —los 2 B son el
  `\n\n` que queda fuera del cierre. Que la captura antigua coincida con esa
  cifra es casualidad, no una relación entre los dos documentos.

#### Límites conocidos, declarados

Los dos vienen de que el contenido del bloque es **texto libre del usuario**, y
los dos van en la misma dirección: miden de MENOS o declaran ausencia, nunca de
más.

1. **Si tu `CLAUDE.md` menciona literalmente `<system-reminder>`** —la etiqueta
   de APERTURA— ese bloque se **salta entero**, y el campo puede salir `null`.
   El recorrido ve dos aperturas y ninguna que el cierre final cierre de forma
   inequívoca, así que no se queda con ninguna. Distinguir "apertura mencionada"
   de "apertura sin cerrar" exigiría inventarse una gramática del contenido.
2. **Si menciona la etiqueta de CIERRE**, `</system-reminder>`, el recorrido
   para ahí y la cifra sale **corta**.

Medir de menos en un caso raro y decirlo es honesto; medir de más en silencio,
no. Y sobremedir es un riesgo real, no hipotético: una versión anterior de este
recorrido fusionaba una apertura sin cerrar con el bloque siguiente y publicaba
123 B donde había 67. Lo encontró una revisión adversarial, no el tráfico.

**No dobla bytes.** Ya los cuenta `context_history_bytes`. `instructions` sólo
**atribuye**; nunca vuelve a sumarlos.

#### 4.13.1. `by_heading`: dice de QUÉ es ese número

`instructions.bytes` publicaba 33.777 B y ahí se acababa. Con eso no se puede
decidir nada: la pregunta accionable no es *cuánto pago* sino **qué parte de lo
que pago siempre la uso siempre** — que es literalmente lo que `GET /mcp`
contesta para los servidores MCP.

`by_heading` reparte esos bytes entre las cabeceras markdown del contenido.
Medido sobre un `CLAUDE.md` real de 33.460 B:

| Sección | Bytes | % |
|---|---:|---:|
| Model Assignments | 9.827 | **29,3%** |
| SDD Workflow | 9.392 | **28,0%** |
| Agent Teams Orchestrator | 5.967 | 17,8% |
| Engram Protocol | 3.857 | 11,5% |
| **Subtotal** | **29.043** | **86,6%** |

Cuatro secciones son el 87%, y las cuatro son protocolo de un flujo concreto que
viaja en cada petición de cada sesión, se use o no.

**Forma:**

```json
"instructions": {
  "bytes": 33507,
  "format": "claude_md",
  "by_heading": [
    { "kind": "preamble", "level": null, "bytes": 18,   "heading": null },
    { "kind": "heading",  "level": 2,    "bytes": 9827, "heading": null }
  ]
}
```

##### Esto NO contradice «las fronteras las pone el envoltorio»

`docs/optimizer-claude-md.md` documenta que cortar por cabecera midió **8.254 B
de 33.716 reales**: se paró en una cabecera del propio fichero del usuario. De
ahí la regla del proyecto.

La regla sigue intacta, y la distinción es toda la clave:

- **La frontera del BLOQUE** la pone el envoltorio del harness
  (`<system-reminder>`…`</system-reminder>`). Eso no se toca.
- **El reparto INTERIOR** usa las cabeceras del usuario, que son su propia
  estructura. No se busca dónde acaba el bloque: se reparte lo que hay dentro.

El error de entonces fue usar el contenido para **delimitar**. Aquí el contenido
solo **reparte** lo ya delimitado.

##### Los nombres NO viajan por defecto

`heading` es `null` salvo que se ponga `OXIDEGATE_INSTRUCTIONS_HEADINGS=on`, y el
proxy lo anuncia al arrancar cuando está activa.

El precedente de `tool_names` **no aplica**. Aquellos nombres los eligió el
cliente y ya viajaban al proveedor en el mismo body, así que publicarlos no
añadía exposición. Una cabecera de `CLAUDE.md` la escribió una persona, puede
llevar nombre de cliente o de proyecto, y además de viajar al proveedor acaba en
`telemetry.jsonl` en claro y en `GET /requests` — donde alguien la mira sin
haberla pedido.

Quitar el nombre **no quita el dato**: bytes, nivel y posición siguen ahí, y con
el fichero delante se sabe qué fila es cuál.

##### Nivel ≤2, y el nivel 3 no es «más detalle»

Bajar a `###` lleva el mismo fichero de 21 filas a 44 y **destruye la señal**:
«Model Assignments» pasa de 9.809 B (29,3%) a **1.705 B (5,1%)** porque su
contenido se reparte entre hijos. La fila que hay que ver deja de existir.

##### La invariante: las filas suman el bloque, siempre

Igual que `group_tools_by_server`. Lo sostienen dos piezas:

- **`preamble`** se lleva todo lo anterior a la primera cabecera — incluido el
  envoltorio del harness. Es lo que hace que cuadre sin restar.
- **`others`** recoge el desborde del cupo (`MAX_INSTRUCTIONS_HEADINGS`, 32) y
  **sigue contando sus bytes**: se pierde el desglose fino, nunca un byte.

Un fichero sin cabeceras da **una sola fila** de `preamble` con el bloque
entero, y esa es la lectura correcta: «esto no está dividido».

##### Hallazgo: la marca del harness es una cabecera más

`# claudeMd` **es una cabecera markdown de nivel 1**, así que sale como fila. Le
pasa igual a Codex, cuya marca es `# AGENTS.md instructions for <ruta>`.

No se filtra, y es deliberado: filtrarla exigiría que el reparto supiera qué
líneas puso el harness — justo la dependencia del contenido que este módulo
evita en la frontera. Se publica el `level` para que un consumidor decida por su
cuenta, y se dice aquí que las primeras filas suelen ser andamiaje.

##### No se publica el porcentaje

Un consumidor divide por `bytes`. Publicar la fracción ya cocinada añadiría un
campo que puede desincronizarse con los bytes sin que nada lo note — mismo
criterio por el que `energy_idle_wh` se publica al lado y no restado.

##### Verificado solo en `claude_md`

El reparto es markdown puro, así que **debería** valer igual para
`codex_agents_md` y `opencode_agents_md`. No se afirma que valga: la regla de
#66 es que ningún dialecto entra sin captura propia, y las capturas de #88 no
están en el árbol (`capturas/` está en `.gitignore`). Hay tests de forma para los
tres dialectos; medición contra tráfico real, solo del primero.

##### No mueve `CONTRACT_VERSION`

Clave nueva dentro de un objeto que ya existía: aditivo. `/version` la declara en
`fields` como `by_heading` —con su nombre literal, porque la comprobación recorre
el JSON en profundidad— para que una lente pueda sondear la capacidad sin
comparar versiones. Ver §8.2.

---

### 4.14. `effort_forced`: la única fila donde el medidor confiesa

`requested_effort` (§4) dice qué nivel de esfuerzo **pidió el cliente**.
`effort_forced` dice cuál **impuso el proxy**, o `null` si no intervino — que es
el caso por defecto, porque la palanca B arranca apagada.

```json
"requested_effort": "high",
"effort_forced": "low"
```

Esa fila cuenta la historia entera: el cliente pidió una cosa, el proxy mandó
otra, y **los `output_tokens` que se midan en esa fila son del segundo**.

#### Por qué el campo tiene que existir

Es la única defensa contra el peor fallo que este proyecto puede cometer:
**medir un ahorro que causó el propio medidor y presentarlo como del cliente.**

Sin este campo, dos filas con `output_tokens` distintos serían indistinguibles
entre «el cliente cambió algo» y «el proxy mutó el body». Toda la telemetría
anterior quedaría bajo sospecha en cuanto alguien encendiera la palanca una vez.

`requested_effort` se lee **antes** de mutar, precisamente para que el par
funcione. Los dos campos juntos, o ninguno.

#### Por qué guarda un `String` y `cache_control_forced` un `bool`

Porque aquí hay algo que declarar. La palanca A inyecta siempre el mismo valor
fijo (`{"type": "ephemeral"}`) y basta con saber que ocurrió. La B impone un
nivel que **el lector no puede deducir de la fila**: sin el valor, sabrías que
te intervinieron pero no en qué dirección. Una fila que no es autocontenida
obliga a ir a buscar la configuración del proxy de aquel momento, que es
justamente lo que nadie va a hacer.

#### `null` significa "el proxy no intervino"

Y cubre cuatro casos que se distinguen entre sí mirando el resto de la fila:

| Caso | Cómo se reconoce |
|---|---|
| La palanca está apagada (default) | Todas las filas con `null` |
| El cliente ya pedía ese nivel | `requested_effort` == el nivel configurado |
| `output_config` presente y no-objeto | El body no es el dialecto esperado |
| Dialecto sin `effort` | `upstream` es `openai` o `gemini` |

Los tres últimos no son fallos: son el proxy prefiriendo **no tocar** a romper
el request. Ver `docs/optimizer-effort.md` §5.

#### No cambia lo que ya medías

`effort_forced` no altera ningún otro campo. `prompt_bytes` sigue siendo el body
**original** —se calcula antes de mutar, como con `cache_control_forced`— y los
`context_*_bytes` también. Lo único que cambia respecto a una fila sin palanca
es que el body que subió al proveedor llevaba otro `effort`, y la fila lo dice.


### 4.15. `tools_invoked`: lo que el modelo USA, frente a lo que el cliente DECLARA

Hasta este slice, el proxy sabía exactamente qué herramientas **se declaran**
(`tools_by_server[].tool_names`, §4.2) y absolutamente nada de cuáles **se
usan**. Es un hueco caro: `mcp-lean.json` es la palanca más grande del
catálogo publicado —**−55.098 B por petición**— y no hay forma de recomendarla
sin saber qué servidor sobra.

```json
"tool_calls": {
  "invoked": [
    { "name": "Read",                    "server": "(native)", "kind": "native" },
    { "name": "mcp__context7__get-docs", "server": "context7", "kind": "mcp"    },
    { "name": "buscar",                  "server": "remoto",   "kind": "mcp"    }
  ],
  "server_invoked": ["web_search"],
  "invoked_total": 3,
  "server_invoked_total": 1,
  "complete": true
}
```

**Cada invocación lleva su servidor RESUELTO, no solo el nombre.** Es lo único
que hace fiable el cruce, porque hay dos casos donde el nombre no basta:

- El **conector MCP de Anthropic** (`mcp_tool_use`) manda el nombre desnudo
  —`buscar` en el ejemplo— y el servidor en un campo hermano. Deducirlo del
  nombre lo mandaría a `(native)` el 100% de las veces, y el servidor real
  aparecería sin usar.
- El nombre se **acota a 128 caracteres** al guardarlo. Deducir el servidor del
  nombre ya recortado partiría un `mcp__<server-largo>__<tool>` antes de su
  segundo `__`.

Ambos fallos publicarían `unused` para un servidor en uso, que es el peor
error que este dato puede provocar.

> **Cambio de forma respecto a la primera versión de este campo.** `invoked`
> empezó siendo un array de strings. Ninguna release lo publicó así —entró en
> `main` después de la v0.12.0 y se corrigió antes de la siguiente— así que
> `CONTRACT_VERSION` sigue en 1: no hay ningún consumidor que pudiera haberse
> escrito contra la forma vieja desde una versión publicada.
>
> Aun así, las filas ya escritas en `telemetry.jsonl` con la forma antigua
> **siguen entrando**: `ToolCall` acepta las dos al deserializar y, al leer un
> string suelto, deriva el servidor con `provider::classify` — que es
> exactamente lo que hacía el consumidor de entonces. Sin esa tolerancia,
> `serde` fallaba al parsear la fila ENTERA y la rehidratación perdía también
> sus tokens, coste y latencia.

Cruzar esos nombres con `tools_by_server` sobre el histórico es lo único que
permite escribir la frase que justifica la palanca:

> Pagas 12.400 B por petición por el servidor `context7` y no has invocado
> ninguna de sus 8 herramientas en 200 peticiones.

Nadie más puede escribirla. Hace falta tener los bytes por servidor y las
invocaciones reales **en el mismo punto**, y ese punto es el proxy.

#### El nombre llega entero; solo el argumento viaja troceado

En streaming, cada bloque de contenido abre con un evento propio, y el de una
invocación trae el nombre **completo**:

```
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":
       {"type":"tool_use","id":"toolu_01T1x…","name":"get_weather","input":{}}}
```

Lo que llega partido entre eventos `input_json_delta` es el `input` — y este
campo **no lo mide**, ni lo necesita: para saber si un servidor MCP se usa
basta con QUÉ se invocó, no con qué argumentos. Esa asimetría es la que hace
que la captura no requiera reensamblar nada.

En no-streaming la misma información está en el array `content` de la
respuesta completa. El proveedor cubre las dos formas en un solo método
(`Provider::extract_tool_use`), igual que ya hacía con el `usage`.

#### No cuesta un recorrido más

El escáner que ya leía el `usage` de cada evento (`UsageScanner`) parsea el
JSON **una vez** y ahora se lo pasa a los dos extractores. No hay un segundo
recorrido del stream, este campo no bufferiza nada que el escáner de `usage`
no bufferizara ya, y no se retrasa un solo byte al cliente: la promesa de passthrough intacto se mantiene porque este
campo se lee del `Value` que ya existía.

#### Por qué crudos y con repeticiones

Los nombres van **tal cual viajan en el cable**, sin agregar por servidor:

- **Crudos** porque son el mismo string que aparece en `tools[]`, así que la
  atribución a servidor la deriva quien lea (`provider::classify`). Publicar
  un agregado `{server, calls}` fosilizaría la convención `mcp__<server>__<tool>`
  del día en que se escribió: si mañana cambia, las filas viejas quedarían mal
  agregadas para siempre, mientras que los nombres crudos seguirían siendo
  reinterpretables.
- **Con repeticiones** porque cuántas veces se llamó a una herramienta es un
  dato real de la respuesta. Deduplicar al escribir lo perdería, y el
  histórico es justo lo que consumirá el recomendador.

#### Dos listas, no una

`server_tools_invoked` recoge los bloques `server_tool_use` (`web_search`,
`web_fetch`), que llegan con IDs `srvtoolu_` y los ejecuta el proveedor. Van
aparte **a propósito**: no salen de la configuración MCP del usuario, así que
sumarlas inflaría el «sí lo usas» de un servidor MCP con llamadas que no son
suyas. Se pueden contrastar contra `usage.server_tool_use`, que Anthropic
reporta por su cuenta.

#### Tres formas de que la lista sea corta, y cómo distinguirlas

Una lista corta puede significar tres cosas muy distintas. **El recomendador
tiene que separarlas o acabará aconsejando borrar servidores que sí se usan**,
y por eso el campo lleva metadatos en vez de ser dos vectores pelados:

| Qué pasó | Cómo se ve en la fila |
|---|---|
| El modelo invocó poco — el caso honesto | `complete: true`, `invoked_total == invoked.len()` |
| **Este proveedor no tiene extractor** | `tool_calls` es **`null`**, no un objeto con listas vacías |
| **El escaneo se cortó** (turno abortado, stream roto) | `complete: false` — las listas son un PREFIJO |
| **La lista se truncó** por el cupo | `invoked_total > invoked.len()` |

**`null` no es lo mismo que listas vacías.** `null` dice "aquí no se midió";
un objeto con `invoked: []` dice "se escaneó la respuesta entera y el modelo
no invocó nada". Son afirmaciones distintas y la segunda es mucho más fuerte.
Hoy solo Anthropic tiene extractor —Gemini y OpenAI usan formas distintas
(`functionCall`, `tool_calls`, items `function_call`) que **no se han
capturado contra tráfico real**— y las filas escritas antes de que el campo
existiera también rehidratan como `null`. Fundir ambos casos en un vector
vacío haría que cada una de esas filas contase como prueba de que un servidor
no se usa.

**`complete: false` no se puede deducir del `status`.** Es la trampa que
parece obvia y no lo es: el `status` se captura de la respuesta del upstream
**antes de que fluya un solo byte del cuerpo**, así que un turno que el
cliente aborta a mitad de stream sale con `200` y una lista parcial. Los
turnos abortados son comunes en flujos de agente. Una fila con
`complete: false` sirve para **confirmar** que un servidor se usó (si aparece
en ella), nunca para concluir que no se usó.

**`mcp_tool_use` sí se captura**, y cuenta como invocación de cliente: el
conector MCP server-side de Anthropic sale de un servidor que configuró el
usuario, que es exactamente lo que el recomendador mide.

#### Acotado igual que los nombres declarados

64 entradas por lista y 128 caracteres por nombre, los mismos topes que
`tool_names` y aplicados por **el mismo helper** (`provider::push_acotado`):
compartirlo no es estética, es que todo el valor del cruce
declarado-vs-invocado depende de que ambos lados guarden el MISMO string, y
dos truncados que pudieran divergir romperían la comparación por igualdad sin
que ningún test lo notara.

Y —igual que en el lado declarado— **el recorte queda a la vista**:
`invoked_total` cuenta lo visto sin aplicar el cupo, así que
`invoked_total > invoked.len()` delata el truncado. Es el mismo mecanismo por
el que `tool_names.len() < tools` lo delata en `tools_by_server`. Los dos
cupos son **independientes**: que un modelo agote el de cliente no puede
silenciar la lista de servidor. El truncado cuenta CARACTERES, no bytes:
cortar UTF-8 a mitad de un punto de código haría panic.

**Aditivo respecto a lo publicado**: `FIELDS` pasa de 13 a 14 (`tool_calls`) y
`CONTRACT_VERSION` sigue en 1. La forma de `invoked` cambió dentro de `main`
antes de que ninguna release la sirviera (ver el aviso de arriba), así que
ningún consumidor pudo escribirse contra la anterior.


## 4.16. `GET /mcp`: qué cuesta cada servidor y cuánto se usa

Los cuatro endpoints anteriores contestan sobre coste. Este contesta sobre
**valor**: cruza los bytes que un servidor MCP cuesta con las veces que
realmente se invoca, y es la única pregunta que necesita los dos lados del
cable a la vez —los bytes viven en la petición, las invocaciones en la
respuesta— así que solo la puede responder un punto que vea las dos.

```json
{
  "threshold": 50,
  "since": "2026-08-01T09:12:03Z",
  "servers_omitted": 0,
  "servers_omitted_saturated": false,
  "invoked_never_declared": [],
  "servers": [
    {
      "server": "context7",
      "kind": "mcp",
      "declared_in_requests": 200,
      "bytes_per_request_declared": 12400,
      "tools_declared": 8,
      "invocations": 0,
      "conclusive_requests": 187,
      "discarded": { "no_extractor": 11, "incomplete": 2, "truncated": 0, "unattributed": 0 },
      "verdict": {
        "type": "unused",
        "conclusive_requests": 187,
        "bytes_per_request_declared": 12400
      }
    }
  ]
}
```

Esa fila es la frase que motiva todo el campo `tool_calls` (§4.15): *pagas
12.400 B por petición por `context7` y no has invocado ninguna de sus 8
herramientas en 187 peticiones concluyentes*.

### El veredicto viene con la evidencia, no en su lugar

Recomendar quitar un servidor que sí se usa es el peor fallo posible aquí: el
usuario pierde una integración que funciona **por consejo del medidor**. Por
eso cada fila publica en qué se apoya, y una petición solo cuenta como prueba
de NO-uso si supera los cuatro filtros:

| Filtro | Qué descarta | Campo |
|---|---|---|
| Tiene extractor | `tool_calls: null` — el proveedor no mide, o la fila es anterior al campo | `discarded.no_extractor` |
| Se escaneó entera | `complete: false` — turno abortado, la lista es un prefijo | `discarded.incomplete` |
| Todo se atribuyó | una invocación se vio pero sin saber de qué servidor era | `discarded.unattributed` |
| No está truncada | el cupo recortó, una llamada pudo quedar fuera | `discarded.truncated` |

Los cuatro se cuentan por separado, y no como un total, porque cada uno se
arregla de forma distinta: `no_extractor` con un extractor nuevo,
`truncated` subiendo el cupo, `incomplete` no se arregla —es tráfico real que
el usuario abortó— y `unattributed` tampoco se arregla desde aquí: depende de
que el conector del proveedor diga de quién era la llamada.

El cuarto filtro merece una nota porque nació de un agujero real. El conector
MCP server-side de Anthropic (`mcp_tool_use`) manda el nombre DESNUDO y el
servidor en un campo hermano. Cuando ese campo no llega legible, la versión
anterior deducía el servidor del nombre — y como el nombre va desnudo, eso
resolvía `(native)` en el **100%** de los casos. No era un «no lo sé»: era una
atribución falsa, y además muda, el único de los cuatro caminos de pérdida de
evidencia que no se contaba. Ahora la llamada se cuenta como no atribuida y
descalifica la fila, porque pudo salir de cualquier servidor — incluido el que
el informe estaba a punto de declarar sin usar.

### La asimetría: ver una llamada prueba el uso; no verla no prueba el no-uso

Una invocación observada en una fila **incompleta** cuenta igual hacia
`invocations`. Solo el veredicto de NO-uso exige filas concluyentes. Si no
fuera así, un turno abortado podría esconder la única prueba de que un
servidor sí se usa, y el recomendador aconsejaría borrarlo.

### Los cuatro veredictos

| `type` | Significa |
|---|---|
| `used` | Se invocó al menos una vez. Nunca se recomienda quitarlo |
| `unused` | Cero invocaciones sobre `umbral` peticiones concluyentes o más |
| `insufficient_data` | Cero invocaciones, pero sin evidencia suficiente. **No es lo mismo que `unused`** — aquí la respuesta honesta es «todavía no lo sé» |
| `not_applicable` | `(native)` o `(others)`: no es algo que el usuario pueda quitar de su configuración |

### El umbral viaja en la respuesta

`threshold` es un **juicio, no una medida**, y por eso se publica: quien no esté
de acuerdo tiene `invocations`, `conclusive_requests` y los descartes para
aplicar el suyo. Está en 50 y no en 200 porque 200 tarda días en acumularse
en un uso normal y el consejo llegaría tarde para ser útil; 3 sería ruido que
cualquier sesión corta produce.

### La ventana: `desde`, y por qué no admite `?since=`

**El agregado NO cubre "todo lo que existió".** Se rehidrata del
`telemetry.jsonl` al arrancar —como `/stats` y `/sessions`, y por un motivo
más fuerte: sin histórico arrancaría en blanco tras cada reinicio y no
llegaría nunca al umbral— pero `rehydrate` solo repone los últimos
`OXIDEGATE_HISTORY_DAYS` días (**7 por defecto**, y `0` desactiva la
rehidratación).

Por eso el snapshot publica **`since`**: el timestamp más antiguo que entró en
el agregado. Sin ese campo, alguien con la ventana corta podría leer un
`unused` como si cubriera meses y borrar un servidor que usa una vez por
semana. Con `OXIDEGATE_HISTORY_DAYS=0` y un reinicio reciente, un `unused`
puede apoyarse en minutos de tráfico — `since` lo dice.

No admite `?since=`, a diferencia de `/stats`: la pregunta es sobre un
HÁBITO, y acotar más la ventana solo bajaría las peticiones concluyentes y
convertiría un `unused` en un `insufficient_data`. Si hace falta, entra
después como parámetro aditivo.

### Tres campos más que evitan un informe engañoso

- **`servers_omitted`**: servidores DISTINTOS que no se admitieron por el
  tope de 256. Las etiquetas salen de nombres que llegan en la respuesta
  —texto de fuera—, así que el registro tiene cupo como todos sus hermanos
  (`SessionRegistry` corta en 10.000, `StatsRegistry` en 50.000). Distinto de
  cero significa que **este informe está incompleto**.
- **`servers_omitted_saturated`**: el propio registro de omitidos también
  está acotado a 256, por la misma razón. Cuando se llena, `servers_omitted`
  deja de crecer y pasa a ser un **mínimo**: esta bandera lo declara. Sin
  ella, el contador que existe para hacer visible el tope se topaba él mismo
  en silencio, y un informe gravemente incompleto parecía solo un poco
  incompleto.
- **`invoked_never_declared`**: servidores MCP que se invocaron pero de los que
  nunca se vio la declaración, así que no hay coste que cruzar. El caso
  típico es el desborde de `MAX_TOOL_SERVERS`: con más de 32 servidores en
  una petición, los que sobran se pliegan en `(others)` y pierden su
  identidad del lado declarado, mientras sus invocaciones sí conservan el
  nombre real. Sin este campo desaparecían del informe — y son justo los del
  usuario con la configuración más cara.

### `bytes_per_request_declared` no es un ahorro sobre todo el tráfico

El nombre lleva `_declared` porque promedia **solo sobre las peticiones en las
que el servidor viajaba**, que es `declared_in_requests`. Un servidor
declarado en 20 de 1.000 peticiones cuesta eso en esas 20 y nada en las otras
980; multiplicar por el total exageraría la ganancia 50 veces. Los dos campos
van juntos en la fila justo para poder hacer la cuenta correcta.

Y ojo al leerlo junto a **`tools_declared`**, que es un valor PUNTUAL (el de
la última vez que se vio) mientras el coste es una media de toda la ventana:
si la configuración cambió dentro de ella, las dos cifras describen momentos
distintos.

---

## 4.17. `hooks`: el 29% del peaje, y una frontera que no existe

Cierra el último de los tres bloques del peaje fijo de una sesión. Con
`instructions` (§4.13) y `skills` (§4.8) ya publicados, los tres se ven:

| Bloque | Campo | % del peaje |
|---|---|---:|
| `CLAUDE.md` | `instructions` | 48% |
| **Salida de hooks** | **`hooks`** | **29%** |
| Listado de skills | `skills` | 23% |

```json
"hooks": { "bytes": 12097, "declared": 1, "format": "claude_code" }
```

- **`bytes`**: el bloque completo, marcas incluidas. No es lo que ocupan tus
  hooks en disco ni lo que imprimen en tu terminal: es lo que el harness
  inyecta en el cuerpo, y se paga en cada petición de la sesión.
- **`declared`**: marcas `hook success:` contadas. Cuenta lo que el CABLE
  trae, no lo que hay en `settings.json` — un hook configurado que no produjo
  salida no aparece, y es correcto: no cuesta nada.
- **`format`**: hoy solo `claude_code`.

### La palanca es distinta a la de los otros dos bloques

Este bloque **no lo escribe el usuario**: lo generan los hooks que tiene
configurados, y muchos vienen de plugins. En la captura de
[`fixed-toll-claude-code.md`](fixed-toll-claude-code.md), uno solo —el del
plugin de Vercel— aportaba 7.654 B. La palanca no es «escribe menos», es
**decidir si cada hook vale su peaje**, la misma conclusión a la que llegó §3
de ese documento con los plugins.

### La frontera, y por qué `null` aparece más de lo que parecería

El harness **abre** el bloque con una marca (`SessionStart:startup hook
success:`) y **no lo cierra**. Verificado sobre captura real del 2026-08-09:
la parte `messages[1]` / `role: "system"` contiene exactamente dos cosas
pegadas —la salida de los hooks y el listado de skills— sin nada en medio.

Con `instructions` el envoltorio existía (`<system-reminder>`…`</…>`). Aquí no,
así que la única frontera disponible es dónde EMPIEZA el listado de skills.

**Y si esa cabecera no se encuentra, el campo es `null` en vez de correr hasta
el final de la parte.** Es deliberado. Correr hasta el final sería correcto
cuando de verdad no hay skills instaladas, pero si la cabecera cambiara,
`hooks.bytes` se tragaría el listado y publicaría ~16 kB de más: un número
plausible y falso. Ese error concreto ya ocurrió dos veces en este proyecto
—están documentados en §4 de `fixed-toll-claude-code.md`— y el precio de
evitarlo es un falso negativo en máquinas sin ninguna skill, que con las que
Claude Code trae de serie es un caso casi vacío.

### Lo que este campo NO hace: restar

`parte − skills.listing_bytes` parece equivalente y no lo es. `listing_bytes`
subestima el listado cuando una skill trae la descripción en varias líneas
(issue #84: −1.453 B sobre esta misma captura), y restar convertiría ese error
en bytes de hooks que nunca existieron. La frontera es el **inicio** de la
cabecera, no una diferencia entre dos medidas.

---

## 4.18. El dialecto nativo de ollama, y qué separa

`POST /api/generate` y `POST /api/chat`. Existe aparte del endpoint
OpenAI-compatible —que el proxy ya medía— porque ese endpoint publica **solo
contadores de tokens**, y tira el reparto interno del tiempo.

Medido a través del proxy con el modelo **frío**:

| | |
|---|---:|
| `load_us` | **1.451 ms — el 57%** |
| `prompt_eval_us` | 23 ms |
| `eval_us` | 1.070 ms |
| `total_ms` | 2.548 ms |

`ttft_ms` mezcla la carga con el procesado del prompt y **no los distingue**.
Estas tres cifras sí, y hacen falta para excluir la carga de cualquier cuenta
por token.

#### Corrección medida: cargar cuesta TIEMPO, no vatios

Una versión anterior de esta sección decía que una petición fría inflaría la
cuenta por token «unas 2,5 veces». **Es falso**, y el error era convertir una
proporción de TIEMPO en una afirmación sobre ENERGÍA sin medirla.

Medido con una petición que es **98% carga** (`num_predict: 1`, modelo frío):

| | |
|---|---:|
| Potencia media de la ventana | **43,0 W** (pico 68,9) |
| La misma tarjeta **generando** | **~189 W** de media |

**Cargar el modelo mueve memoria, no calcula.** Dibuja del orden de una cuarta
parte de lo que dibuja generar, así que su peso en el tiempo **sobrestima su
peso en la energía** unas cuatro veces.

En la comparativa de abajo, sobre `qwen2.5:7b` con 200 tokens fijos:

| | frío | caliente | inflación |
|---|---:|---:|---:|
| `load_us` | 1.870 ms | 124 ms | — |
| Wh atribuibles / 1k tokens | 0,465 | 0,396 | **+17%** |

Diecisiete por ciento, no dos veces y media. La carga fue el **54% del tiempo**
y el **11% de la energía atribuible** de esa petición.

Excluirla sigue haciendo falta —un 17% no es ruido, y con respuestas cortas la
proporción crece— pero el motivo correcto es ese, no el que estaba escrito.

### Lo que NO arregla

**No corrige ningún error de `tokens_per_sec`.** En streaming el proxy calcula
`salida / (total_ms − ttft_ms)`, y el `ttft_ms` **ya absorbe la carga**: el
primer chunk no sale hasta que el modelo está cargado. Sobre esa misma
petición, `ttft_ms` fue 1.477 ms contra 1.474 de `load + prompt_eval`, y la
velocidad publicada **126,1 frente a 126,2 reales**. Fuera de streaming el
campo es `null`, no un número malo.

Se deja escrito porque una versión anterior de esta documentación afirmaba lo
contrario, y hay un test que fija la equivalencia para que no vuelva.

### NDJSON, no SSE

Ollama nativo **hace streaming por defecto** —al revés que OpenAI— y manda
NDJSON: un objeto JSON por línea, **sin prefijo `data:`**. Los totales viajan
en la última línea, la del `done: true`.

Eso obligó a que el formato de línea lo decida el proveedor
(`Provider::payload_de_linea`, sin default) en vez del escáner. Antes el
escáner exigía `data:` a todo el mundo: contra este dialecto habría ignorado
cada línea y publicado **cero tokens en silencio**, indistinguible de una
respuesta sin tokens.

`OXIDEGATE_OLLAMA_API_BASE` apunta al motor; por defecto `127.0.0.1:11434`.

---

## 4.19. Energía: lo que gasta la máquina, nunca lo que cuesta en euros

Contra un modelo de nube, `cost_estimate_usd` dice exactamente lo que cuesta
una petición. Contra `ollama` en tu propia máquina decía **nada** — y sin
embargo se paga: se paga en electricidad.

| Campo | Qué es |
|---|---|
| `energy_wh` | Energía **bruta**: el área bajo la curva de potencia durante la ventana, reposo incluido |
| `energy_idle_wh` | Lo que la máquina habría gastado **en reposo** esa misma ventana |
| `power_peak_w` | Pico de potencia dentro de la ventana |
| `energy_samples` | Cuántas muestras **reales** cayeron dentro |

### Lo que este campo NO dice

**No dice «esta petición gastó tanto».** Dice «la máquina gastó tanto MIENTRAS
esta petición estuvo abierta».

Si dos peticiones se solapan, **las dos integran los mismos vatios y las dos
los reclaman**. Sumar `energy_wh` sobre filas solapadas da más energía de la
que la máquina consumió. No es un bug que se pueda arreglar con estos datos: es
lo que significa el campo. Hay un test que fija la propiedad
(`dos_ventanas_solapadas_reclaman_la_misma_energia`) para que quede escrita en
vez de descubrirse sumando una columna.

Es la misma trampa que `fixed-toll-claude-code.md` §4 llama **«leer los bytes,
no restarlos»**: mezclar dos medidas tomadas en puntos distintos y presentar el
resultado como si fuera una sola.

### Por qué el reposo se publica y no se resta

Porque la atribución **no es limpia**. Si otra cosa usa la GPU a la vez, la
muestra no es solo de la inferencia. Un único número ya restado fingiría una
precisión que no hay; publicando el reposo al lado, la resta la hace quien lee
**viendo lo que resta**.

Y el reposo tampoco es una constante: es el **mínimo observado** en los últimos
minutos. Si la GPU nunca estuvo ociosa en esa ventana, será alto y la resta
dará de menos — cosa que se puede ver, justamente porque está publicado.

### Nunca euros

Se publica la energía. El precio del kWh lo pone quien lee: cambia por país,
por contrato y por hora del día. Un euro impreso aquí sería falso en cuanto
cambiara la tarifa, y nadie volvería a mirarlo.

### Cuándo es `null`, y por qué cada caso

| Caso | Por qué |
|---|---|
| Upstream **remoto** | Muestrear tu GPU mientras responde Anthropic mide **tu escritorio**, no la inferencia |
| Sin `nvidia-smi` | No hay nada que leer. Ausencia honesta, no un cero |
| `OXIDEGATE_POWER_SAMPLING=off` | Lo apagaste |
| El anillo no cubre la ventana | Típico en la primera petición tras arrancar: cualquier cifra sería una extrapolación disfrazada de medición |
| Upstream falló (502) | No se llegó a inferir. Los vatios de esa ventana son de otra cosa |

Lo local se decide por el **host parseado** de la URL destino, no por
`contains`: `localhost.ejemplo.com` es un dominio remoto perfectamente
registrable y contiene la palabra.

### Lo que se ve al comparar dos modelos

Medido a través del proxy, mismo prompt, `num_predict: 200` para que los dos
generen **exactamente los mismos tokens**, `temperature: 0`, modelo caliente:

| | tok/s | ventana | Wh netos | W netos medios | pico |
|---|---:|---:|---:|---:|---:|
| `qwen2.5:7b` | 126,8 | 1.720 ms | 79,1 mWh | **165,6 W** | 280,9 W |
| `llama3.2:3b` | 231,1 | 999 ms | 27,4 mWh | **98,7 W** | 188,2 W |
| **razón** | 1,82× | 1,72× | **2,89×** | 1,68× | |

**El 3b es 1,82× más rápido pero 2,89× más barato en energía.** Los números no
coinciden porque son **dos factores independientes**: tarda 1,72× menos *y*
dibuja 1,68× menos potencia mientras lo hace. 1,72 × 1,68 = 2,89.

Esto es exactamente lo que el issue #92 dice que no se puede despejar: **el
rendimiento no predice el consumo**. Con solo el `tok/s` habrías estimado un
ahorro de 1,8× y el real es de 2,9× — y el error va en la dirección que hace
parecer peor de lo que es al modelo pequeño.

Por 1.000 tokens de salida, la energía atribuible:

| | frío | caliente |
|---|---:|---:|
| `qwen2.5:7b` | 0,465 Wh | 0,396 Wh |
| `llama3.2:3b` | 0,200 Wh | 0,137 Wh |

Se publica la energía, no el precio. Multiplicar por tu tarifa es cosa tuya.

### El muestreador

Un `nvidia-smi -lms 200` **persistente**, arrancado una vez con el proxy. La
objeción obvia es el coste, y está medida: **arrancar** `nvidia-smi` cuesta
23,81 ms —seis veces todo el overhead del proxy— pero eso es el coste de
arrancarlo, no el de leerlo. El proceso persistente cuesta **0,1% de un core**
(50 muestras en 10 s, medido), y leer el anillo dentro de `emit` cuesta
microsegundos.

Esto es **Linux con NVIDIA primero**, y se declara en vez de que el campo salga
`null` en un Mac sin que nadie sepa por qué. RAPL para la CPU y `powermetrics`
en macOS no están. La integración sí está separada de quién llena el anillo,
así que añadir otra fuente no tocaría la cuenta — pero hoy no hay ninguna, y
decir lo contrario sería vender una capacidad que no existe.

---

## 5. Límite de memoria: 200 filas, y se pierden al reiniciar

`RECENT_CAPACITY = 200` (`src/telemetry/recent.rs`): el buffer es un
`VecDeque` que nunca guarda más de 200 requests. Al llegar la 201, se
desaloja la más vieja (`pop_front`) — memoria acotada y constante en un
proceso de larga vida, sin necesidad de configurar nada.

**Esto vive únicamente en RAM.** A diferencia de `telemetry.jsonl`, que
persiste en disco y sobrevive a un reinicio del proxy, `/requests` se vacía
por completo cada vez que el proceso de OxideGate se reinicia. Si se necesita
el historial completo, o algo más viejo que las últimas 200 peticiones, la
única fuente confiable es `telemetry.jsonl`.

| | `GET /requests` | `GET /stats` | `telemetry.jsonl` |
|---|---|---|---|
| Nivel de detalle | por petición individual | agregado por `(proveedor, modelo)` | por petición individual |
| Ventana | últimas 200 peticiones | todo el histórico del proceso | todo el histórico, en disco |
| Persistencia | en memoria, se pierde al reiniciar | en memoria, se pierde al reiniciar | persistente en disco |
| `prompt_hash` | nunca (no existe el campo) | nunca | sí, por fila |
| Para qué sirve | ver la fila atípica puntual | decidir qué modelo optimizar | análisis offline, auditoría, recuperar historial completo |

---

## 6. Cómo se calcula (diseño interno)

- **`src/telemetry/recent.rs`** es puro: no conoce axum, solo `RequestMetric`
  y su propia proyección `RecentRequest`. `RecentRequests::ingest` agrega al
  final del `VecDeque` (orden cronológico) y desaloja la más vieja si se
  supera `RECENT_CAPACITY`; `RecentRequests::snapshot` devuelve una copia
  independiente del estado actual, sin decidir orden de presentación (eso
  queda del lado del consumidor — ver el panel del monitor en
  `docs/monitor-tui.md`).
- **`src/middleware/requests.rs`** es el único archivo de esta cadena que
  conoce axum: expone `GET /requests`, toma un read-lock breve sobre el
  buffer compartido, clona el snapshot y lo serializa a JSON. El lock se
  suelta **antes** de cualquier punto de suspensión (`.await`), igual que
  hace `middleware/stats.rs` con `StatsRegistry`.
- El buffer vive en `Arc<RwLock<RecentRequests>>`, alimentado por la MISMA
  task de drenaje en segundo plano que ya alimenta `StatsRegistry`
  (`src/telemetry/logger.rs`). No hay una segunda ruta de instrumentación:
  cada `RequestMetric` que se escribe a disco es también la que alimenta este
  buffer, en el mismo lugar y al mismo tiempo. Esto mantiene la captura
  **fuera del camino crítico del request** — igual que el resto de la
  telemetría.

---

## 7. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/recent.rs` | `RecentRequests`, `RecentRequest` — buffer FIFO acotado y proyección, sin axum |
| `src/provider/skills.rs` | `detect_skills`, `detect_skills_in_body` — reconoce las tres formas medidas; un bloque sin entradas no cuenta (§4.8) |
| `src/provider/anthropic.rs` | `aplicar_palancas`, `fuerza_effort` — las dos palancas del optimizador sobre el mismo body, con UNA sola serialización (§4.14) |
| `src/provider/instructions.rs` | `detect_instructions`, `detect_instructions_in_body` — el bloque de `CLAUDE.md`, delimitado por su envoltorio y no por una cabecera (§4.13) |
| `src/provider/block_scan.rs` | `primer_bloque_con` — el recorrido que comparten los dos detectores: un bloque sólo cuenta si trae su marca interna, y una mención no interrumpe la búsqueda |
| `src/telemetry/codex_quota.rs` | `CodexQuota`, `CodexQuota::from_headers` — parseo y saneo de las doce cabeceras `x-codex-*`, sin ningún campo en USD (§4.7) |
| `src/telemetry/cache_attribution.rs` | `CacheBySection`, `attribute_cache` — el paseo por el prefijo, función pura. Se llama desde `metered.rs` al emitir (único punto donde coinciden los cubos y los tokens de caché), nunca en el camino crítico (§4.11) |
| `src/telemetry/pricing.rs` | `cache_accounting_for_upstream` — contabilidad de caché por FAMILIA, sin pasar por la tarifa: un modelo sin precio declarado sigue siendo atribuible. Un test guarda que no diverja de `model_pricing` (§4.11) |
| `src/telemetry/logger.rs` | `TelemetrySink::spawn` alimenta el buffer en la misma task que escribe el JSONL; `TelemetrySink::recent()` expone el `Arc<RwLock<RecentRequests>>` |
| `src/middleware/requests.rs` | `handle_requests` — el handler HTTP de `GET /requests` |
| `src/middleware/version.rs` | `handle_version`, `CONTRACT_VERSION`, `ENDPOINTS`, `FIELDS` — el contrato declarado de `GET /version` (§8) |
| `src/main.rs` | Registra las rutas `/requests` y `/version` en el `Router` |

---

## 8. El contrato: qué se puede cambiar y qué no

`/requests`, `/stats` y `/sessions` son la API pública de facto del
ecosistema. Hoy dependen de sus campos, como mínimo, `oxidegate-monitor` y
`oxidegate-lens` (`oxidegate-savings`, `oxidegate-mcp`). Esta sección dice
qué puede cambiar sin avisar y qué no.

### 8.1. `GET /version`: preguntar en vez de deducir

Antes de esta ruta, la única forma que tenía un consumidor de saber si un
campo existía era hacer una petición real, mirar el JSON y deducirlo — es
decir, **sondear por ausencia**. Y sondear por ausencia no distingue *«este
proxy no lo soporta»* de *«aquí no había dato»*, que es exactamente la
confusión que el resto de este documento se niega a cometer: **un hueco
honesto no es un cero.**

```
GET /version → {
  "oxidegate": "0.3.1",
  "contract": 1,
  "endpoints": ["/health", "/stats", "/sessions", "/requests"],
  "fields": ["tool_names", "tool_search", "tools_flattened", "skills", "instructions",
             "effort_forced",
             "response_bytes", "codex_quota", "session", "prepare_us"]
}
```

- **`oxidegate`** — `CARGO_PKG_VERSION` del binario que responde.
- **`contract`** — versión del contrato, **independiente** de la del crate.
  Sube solo al ROMPER (ver §8.2). Arranca en `1`: es el primer contrato
  declarado.
- **`endpoints`** — rutas que sirve este binario, absolutas y concatenables
  a la base-URL tal cual.
- **`fields`** — el subconjunto de campos que marca una CAPACIDAD. No es la
  lista completa de claves: es la lista de campos cuya **ausencia significa
  "actualiza el proxy"**, no "aquí no había dato". Es lo que permite a
  `oxidegate-savings --doctor` decir *«tu OxideGate es antiguo, actualiza
  para ver esta columna»* en vez de enseñar una tabla vacía sin explicar por
  qué.

La ruta es **aditiva**: un proxy anterior devuelve `404`, y eso ya es una
respuesta útil — «esto es previo al contrato». `GET /health` **no se toca**:
sigue devolviendo `{"status":"ok"}` y nada más, porque es liveness y hay
clientes que dependen de que su payload no cambie.

### 8.2. Qué es aditivo y qué es ruptura

| Cambio | ¿Rompe? | ¿Sube `contract`? |
|---|---|---|
| Añadir un campo nuevo a una fila | No | No — se añade a `fields` si es una capacidad sondeable |
| Añadir un endpoint nuevo | No | No — se añade a `endpoints` |
| Añadir una variante a un enum ya publicado | No | No — el consumidor debe tolerar valores desconocidos |
| **Renombrar** un campo | **Sí** | **Sí** |
| **Quitar** un campo | **Sí** | **Sí** |
| **Cambiar el tipo** de un campo (`string` → `int`, escalar → objeto) | **Sí** | **Sí** |
| **Cambiar la unidad** de un campo (bytes → tokens, ms → µs) | **Sí**, y en silencio | **Sí** |

El último es el peor de todos: un cambio de unidad no rompe ningún parser.
El consumidor sigue leyendo un número, lo pinta, y el usuario lee una cifra
mil veces mayor sin que nada falle. Si alguna vez hay que hacerlo, **el campo
se renombra** (`total_ms` → `total_us`) para que la ruptura sea ruidosa.

### 8.3. Qué debe hacer un consumidor

1. **Ignorar todo campo que no conozca.** Un campo nuevo nunca es un error.
2. **Tratar `404` en `/version` como «proxy anterior al contrato»**, no como
   «proxy caído» — para eso está `/health`.
3. **Sondear `fields`, no comparar versiones.** `contract` dice si algo se
   rompió; `fields` dice si una capacidad concreta está. Casi siempre lo que
   se necesita es lo segundo.
4. **No confundir `null` con ausencia de la clave.** Todas las claves de una
   fila se publican siempre; `null` es un hueco honesto y significa «no se
   pudo medir», nunca cero (§4.1).

### 8.4. Los tests que lo guardan

Tres snapshots congelan las claves publicadas, uno por endpoint:

| Test | Archivo | Cubre |
|---|---|---|
| `las_claves_de_requests_no_cambian_sin_querer` | `src/telemetry/recent.rs` | `GET /requests` |
| `las_claves_de_stats_no_cambian_sin_querer` | `src/telemetry/stats.rs` | `GET /stats` |
| `las_claves_de_sessions_no_cambian_sin_querer` | `src/telemetry/stats.rs` | `GET /sessions` |

Si alguien renombra un campo, el test lo cuenta antes que un usuario. El
mensaje de fallo dice qué hacer según el cambio sea aditivo o ruptura.

#### Los snapshots NO miran dentro de los objetos anidados

Los tres snapshots de arriba congelan las claves de **primer nivel** de la fila
(`fila.as_object().keys()`). No recorren nada recursivamente, así que ninguno
cubre lo que va dentro de `tools_by_server`, `codex_quota`, `skills`, `session`,
`tool_search` o `cache_by_section`.

El que sí es recursivo es otro: `version_no_anuncia_campos_que_requests_no_publique`
recorre el JSON entero, pero solo comprueba que cada entrada de `FIELDS` aparezca
en algún sitio. Eso cubre el NOMBRE de cada objeto anidado —y la clave interna
`tool_names`, que está en `FIELDS` por sí misma— y nada más.

**El hueco se cierra con una guarda de forma por objeto**, cada una junto al tipo
que la define:

| Objeto | Guarda | Dónde |
|---|---|---|
| `tools_by_server` | `la_forma_de_tool_server_bytes_no_cambia_sin_querer` | `src/provider/mod.rs` |
| `tool_search` | `la_forma_de_tool_search_signal_no_cambia_sin_querer` | `src/provider/mod.rs` |
| `skills` | `la_forma_de_skills_no_cambia_sin_querer` | `src/provider/skills.rs` |
| `instructions` | `la_forma_de_instructions_no_cambia_sin_querer` | `src/provider/instructions.rs` |
| `hooks` | `la_forma_de_hooks_block_no_cambia_sin_querer` | `src/provider/hooks.rs` |
| `codex_quota` | `la_forma_de_codex_quota_no_cambia_sin_querer` | `src/telemetry/codex_quota.rs` |
| `session` | `la_forma_de_session_no_cambia_sin_querer` | `src/telemetry/session.rs` |
| `cache_by_section` | `el_json_publicado_conserva_method_y_las_cinco_secciones` | `src/telemetry/cache_attribution.rs` |
| `input_share_by_section` | `el_json_publicado_conserva_method_y_las_cinco_fracciones` | `src/telemetry/section_share.rs` |

Cada una serializa el objeto POBLADO y afirma el conjunto exacto de claves, con
un mensaje que dice qué hacer según el cambio sea aditivo o ruptura. La de
`codex_quota` construye por `from_headers` en vez de a mano, así que además
guarda que el camino de producción siga rellenando las doce claves.

Y `session` lleva una segunda guarda,
`las_etiquetas_de_source_no_cambian_sin_querer`: el VALOR de `source`
(`explicit`/`native`/`unattributed`) también es contrato, porque un consumidor
ramifica sobre esas cadenas, y lo produce un `rename_all` que se podría cambiar
sin tocar ningún nombre de campo.

**Lo que cazan, comprobado**: aplicar un `#[serde(rename = "deferred")]` a
`deferred_tools` hace fallar la guarda con el diff exacto de claves. Un renombrado
de campo Rust rompe la compilación por otros sitios; el peligroso es este otro, el
que cambia el cable sin tocar el código que lo lee.

Y un cuarto, `version_no_anuncia_campos_que_requests_no_publique`, comprueba
que **cada entrada de `fields` existe de verdad en el JSON** —incluidas las
anidadas, como `tool_names` dentro de `tools_by_server`—. Sin él, `/version`
podría anunciar una capacidad que el proxy no tiene, y el consumidor volvería
justo al agujero que esta ruta viene a tapar.

### 8.5. Por qué existe esta sección

Porque el patrón ya falló dos veces, y las dos igual: **dos piezas correctas
a ambos lados y un fallo silencioso en medio.**

- **`tool_names`** — `oxidegate-lens` consume el campo, testeado y en su
  `main`, y no hacía nada contra un proxy 0.3.1 porque el campo no existía
  todavía en la build instalada.
- **`GET /health`** — existía en el código desde 0.3.0 mientras el tap de
  Homebrew servía 0.2.1. El plugin sondeaba la ruta, recibía `404` y caía al
  proveedor directo **en silencio**, sin error y sin log. El monitor estuvo
  vacío meses por eso.

El coste de versionar un contrato crece con el número de consumidores. Ahora
mismo son dos.
