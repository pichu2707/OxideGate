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

`RecentRequest` — el tipo que serializa cada fila — **no tiene** los campos
`prompt_hash` ni `prompt_bytes`. No es un filtro en tiempo de ejecución que
alguien pueda desactivar por error: es una garantía de compilación, porque el
campo directamente no existe en el struct. `telemetry.jsonl` sí guarda
`prompt_hash` por fila (para poder correlacionar redundancia offline), pero
esa huella nunca llega a la API HTTP.

Esto mirroriza la misma invariante que ya documenta
`docs/telemetry-by-model.md` para los agregados de `/stats` y que impone
`src/middleware/stats.rs`: el proxy no expone huellas de prompt por HTTP,
haya o no autenticación de por medio.

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
| `tools_by_server` | Desglose de `context_tools_bytes` por servidor MCP declarante: `[{server, kind, tools, bytes, deferred_tools}, …]`, ordenado por `bytes` descendente | `null` si el body no parseó como objeto (o build anterior a este campo); `[]` si SÍ parseó pero no declaraba `tools` — son estados DISTINTOS, ver §4.2. `deferred_tools` (por elemento) es la fuente de verdad POR SERVIDOR de cuánto está diferido, ver §4.2 |
| `tools_overhead_bytes` | Bytes de `tools` no atribuidos a ningún servidor (brackets/comas del array, wrapper de Gemini, herramientas huérfanas) | `null` en los mismos casos que `tools_by_server` es `null`; `sum(tools_by_server[].bytes) + tools_overhead_bytes == context_tools_bytes` siempre que ambos sean no-nulos |
| `tool_search` | Señal de carga diferida de herramientas del dialecto Responses/Codex: `{used, deferred_loaded}`, o `null`. `used: false` ⇒ EAGER confirmado este turno; `used: true` ⇒ LAZY (el cliente cargó tools a mitad de sesión) | `null` en Anthropic/Gemini/OpenAI-Chat (no aplica) o si el body no parseó. Es el diferenciador eager-vs-lazy por cliente que `tools_by_server` NO puede dar — ver §4.3 |
| `tools_flattened` | Honestidad de la atribución de `tools_by_server`: `true` ⇒ el cliente NO usa el namespacing `mcp__`, así que su cubo `(native)` puede ocultar MCP aplanado; `false` ⇒ hay tools `mcp__`, el `(native)` es de fiar; `null` ⇒ no aplica (Anthropic/Gemini/Chat) o sin tools | Solo dialecto Responses/Codex. `pi` manda nombres crudos y `opencode` usa `<server>_<tool>` (ambiguo) — ninguno con `mcp__`. Es una advertencia estructural, NUNCA una atribución inventada: no nombra servidores. Ver §4.4 |
| `session` | Sesión resuelta por precedencia de cabeceras del request: `{source, key}`. Nunca `null` — la peor rama es un fallback honesto (`source: "unattributed"`), no una ausencia | Ver §4.6 para la tabla de precedencia completa y cómo estampar el header desde cada harness |
| `skills` | Listado de skills declarado en el body: `{declared, listing_bytes, format}`, o `null`. Se paga en CADA petición, se invoque una skill o no | `null` = no se reconoció ningún listado, **nunca "cero skills"**. `format` dice qué forma se encontró y, de paso, de qué herramienta viene el tráfico sin fiarse del `User-Agent` — ver §4.8 |
| `codex_quota` | Estado de la cuota de suscripción de Codex (OAuth de `chatgpt.com`), parseado de las doce cabeceras `x-codex-*` de la RESPUESTA del upstream: doce campos, todos opcionales. `null` si la petición no fue a Codex vía OAuth, o si el upstream falló antes de responder | Es el ÚNICO campo de esta fila que se lee de la respuesta y no del request ni del body. Cuota NUNCA son dólares: no alimenta ni puede alimentar `cost_estimate_usd` — ver §4.7 |

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
invariante de privacidad del §3 (`prompt_hash`/`prompt_bytes` nunca se
exponen) se extiende aquí: el nombre de una herramienta puntual, o un
fragmento de su `input_schema`/`description`, tampoco sale por HTTP.

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
| `declared` | Cuántas skills declara el listado |
| `listing_bytes` | Bytes del bloque completo. Se pagan en CADA petición, se invoque una skill o no |
| `format` | `flat_list` \| `available_skills_xml` \| `skills_instructions` |

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

**No dobla bytes.** Esos bytes ya los cuentan los campos `context_*` — viven
dentro de uno de sus buckets. `skills` sólo **atribuye**; nunca vuelve a
sumarlos.

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
| `src/telemetry/codex_quota.rs` | `CodexQuota`, `CodexQuota::from_headers` — parseo y saneo de las doce cabeceras `x-codex-*`, sin ningún campo en USD (§4.7) |
| `src/telemetry/logger.rs` | `TelemetrySink::spawn` alimenta el buffer en la misma task que escribe el JSONL; `TelemetrySink::recent()` expone el `Arc<RwLock<RecentRequests>>` |
| `src/middleware/requests.rs` | `handle_requests` — el handler HTTP de `GET /requests` |
| `src/main.rs` | Registra la ruta `/requests` en el `Router` |
