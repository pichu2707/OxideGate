# El impuesto de contexto — qué pagás realmente en una sesión de agente

> Estado: **medido**, no estimado. Todo lo que sigue sale de tráfico real de
> Claude Code contra `claude-opus-4-8`, capturado por OxideGate en
> `~/.config/oxidegate/telemetry.jsonl` durante una sesión de medición del
> 2026-07-09 (proxy en el puerto 8899). Donde algo es estimación o hipótesis
> sin confirmar, se marca explícitamente como tal — no se mezcla con lo medido.

---

## 1. La respuesta primero

De cada dólar que factura esta sesión:

- **78.2%** es máquina de contexto (releer + escribir prefijo).
- **18.9%** es la respuesta que de verdad pediste (output).
- **3.0%** es input fresco — lo único que un dedup de respuestas podría atacar.

El caching de Anthropic **ya está andando al máximo**: sin él, esta sesión
hubiese costado ~$45 en vez de ~$8.33. La palanca que queda no es "cachear
más" — es **reducir el prefijo que se cachea**, porque ese prefijo se repaga
completo en cada turno y una conversación de N turnos crece en costo como
N², no como N.

---

## 2. Descomposición del costo

29 requests reales a `claude-opus-4-8`, $8.3296 notionales en total
(~$0.2872/request):

| componente | USD | % | tokens |
|---|---|---|---|
| `cache_read` (re-leer contexto) | 4.1387 | 49.7% | 2,759,124 |
| `cache_write` (escribir contexto nuevo a caché) | 2.3707 | 28.5% | 126,435 |
| output (generación) | 1.5707 | 18.9% | 20,943 |
| input fresco | 0.2495 | 3.0% | 16,635 |

**Lecturas clave:**

- **78.2% del costo es máquina de contexto** (`cache_read` + `cache_write`
  juntos); solo el 3.0% es input genuinamente nuevo.
- **El caching ya está en su techo.** Esos 2.76 M tokens re-leídos hubiesen
  costado $41.39 sin caché (tarifa plena de input) en vez de los $4.14 que
  costaron con `cache_read` (a 0.1× la tarifa de input). La palanca que
  queda **no es "cachear más"** — ya se está cacheando todo lo cacheable. Es
  **"reducir el prefijo"**: cada token que entra al prefijo se paga una vez
  para escribirlo y de nuevo, a cada turno siguiente, para releerlo.
- **Una sola fila de cold-start concentra el 20% de la factura.** Un request
  escribió 82,947 tokens a caché de una sola vez y costó $1.69 — el 20.3% de
  toda la sesión, y el 65.6% de todos los bytes de `cache_write` de la
  sesión. El arranque de una conversación es, en plata, el momento más caro
  por lejos.
- **El costo de una conversación crece como N², no como N.** Cada turno
  relee el prefijo completo acumulado hasta ese punto, y el prefijo crece con
  cada turno. Un token puesto en el prefijo en el turno 1 se vuelve a pagar
  (a tarifa de `cache_read`, pero se paga) en los turnos 2, 3, 4… hasta el
  final de la sesión. No es overhead fijo por turno: es overhead acumulativo.

> **Nota metodológica.** El conjunto de 29 requests combina 27 turnos
> exitosos de la conversación medida el 2026-07-09 entre 11:38 y 12:03
> (1507 s de span), 1 request que devolvió `429` en ese mismo tramo (0
> tokens, 0 costo, pero sí cuenta como request real emitido), y 1 turno
> exitoso capturado dos días antes (2026-07-07) contra el mismo modelo, sin
> escritura de caché previa. Se incluye porque es el único dato disponible
> de un turno "limpio" sin ningún prefijo cacheado todavía; se señala acá
> para que quien re-derive estos números desde el JSONL no interprete que
> los 29 requests son 29 turnos consecutivos de una sola conversación.

---

## 3. Descomposición de la latencia

347 s de tiempo "ocupado" (proxy + red + generación) dentro de un span de
pared de 1507 s. Es decir: **el 77% del reloj de pared es tiempo humano
pensando**, no latencia de máquina.

| componente | % del tiempo ocupado | media |
|---|---|---|
| generación (streaming) | 82% | 10,309 ms/request |
| TTFT (time-to-first-token) | 18% | 2,097 ms |

- **La generación (82% del tiempo ocupado) es intocable para un proxy.**
  OxideGate se sienta en el medio del wire; no puede acelerar cuánto tarda el
  modelo en producir tokens. Cualquier optimización de latencia tiene que
  atacar el otro 18%.
- **TTFT varía 17× sin explicación identificada**: rango 347–5,888 ms.
  Ninguna variable medida (bytes subidos, hora del día, tamaño de prefijo)
  explica esa dispersión por sí sola — ver hipótesis descartada en §8 sobre
  el pool de conexiones.
- **La subida de bytes queda descartada como cuello de botella.** Media de
  280,764 bytes por request: eso son ~7 ms de transferencia en fibra y ~225
  ms a 10 Mbps. Ninguno de los dos explica un TTFT medio de 2,097 ms — el
  tiempo se pierde después de que el body ya llegó al proveedor, no en la
  subida.
- **Ya hay algo de concurrencia.** 13 de 28 transiciones se solapan en el
  tiempo con la petición anterior, y la concurrencia máxima simultánea
  observada es de 4 peticiones. Claude Code ya dispara trabajo en paralelo
  (subagentes), no todo es estrictamente secuencial.

  > El solapamiento se calcula tomando `timestamp` como el instante de FIN de
  > la petición (es cuando se emite la métrica) y derivando el inicio como
  > `timestamp - total_ms`. Tratar `timestamp` como el inicio subestima el
  > solapamiento; es un error fácil de cometer al re-derivar estos números
  > desde el JSONL.

- **Un `429` en el conjunto.** El request de las 11:38:30 fue rechazado por
  rate limit. Es la señal que de verdad importa cuando el cliente se
  autentica con una suscripción plana (ver §7): ahí el techo no lo pone el
  gasto, lo pone la cuota.

---

## 4. El experimento controlado: el piso del harness

Mismo probe (`claude -p "Responde solo: ok"`), dos directorios de trabajo,
medido en el wire por OxideGate:

| escenario | prefijo (tokens) | prefijo (bytes) | TTFT |
|---|---|---|---|
| directorio vacío (sin git, sin memoria de proyecto) | 61,566 | 168,822 | 1,368 ms |
| directorio de OxideGate (proyecto real) | 83,719 | 224,533 | 2,961 ms |
| **delta** | **+22,153 (+36%)** | +55,711 | +1,593 ms |

> ### CORRECCIÓN — este delta es un ARTEFACTO, no un hallazgo
>
> La tabla de arriba se conserva como registro de lo que se midió, pero su
> lectura original era **falsa** y se refuta más abajo. Se dejó a la vista, en
> lugar de borrarla, porque el error importa tanto como el dato.
>
> La conclusión que se sacó entonces —"estar dentro del proyecto cuesta 22,153
> tokens de contexto de memoria y registro de skills"— **no se sostiene**. El
> experimento tenía n=1 por escenario y no controlaba la variable que en
> realidad cambiaba: **cuántos servidores MCP se cargaron en cada corrida**.
>
> Verificación posterior, con dos métodos independientes:
>
> 1. **Captura directa de ambos bodies** con un sumidero HTTP local, comparados
>    componente a componente:
>
>    | componente | vacío | proyecto | delta |
>    |---|---|---|---|
>    | `system` | 8,339 B | 8,928 B | **+589 B** |
>    | `tools` | 159,874 B | 159,874 B | 0 |
>    | `messages` | 56,515 B | 56,515 B | 0 |
>
>    `tools` y `messages` son **idénticos byte a byte**. La única diferencia
>    real de estar dentro del proyecto son **589 bytes** de `system`.
>
> 2. **Cuatro corridas repetidas** en el mismo directorio vacío, a través del
>    proxy: las cuatro cargaron los 4 servidores MCP y dieron `tools = 159,100 B`,
>    igual que el proyecto. El delta original desapareció.
>
> Los ~55,700 bytes de diferencia de la primera medición coinciden, dentro del
> error de re-serialización, con el costo de los tres conectores de Google
> (55,127 B — ver §5). En aquella corrida no se cargaron en el directorio vacío
> y sí en el proyecto. Se atribuyó a la memoria del proyecto una causa que era,
> simplemente, tres servidores MCP.
>
> **Lección metodológica:** un experimento de n=1 no distingue una causa de una
> coincidencia. Antes de atribuir un delta, hay que repetirlo y controlar las
> variables que no se están mirando.

Lo que sí quedó verificado de esta sección:

- `input_tokens` fue **idéntico** (7,368) en ambos casos: toda la diferencia
  vive en el prefijo, no en el contenido de la tarea.
- **Salvedad sobre el TTFT:** n=1 por escenario, **no probado** con
  repeticiones. No se afirma causalidad.
- **El piso del harness creció ~8.8× desde la última medición en este mismo
  proyecto.** Un benchmark anterior había registrado ~7,000 tokens de overhead;
  hoy, en un directorio **vacío**, el piso ya es 61,566 tokens. Ese crecimiento
  sí es real, y §5 y §9 lo decomponen: la superficie de herramientas es la
  causa dominante.

### 4.1. Qué hay realmente dentro del prefijo

Descomposición del body capturado (225,798 B), por bloque:

| bloque | bytes | % del body |
|---|---|---|
| `tools` (76 esquemas) | 159,874 | 70.8% |
| `CLAUDE.md` global, inyectado como `<system-reminder>` en `messages[0]` | 35,140 | 15.6% |
| volcado del hook `SessionStart` de Engram, en `messages[1]` | 19,668 | 8.7% |
| `system` (prompt del harness) | 8,928 | 4.0% |
| **el mensaje del usuario** | **75** | **0.03%** |

Dos observaciones que cuestan dinero:

- El `CLAUDE.md` global (34,922 bytes en disco) **no viaja en el bloque
  `system`**, como sería intuitivo, sino envuelto en un `<system-reminder>`
  dentro del primer mensaje del usuario. Buscarlo en `system` lleva a
  conclusiones equivocadas.
- El protocolo de Engram se vuelca **entero al arrancar la sesión**, use el
  agente la memoria o no. Es carga ansiosa: entra al prefijo y se relee en cada
  turno posterior. Una carga perezosa —solo el índice, y búsqueda bajo
  demanda— es la palanca obvia sobre esos 19,668 bytes.

Juntos, `CLAUDE.md` y el hook de Engram son **54,808 bytes: el 24.3% del body**,
pagados en cada petición de cada turno.

---

## 5. `--tools` contra `--disallowedTools`: dos palancas que no hacen lo mismo

Cuatro sondas, misma orden (`claude -p "Responde solo: ok"`), todas con
`--strict-mcp-config` (para aislar las herramientas nativas del harness de
cualquier servidor MCP) y todas comparadas con `context_messages_count = 2`
idéntico — la misma regla metodológica que en §4: solo se compara lo
comparable.

| sonda | `tools` (bytes) | nº herramientas | body medido (bytes) | `prepare_us` |
|---|---|---|---|---|
| A) `--strict-mcp-config` (nativas completas) | 86,198 | 29 | 149,221 | 4,774 |
| B) A + `--disallowedTools "Bash" "Edit" "Write"` | 85,777 | 28 | 148,800 | 5,284 |
| C) A + `--tools "Read" "Bash"` | 4,371 | 2 | 51,540 | 2,322 |
| D) A + `--tools ""` | 2 | 0 | 47,171 | 2,989 |

**Hallazgo 1 — la trampa conceptual, y por qué se dice primero.**
`--disallowedTools` no ahorra prácticamente nada: −421 B, un 0.5% del body.
Es una puerta de **permiso**, no de **payload**. El esquema completo de la
herramienta se sigue mandando en cada turno, se sigue pagando y el modelo lo
sigue leyendo; lo único que cambia es que tiene prohibido ejecutarla. Mucha
gente asume que negar una herramienta ahorra tokens. No es así. Es del tipo
de creencia que sobrevive años porque nadie la mide.

**Hallazgo 2 — `--tools` es la palanca real.**
`--tools <lista>` controla el array de esquemas en sí: −94.9% (86,198 B →
4,371 B; 29 herramientas → 2). Con `--tools ""` el array queda en 2 bytes —
los corchetes del array vacío, exactamente lo que predice el helper
`tools_overhead_bytes` para una lista vacía (ver `src/provider/mod.rs`); es
una validación cruzada de ese helper, no un número nuevo. Efectos
colaterales: `system` baja de 8,843 B a 8,673 B (el system prompt referencia
las herramientas disponibles), y `prepare_us` baja de 4,774 a 2,322 µs.

**Hallazgo 3 — el techo, apilando las dos palancas.**

| paso | body medido (bytes) | ahorro |
|---|---|---|
| Claude Code, sin cambios | 224,653 | — |
| + `--strict-mcp-config` (sin ningún servidor MCP, piso nativo) | 149,221 | −33.6% |
| + `--tools Read,Bash` | 51,540 | −77.1% |

> **Corrección de etiqueta (para no arrastrar un error).** El paso
> intermedio de esta tabla mide `--strict-mcp-config` **sin ningún servidor
> MCP cargado** — la misma sonda A de arriba —, no
> `.claude/mcp-lean.json`. Ese archivo carga Engram (ver
> `.claude/mcp-lean.json`), y cargar Engram **suma** bytes de `tools` en vez
> de restarlos: la fila "Solo Engram" de la tabla de MCP en el README mide
> 103,701 B de `tools`, más que los 86,198 B del piso nativo sin MCP. Si el
> paso real fuera `.claude/mcp-lean.json`, el body total sería MAYOR a
> 149,221 B, no menor. El 77% de la sección "El ceiling" es, entonces, MCP
> completamente apagado + selección de herramientas, no "MCP mínimo".

El 77.1% final del body es removible SI la tarea no necesita esas
herramientas.

**Hallazgo 4 — el trade-off, sin edulcorar.**
Un agente con solo `Read` y `Bash` no puede editar, buscar por patrón, ni
delegar a subagentes. Esos 86 kB de sonda A son el precio de la CAPACIDAD DE
ACTUAR. Cuando la tarea los necesita, ese peaje es el costo de tener un
agente, no grasa. Pero no toda tarea los necesita: un revisor que solo lee y
grepea, o un explorador de código, no tiene motivo para cargar 29 esquemas.
Vale la ironía: las definiciones de subagentes de este mismo workflow
(`jd-judge-a`, `Explore`) YA restringen sus herramientas — la palanca estaba
activa y nadie sabía cuánto valía. Cada subagente de solo lectura ahorra del
orden de 80 kB de prefijo en CADA UNO de sus turnos.

### 5.1. `--exclude-dynamic-system-prompt-sections`: una reubicación, no una reducción

El flag `--exclude-dynamic-system-prompt-sections` (default `false`) promete,
según su propia ayuda, "mover las secciones per-máquina (cwd, env info, rutas
de memoria, git status) del system prompt al primer mensaje de usuario" para
"mejorar la reutilización de caché entre usuarios". La pregunta que hay que
medir no es si funciona —lo hace—, sino **si achica el body o solo cambia de
lugar unos bytes**.

Dos sondas idénticas (`claude -p "Responde solo: ok"`, ambas con
`--strict-mcp-config` para congelar la superficie de herramientas), medidas en
la misma corrida del 2026-07-10 para que el delta sea limpio. La única
variable es el flag:

| componente | SIN flag | CON flag | delta |
|---|---|---|---|
| `context_system_bytes` | 6.923 | 3.409 | **−3.514** |
| `context_history_bytes` (`messages`) | 36.046 | 39.230 | **+3.184** |
| `context_tools_bytes` | 86.198 | 86.198 | 0 |
| `context_last_turn_bytes` | 17.781 | 17.781 | 0 |
| `context_other_bytes` | 353 | 353 | 0 |
| **`prompt_bytes` (body total)** | **147.432** | **147.102** | **−330 (−0,22%)** |

**Hallazgo 1 — el flag hace exactamente lo que dice.** Los ~3,5 kB de
secciones per-máquina salieron de `system` (−3.514 B) y reaparecieron en
`messages` (+3.184 B). El efecto es inequívoco: no es ruido de
re-serialización, es la reubicación anunciada. (Cross-check: `tools` quedó en
86.198 B, idéntico byte a byte a la sonda A de §5 —la misma superficie nativa
sin MCP—, lo que confirma que `--strict-mcp-config` congeló esa variable.)

**Hallazgo 2 — como palanca de TAMAÑO, es ruido.** El neto que de verdad
desaparece del body son **330 bytes: un 0,22%**. El flag no está pensado para
achicar el prefijo, y no lo achica de forma apreciable. Mover bytes de un
bloque a otro no ahorra window de contexto, ni prefill, ni rate limit: el
modelo sigue tokenizando la misma cantidad de texto.

**Hallazgo 3 — el beneficio real (cacheabilidad) es cross-usuario, y aquí es
inerte.** El propósito del flag es que el bloque `system` quede idéntico entre
máquinas y usuarios distintos, para que compartan el mismo prefijo cacheado.
Para un **único usuario** no hay con quién compartir esa caché. Es el mismo
patrón que la Palanca A del optimizador de prompt cache
(`docs/optimizer-prompt-cache.md` §6): mecanismo correcto, apuntando a un
escenario que este uso no tiene.

Queda un posible beneficio **local** sin medir: entre sesiones del mismo
usuario, si cambia el `cwd` o el `git status`, mantenerlos fuera de `system`
deja ese bloque estable entre corridas y podría alargar el cache-hit. Pero eso
choca con la misma tenaza que la Palanca A —no se puede verificar que Anthropic
honre el prefijo sin créditos de API (ver `docs/optimizer-prompt-cache.md`
§6.2)—, así que se anota como mecanismo plausible, **no medido**.

> **Salvedad de n.** n=1 por escenario. A diferencia del TTFT (§3, §8), esto no
> es una medición ruidosa sino la composición de un body **determinista** dado
> el mismo input; la reubicación (−3.514 / +3.184) es demasiado nítida para ser
> azar. No se afirma nada de latencia a partir de estas dos sondas.

---

## 6. La memoria persistente no es compresión, es una inyección

Un sistema de memoria persistente (p. ej. Engram) **no reduce** el contexto
que se envía: **lo aumenta**. El protocolo de instrucciones en `CLAUDE.md`,
los volcados de memoria al arrancar sesión, y los resultados de búsqueda de
memoria — todo eso aterriza como tokens de prefijo, y el prefijo se relee
completo en cada turno (ver la mecánica de N² en §2).

Ejemplo medido: el `CLAUDE.md` global por sí solo son ~8,730 tokens. A lo
largo de los 28 turnos de la sesión medida, eso son **244,440 tokens
re-leídos** solo por ese archivo — antes de sumar ningún otro contenido de
memoria.

Esto no es necesariamente malo: es un **trade**, no un ahorro automático.

- **Gana** cuando 500 tokens de memoria evitan releer cinco archivos enteros
  (o rehacer una búsqueda cara) en cada turno.
- **Pierde** cuando inyecta 9k tokens de protocolo que no tienen nada que ver
  con la tarea actual del turno.

**Remedio propuesto (no implementado):** carga LAZY en vez de EAGER — cargar
solo el índice de memoria al arrancar sesión, y buscar el contenido completo
recién cuando el turno lo necesita, en vez de volcar todo el protocolo y el
contexto de memoria al principio de cada sesión.

---

## 7. La salvedad del costo nocional

El tráfico medido acá se autentica vía **OAuth de Claude Max** — una
suscripción plana, no facturación por token. `cost_estimate_usd` es
**nocional**: "lo que esto hubiese costado en la API de pago por token", no
un cargo real. Para este usuario, en este momento, **la moneda real es
tiempo y rate limits**, no dólares. Los números en USD de este documento
sirven para comparar componentes entre sí (qué proporción es contexto vs.
output), no para leerlos como una factura real.

---

## 8. Hipótesis descartadas (para que nadie las reintente)

Documentarlas cuesta tan poco como la sección de hallazgos y ahorra volver
a investigar lo mismo:

- **DESCARTADA con datos: "el `cache_write` alto viene del TTL de 5 minutos
  venciendo".** Falso. La fila de las 11:55:40 tuvo un gap de 367 s (>300 s)
  contra la fila anterior y aun así llegó con `cache_read = 124,733` — es
  decir, el prefijo seguía cacheado. Las lecturas refrescan el TTL. El
  `cache_write` alto se explica por (a) una escritura fría masiva de 82,947
  tokens al arrancar y (b) ~1.7k tokens incrementales de `cache_write` por
  turno normal, no por vencimientos repetidos.
- **SIN PROBAR, no afirmar como hallazgo: "un gap de inactividad más allá
  del `pool_idle_timeout` de 90 s de `reqwest` infla el TTFT por un nuevo
  handshake TLS".** El delta mediano fue de solo +148 ms sobre n=5, con un
  spread de 1,291–5,888 ms — y la fila con el gap más frío (367 s) tuvo uno
  de los TTFT más **rápidos** (1,291 ms), justo lo contrario de lo que la
  hipótesis predice. Necesita un test controlado antes de afirmarse. Nota
  aparte: `reqwest::Client` se construye una sola vez en `main.rs` y vive en
  `AppState`, así que el pool de conexiones ya se reutiliza entre requests
  — la hipótesis, si fuera cierta, tendría que operar a pesar de ese reuso,
  no por falta de él.

---

## 9. Lo que OxideGate no podía ver, y por qué existe este documento

> **Corrección de estado.** Esta sección decía, en una versión anterior de
> este documento, que descomponer `prompt_bytes` por componente era "la
> siguiente feature natural" — pendiente. Ya no lo es: `context_system_bytes`,
> `context_tools_bytes`, `context_history_bytes`, `context_last_turn_bytes` y
> `context_other_bytes` existen hoy en `RequestMetric`
> (`src/provider/mod.rs`, `src/middleware/proxy.rs`) y están en cada fila de
> `telemetry.jsonl`. §5 de este mismo documento es la primera vez que se
> explotan para un hallazgo. Se deja la sección para que quede registrado
> qué preguntaba este documento ANTES de tener esa descomposición, y para no
> perpetuar el "pendiente" en el README (ver ahí, sección Roadmap).

`prompt_bytes` seguía siendo, hasta esa descomposición, un número plano,
único: el proxy tenía el body completo del request en sus manos, pero no lo
partía por componente. Partirlo (`system` / `tools` / historial / turno
actual) era el paso que faltaba para que la pregunta que motiva todo este
documento — **"de lo que pago, ¿cuánto es trabajo y cuánto es ceremonia?"**
— se pudiera responder con datos, no con estimaciones como el `CLAUDE.md` de
§4/§6.

---

## 10. Relación con Metronous

[Metronous](https://github.com/kiosvantra/metronous) es complementario, no
competidor — mide en una capa distinta. Metronous vive **adentro del
agente** (plugin de OpenCode + shim MCP + daemon en Go + SQLite), ve
sesiones y llamadas a herramientas, y calcula ROI como
`accuracy / avg_cost_per_session`. OxideGate vive **en el wire** y ve bytes
y tokens exactos de cada request/response.

Ninguno de los dos solo puede responder, hoy, "¿este cambio de modelo me
hizo más rápido y más barato para LA MISMA TAREA?". Responderla exigiría
correlacionar ambas capas por un `session_id` compartido — y ninguno de los
dos lo expone todavía.

---

## 11. Fuentes de estos números

Todo lo de §2–§4 sale de filas reales de
`~/.config/oxidegate/telemetry.jsonl` filtradas por
`"model":"claude-opus-4-8"`, agregadas con los multiplicadores de
`src/telemetry/pricing.rs` (`price_in: 15.0`, `price_out: 75.0`,
`ANTHROPIC_CACHE_READ_MULTIPLIER = 0.1`,
`ANTHROPIC_CACHE_WRITE_MULTIPLIER = 1.25`). El `prompt_hash` que hace
posible detectar redundancia (§2, y ver `docs/optimizer-dedup.md`) se
calcula en `provider::fingerprint`, sobre `incoming.body` — el body
ORIGINAL, antes de cualquier mutación del proveedor.

Los números de §5 (las cuatro sondas de `--tools`/`--disallowedTools`, la
tabla del techo apilado) salen de las cuatro últimas filas de ese mismo
`telemetry.jsonl` al 2026-07-09, capturadas entre 16:06:19 y 16:06:35 UTC —
sin pasar por `pricing.rs`, porque son campos crudos de bytes y microsegundos
(`context_tools_bytes`, `context_measured_bytes`, `prepare_us`), no cálculos
de costo. Se verificaron byte a byte contra ese archivo antes de publicarse
acá.
