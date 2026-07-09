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
  explica esa dispersión por sí sola — ver hipótesis descartada en §7 sobre
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
  autentica con una suscripción plana (ver §5): ahí el techo no lo pone el
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

- `input_tokens` fue **idéntico** (7,368) en ambos casos. Toda la diferencia
  vive en el prefijo cacheado (`cache_write_tokens`), no en el input medido
  — confirma que el overhead de estar "dentro del proyecto" es 100% contexto
  de herramientas/memoria, cero contenido de la tarea.
- **Salvedad honesta sobre el TTFT:** esta comparación es n=1 por escenario.
  Es consistente en dirección con un `cache_write` más grande (más para
  escribir, más tarda en confirmarse), pero **no está probado** con
  repeticiones — no se afirma causalidad, solo correlación observada una vez.
- **El piso del harness creció ~8.8× desde la última medición en este mismo
  proyecto.** Un benchmark anterior había registrado un overhead de ~7,000
  tokens; hoy, en un directorio **vacío** (sin nada de OxideGate todavía),
  el piso ya es 61,566 tokens. Atribuible a definiciones de herramientas,
  servidores MCP conectados, el índice de skills, y el `CLAUDE.md` global
  del usuario (34,922 bytes, ~8,730 tokens) — ese archivo solo explica una
  fracción del crecimiento, el resto es superficie de herramientas/MCP que
  no se decompone hoy (ver §8).

---

## 5. La memoria persistente no es compresión, es una inyección

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

## 6. La salvedad del costo nocional

El tráfico medido acá se autentica vía **OAuth de Claude Max** — una
suscripción plana, no facturación por token. `cost_estimate_usd` es
**nocional**: "lo que esto hubiese costado en la API de pago por token", no
un cargo real. Para este usuario, en este momento, **la moneda real es
tiempo y rate limits**, no dólares. Los números en USD de este documento
sirven para comparar componentes entre sí (qué proporción es contexto vs.
output), no para leerlos como una factura real.

---

## 7. Hipótesis descartadas (para que nadie las reintente)

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

## 8. Lo que OxideGate no puede ver hoy, y por qué existe este documento

`prompt_bytes` es un número plano, único. El proxy tiene el body completo
del request en sus manos, pero nunca lo decompone. La siguiente feature
natural es partir ese body por componente (`system` / `tools` / historial /
turno actual) para que la pregunta que motiva todo este documento —
**"de lo que pago, ¿cuánto es trabajo y cuánto es ceremonia?"** — se pueda
responder con datos, no con estimaciones como el `CLAUDE.md` de §4-§5.

---

## 9. Relación con Metronous

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

## 10. Fuentes de estos números

Todo lo de §2–§4 sale de filas reales de
`~/.config/oxidegate/telemetry.jsonl` filtradas por
`"model":"claude-opus-4-8"`, agregadas con los multiplicadores de
`src/telemetry/pricing.rs` (`price_in: 15.0`, `price_out: 75.0`,
`ANTHROPIC_CACHE_READ_MULTIPLIER = 0.1`,
`ANTHROPIC_CACHE_WRITE_MULTIPLIER = 1.25`). El `prompt_hash` que hace
posible detectar redundancia (§2, y ver `docs/optimizer-dedup.md`) se
calcula en `provider::fingerprint`, sobre `incoming.body` — el body
ORIGINAL, antes de cualquier mutación del proveedor.
