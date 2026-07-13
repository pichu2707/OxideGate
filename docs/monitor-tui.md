# Monitor TUI — Velocidad por modelo, en vivo, con ANTES/DESPUÉS

> Herramienta: `src/bin/monitor.rs`. Cliente de terminal que consume
> `GET /stats` y muestra, en tiempo real, el efecto de una optimización
> (p. ej. forzar `cache_control`) sobre el throughput y la latencia por
> modelo.

---

## 1. Qué es y qué NO es

Es un **cliente HTTP separado del proxy**: pollea `GET /stats` cada ~1
segundo, igual que haría `curl` en loop, y pinta lo que recibe. Además,
pollea `GET /requests` (`docs/telemetry-per-request.md`) para el panel de
detalle por petición (ver §7). No lee `telemetry.jsonl`, no conoce el
acumulador interno (`src/telemetry/stats.rs`) más allá del contrato JSON de
ambos endpoints, y no toca la captura de métricas. Se puede matar y volver a
levantar sin afectar al proxy — es un observador, no una dependencia.

Requisito: **OxideGate tiene que estar corriendo** con `/stats` disponible
(ver `docs/telemetry-by-model.md`). Si no lo está, el monitor no crashea: lo
avisa en pantalla y sigue reintentando cada poll.

## 2. Cómo se lanza

```bash
cargo run --bin oxidegate-monitor
```

O ya compilado:

```bash
cargo build --release --bin oxidegate-monitor
./target/release/oxidegate-monitor
```

### URL del endpoint

En orden de prioridad:

1. flag `--url <url>`
2. env `OXIDEGATE_STATS_URL`
3. `http://127.0.0.1:{OXIDEGATE_PORT}/stats` (`OXIDEGATE_PORT` default `8080`,
   el mismo default que usa el proxy — ver `src/config.rs`)

```bash
# proxy en el puerto default
cargo run --bin oxidegate-monitor

# proxy en otro puerto
OXIDEGATE_PORT=8899 cargo run --bin oxidegate-monitor

# URL explícita
cargo run --bin oxidegate-monitor -- --url http://127.0.0.1:8899/stats
```

### Modo headless: `--once`

```bash
cargo run --bin oxidegate-monitor -- --once
```

Hace UN solo fetch de `/stats` y otro de `/requests`, e imprime todo en texto
plano (sin raw mode, sin pantalla alternada) y sale con código `0`. Sirve para
scripts, CI, o para pegar resultados en una conversación sin entrar a la TUI.
Igual que el modo interactivo, si el proxy está caído no crashea: imprime
`proxy no disponible en {url}` y sale limpio.

Para `/requests`, `--once` imprime **las dos vistas, una debajo de la otra**
(`Latency` y luego `Context`, cada una con su propio header) — no hay forma
de apretar `c` en un snapshot que ya salió, así que ambas salen siempre. Ver
§7.1 para qué muestra cada vista.

## 3. El flujo ANTES/DESPUÉS (el punto central de la herramienta)

La pregunta que responde: *"¿la palanca que acabo de prender realmente
mejoró la velocidad/latencia de ESTE modelo?"* Sin esto, tendrías que
comparar promedios acumulados desde el arranque del proxy — contaminados por
todo el tráfico previo.

Pasos:

1. Levante el proxy con la optimización **apagada** (p. ej. `force_cache`
   off — ver `docs/optimizer-prompt-cache.md`).
2. Genere algo de tráfico normal para ese modelo.
3. En el monitor, elija el modelo con `↑`/`↓` y pulse **`b`** para marcar el
   baseline (contadores crudos acumulados en ese instante).
4. Active la optimización (p. ej. `OXIDEGATE_FORCE_CACHE=true` y reinicie el
   proxy, o el mecanismo que corresponda).
5. Siga generando tráfico. El panel **ANTES/DESPUÉS** muestra el delta
   desde el baseline: throughput de la ventana (tok/s), TTFT de la ventana,
   cache-hit de la ventana, Δcoste, Δrequests, Δoutput_tokens y error% de la
   ventana — todo calculado sobre lo que pasó **después** de marcar `b`, no
   sobre el histórico completo.
6. `r` resetea el baseline para volver a arrancar la medición.

### Por qué no se promedian dos promedios

`/stats` ya expone `avg_ttft_ms` y `avg_tokens_per_sec`, pero promediar el
valor viejo y el nuevo sería matemáticamente incorrecto: el número de
requests que aportó a cada promedio pudo cambiar entre polls. Por eso el
snapshot ahora expone también las **sumas y counts crudos** (`ttft_ms_sum`,
`ttft_ms_count`, `total_ms_sum`, `tokens_per_sec_sum`,
`tokens_per_sec_count`, `errors` — ver §5) y el monitor calcula el promedio
de la ventana como `Δsuma / Δcount`, que sí es correcto.

## 4. Teclas

| Tecla | Acción |
|---|---|
| `q` / `Esc` | Salir |
| `b` | Marcar baseline (para el panel ANTES/DESPUÉS) |
| `r` | Resetear baseline |
| `↑` / `↓` | Elegir el modelo (fila resaltada, afecta el panel ANTES/DESPUÉS y los sparklines) |
| `p` | Mostrar/ocultar el panel de requests recientes (ver §7) |
| `c` | Ciclar la vista de columnas del panel de requests recientes — `Latency` ⇄ `Context` (ver §7.1). **No-op si el panel está oculto**: no cambia nada mientras `p` lo tenga escondido |
| `s` | Mostrar/ocultar el panel de tools por servidor (ver §8). **INDEPENDIENTE** de `p`/`c`: ninguna de las tres teclas afecta el estado de las otras |
| `u` | Mostrar/ocultar el panel de cuota de suscripción Codex — "uso de cuota" (ver §9). **INDEPENDIENTE** de `p`/`c`/`s` |

## 5. Layout de la pantalla

1. **Header**: título, URL del endpoint, estado del último fetch ("ok · N
   modelos" o "proxy no disponible en..."), y edad del baseline ("baseline
   hace 12s" o "sin baseline — pulse 'b'").
2. **Tabla principal**, una fila por `(upstream, model)`, TOTAL acumulado
   desde que el proxy arrancó: `MODELO | REQ | tok/s | TTFT ms | cache-hit |
   coste $ | redun%`. Fila seleccionada resaltada.
3. **Panel ANTES/DESPUÉS**: delta de ventana del modelo seleccionado desde
   el baseline (ver §3). Si no hay baseline, muestra el aviso para marcarlo.
4. **Sparklines**: throughput (tok/s) y TTFT (ms) del modelo seleccionado a
   lo largo del tiempo, últimas ~120 muestras (~2 minutos a 1 poll/seg).
5. **Panel de requests recientes** (toggleable con `p`, ver §7): las últimas
   peticiones individuales, más nueva arriba, con marcadores de outlier, en
   una de dos vistas cicladas con `c` (`Latency` o `Context`, ver §7.1).
6. **Panel de tools por servidor** (toggleable con `s`, ver §8): desglose de
   bytes de herramientas por servidor MCP de la petición más reciente que
   los declare, con delta contra el baseline. Independiente del panel
   anterior — cualquier combinación de `p`/`s` visibles u ocultos es válida.
7. **Panel de cuota de suscripción Codex** (toggleable con `u`, ver §9):
   gauge de estado de cuenta (no una tabla por fila) de la petición más
   reciente que traiga cabeceras `x-codex-*`. Independiente de los paneles
   anteriores.
8. **Footer**: recordatorio de teclas.

## 6. Enhance del snapshot (`ModelStatsRow`)

Cambio aditivo y retrocompatible en `src/telemetry/stats.rs`: `ModelStatsRow`
ahora expone, además de los promedios que ya tenía, las sumas/counts crudas
que los originan:

| Campo nuevo | Qué es |
|---|---|
| `ttft_ms_sum` | Suma cruda de `ttft_ms` acumulada |
| `ttft_ms_count` | Cantidad de requests que aportaron TTFT (puede ser < `requests`) |
| `total_ms_sum` | Suma cruda de `total_ms` (count == `requests`) |
| `tokens_per_sec_sum` | Suma cruda de `tokens_per_sec` |
| `tokens_per_sec_count` | Cantidad de requests que aportaron `tokens_per_sec` |
| `errors` | Cantidad cruda de requests con `status >= 400` |

Ningún campo existente cambió de significado ni de tipo; cualquier
consumidor de `/stats` que ya exista sigue funcionando igual.

## 7. Panel de requests recientes (`p`)

Consume `GET /requests` (`docs/telemetry-per-request.md`): las últimas
peticiones individuales atendidas por el proxy, no un agregado. Sirve para
ver la fila puntual que un promedio esconde — el cache-miss aislado, el TTFT
que se disparó una sola vez. Se alterna con la tecla `p`; arranca visible.

### 7.1. Dos vistas, una tecla (`c`)

El panel ya tiene ~12 columnas — cramear más ahí lo haría ilegible. En vez de
eso, el panel tiene **dos vistas mutuamente excluyentes**, cicladas con `c`:

| Vista | Para qué sirve | Se muestra con... |
|---|---|---|
| `Latency` (default) | Latencia, tokens y coste por request — la que ya existía | `q`/`b`/`r` recién arrancado el monitor |
| `Context` | Desglose de bytes de contexto por request: cuánto pesa cada bucket del body (`tools`, `history`, `system`, `last_turn`, `other`) | apretando `c` una vez |

El título del panel muestra la vista activa (`vista:latency` /
`vista:context`) y el estado del último poll a `/requests`.

**`c` es un no-op si el panel está oculto** (`p` lo escondió): no tiene
sentido cambiar qué columnas se muestran en algo que no se está mostrando, y
hacerlo igual dejaría un cambio de estado invisible hasta volver a mostrar el
panel — así que directamente no pasa nada. Muestre el panel de nuevo con `p`
para poder ciclar la vista.

### 7.2. Columnas — vista `Latency`

Tabla con **más nueva arriba** (al revés que el orden cronológico en que
llega el JSON, que es más vieja primero — el monitor invierte para
presentación):

| Columna | Qué muestra |
|---|---|
| `hora` | `HH:MM:SS` UTC extraído del timestamp RFC 3339 |
| `modelo` | Modelo solicitado, truncado a 16 caracteres; `-` si no venía en el body |
| `st` | `y`/`n` — si el request pidió streaming |
| `status` | Código HTTP devuelto al cliente |
| `in` / `out` | Tokens de entrada/salida exactos |
| `c_rd` / `c_wr` | Tokens de caché leídos/escritos |
| `ttft_ms` | Time To First Token en ms |
| `gen_ms` | Tiempo de generación, `total_ms - ttft_ms` |
| `tok/s` | Throughput de generación, `output_tokens / (gen_ms / 1000)` |
| `usd` | Coste estimado |
| `outlier` | Marcadores de esta fila (ver abajo), p. ej. `ERR+TTFT` |

Un valor ausente se muestra como `-`, **nunca como `0`**: un `0` real (p. ej.
0 tokens de caché) y un dato que no llegó son cosas distintas, y confundirlos
lleva a conclusiones equivocadas sobre qué está pasando.

### 7.3. Columnas — vista `Context`

Mismo orden (más nueva arriba) y los mismos marcadores de outlier que la
vista `Latency`; lo único que cambia es qué mide cada columna. Todos los
campos de bytes vienen de `RecentRequest`/`ContextBreakdown` — ver
`docs/telemetry-per-request.md` para el contrato completo del endpoint.

| Columna | Qué muestra | Qué SEÑALA |
|---|---|---|
| `hora` / `modelo` | Igual que en `Latency` | — |
| `msgs` | `context_messages_count`: cantidad de mensajes del historial completo | Una conversación que crece sin recortar prefijo: `history` va a subir en proporción |
| `tools` | Bytes del esquema de herramientas (`context_tools_bytes`) | **La columna más importante de este slice.** Si `tools` domina el body en TODAS las filas, es un candidato directo a desconectar servidores MCP que este proyecto no usa — en tráfico real medido, `tools` llegó a ser ~71% del body |
| `history` | Bytes de todos los mensajes menos el último (`context_history_bytes`) | Crece con la conversación; si domina sobre `tools`, el prefijo de historial es el costo principal, no el catálogo de herramientas |
| `system` | Bytes del prompt de sistema (`context_system_bytes`) | Estable entre requests del mismo cliente; un salto brusco sugiere que cambió el system prompt |
| `last_turn` | Bytes del último mensaje, el turno genuinamente nuevo (`context_last_turn_bytes`) | Esto es lo ÚNICO que el usuario "escribió ahora". En tráfico real medido, puede ser tan poco como 0.06% del body — el resto es reenviar lo mismo de siempre |
| `other` | Bytes del resto de campos de control a nivel raíz (`context_other_bytes`) | Normalmente chico; si crece, revisar qué campos nuevos está mandando el cliente |
| `total` | Suma de los cinco anteriores (`context_measured_bytes`) — BYTES de JSON canónico re-serializado, **nunca tokens**, y **nunca combinar con el tamaño de wire** (ver `docs/telemetry-per-request.md`) | El tamaño total que el proxy mide por request |
| `tax%` | `context_tax_ratio * 100`, un decimal — `(system + tools + history) / total` | **Cercano a 100% ⇒ casi todo lo enviado ya se había enviado antes.** Es la "tasa" pagada por turno solo para repetir contexto; un `tax%` alto con `cache-hit` bajo (ver vista `Latency`/tabla principal) es la peor combinación posible |
| `B/tok` | `context_measured_bytes / prompt_tokens_total(fila)`, un decimal — ver §7.3.1 para qué es `prompt_tokens_total` y por qué el denominador NO es siempre `input_tokens` | Un valor **muchas veces más alto que sus vecinos en la misma columna de modelo** es el olfato de que algo se truncó — el marcador `TRUNC` (§7.4) lo CONFIRMA cuando hay >= 2 filas con las que probarlo; `B/tok` es la escotilla de escape para el caso de una sola fila, donde `TRUNC` no puede probar nada |
| `prep_us` | Microsegundos que el proxy pasó dentro de `Provider::prepare` (parseo + `decompose` + mutación opcional) | Overhead propio de OxideGate, NO incluye leer el body del socket ni el round-trip al proveedor — si esto crece con el tamaño del body, el parseo/decompose es el cuello de botella, no la red |
| `outlier` | Igual que en `Latency` | — |

Los bytes se muestran en formato compacto vía `format_bytes` (ver
`src/bin/monitor.rs`): **convención DECIMAL** (base 1000, no binaria
KiB/MiB) — `159123 B` se ve como `159.1 kB`, `281 B` se ve tal cual. Se
eligió decimal porque mide tamaño de un JSON re-serializado, no bloques de
memoria alineados a potencias de 2.

`tax%` se muestra como `-` (nunca `0.0`) cuando `context_tax_ratio` es
`None` — mismo criterio de "ausente ≠ cero" que el resto del panel.

### 7.3.1. `B/tok` y el denominador dependiente del dialecto

`B/tok` divide `context_measured_bytes` (bytes del body, ver §7.3) por
`prompt_tokens_total(fila)` — **no** por `input_tokens` a secas. La razón es
que cada proveedor contabiliza los tokens de caché distinto (ver
`src/telemetry/pricing.rs::CacheAccounting`, que este binario NO puede
importar porque el crate no expone `lib.rs` — `src/bin/monitor.rs` define su
propia copia de esta lógica en `prompt_tokens_total`, documentada como
DUPLICACIÓN DELIBERADA que hay que mantener sincronizada a mano si
`pricing.rs` cambia):

| `upstream` | `prompt_tokens_total` |
|---|---|
| `anthropic` | `input_tokens + cache_read_tokens + cache_write_tokens` (la caché va APARTE del input medido) |
| cualquier otro (`openai`, `gemini`, y cualquier proveedor compatible con su API — p. ej. Ollama vía el provider `openai`) | `input_tokens` solo (`cache_read` ya es SUBCONJUNTO de `input_tokens`; sumarlo encima doblaría el conteo) |

**El gotcha que motiva esta tabla:** un request de Claude Code con un
cache-hit grande puede reportar `input_tokens = 2` con un body de 224.653 B.
Un detector naïve `bytes / input_tokens` da ~112.326 B/tok — un número que
gritaría "truncamiento" en el request MÁS SANO posible. Sumando
`cache_read_tokens` (124.733) y `cache_write_tokens` (1.355), el denominador
real es 126.090 y el ratio cae a ~1,8 B/tok, coherente con el resto del
tráfico Anthropic. **No "simplificar" este cálculo a `input_tokens` puro** —
es exactamente el error que produciría el falso positivo catastrófico.

Valores **observados** (no universales — cada tokenizer da un ratio propio,
no hay una constante que sirva para todos los proveedores):

| Proveedor/modelo | `B/tok` sano observado |
|---|---|
| Anthropic (Claude) | ~2,7 |
| llama.cpp / Ollama (`llama3.2:3b`) | ~4,1 |

`B/tok` se muestra como `-` (nunca `0.0`) cuando falta `input_tokens`, falta
`context_measured_bytes`, o `prompt_tokens_total` da `0` — un denominador
indefinido nunca se colapsa a un número inventado.

### 7.4. Marcadores de outlier

Aplican por igual a AMBAS vistas (`Latency` y `Context`): la clasificación de
outliers no depende de qué columnas se estén mirando.

Cada fila se compara solo contra las OTRAS filas del mismo `(upstream,
model)` — nunca contra el total del panel. El color de fila (rojo si tiene
algún marcador) es solo refuerzo visual: el texto del marcador es la señal
real, para que no se pierda en terminales sin color.

| Marcador | Condición exacta | Qué señala |
|---|---|---|
| `ERR` | `status >= 400` | Siempre se flaggea, sin estadística ni mínimo de muestra: un error no necesita comparación para ser relevante |
| `MISS` | `cache_read_tokens` es `None`/`0` mientras **al menos la mitad** de las OTRAS filas del mismo grupo sí tuvieron `cache_read_tokens > 0` | En una conversación larga el prefijo estable debería venir de caché; un miss aislado es caro y, en un promedio, invisible |
| `TTFT` | `ttft_ms >= media + 2σ` del grupo | TTFT muy por encima de lo normal para ese modelo |
| `SLOW` | throughput de generación (`output_tokens / (gen_ms / 1000)`) `<= media - 2σ` del grupo | Generación mucho más lenta que el resto del mismo modelo |
| `TRUNC` | Dentro del mismo grupo `(upstream, modelo)`, esta fila comparte el MISMO `prompt_tokens_total` (§7.3.1) que al menos otra fila, y sus `context_measured_bytes` difieren entre sí en >= 5% (`TRUNCATION_BYTES_DELTA`, fracción del body más grande del par) | El proveedor dejó de contar el prompt al llegar a un tope (`num_ctx` de Ollama, ventana de contexto del modelo, etc.) y lo truncó EN SILENCIO, devolviendo `200 OK` igual — el modelo nunca vio buena parte de lo que se le mandó. **Qué hacer:** subir `OLLAMA_CONTEXT_LENGTH` (o el equivalente del proveedor), o recortar `tools`/`history` — la ventana del modelo local puede ser más chica que el catálogo de herramientas por sí solo |

Una fila puede llevar más de un marcador a la vez (p. ej. `ERR+TTFT` o
`TRUNC+SLOW`): no se colapsa a uno solo porque eso escondería información
real.

**Por qué `TRUNC` no es un umbral de bytes-por-token:** un detector naïve
`bytes / input_tokens > constante` dispara sobre casi cualquier request sano
de Anthropic con cache-hit (ver el gotcha de §7.3.1, `input_tokens = 2` con
un body de 224.653 B). `TRUNC` en cambio prueba algo que NO necesita ninguna
constante de bytes-por-token: que el MISMO total de tokens aparezca en >= 2
bodies de tamaño MATERIALMENTE distinto no es casualidad, es la firma de que
el proveedor dejó de contar.

**Por qué el umbral es una FRACCIÓN del body y no un piso absoluto de
bytes:** si un body crece en `ΔB` bytes y el total de tokens reportado no se
mueve, esos tokens que "faltan" son aproximadamente `ΔB / (bytes por
token)`. Como `total_bytes ≈ (bytes por token) × tokens`, el delta RELATIVO
de bytes es aproximadamente la fracción del prompt que desapareció sin
contarse — `(max_bytes - min_bytes) / max_bytes >= 5%` significa
literalmente "al menos un 5% del prompt se perdió en silencio". Eso escala
correctamente con el tamaño del body: el ruido de serialización (un UUID, un
timestamp, un request id) es una fracción cada vez más chica cuanto más
grande es el prompt — exactamente como se espera de ruido —, mientras que un
piso absoluto de bytes trataría igual "500 B de ruido en un body de 1 kB"
que "500 B de ruido en un body de 200 kB", que son señales completamente
distintas. Un grupo donde todos los bodies miden prácticamente lo mismo
(probes repetidos con el mismo prompt) NO flaggea: coincidir en tokens Y en
bytes ahí es justamente lo esperado.

**Calibración de `TRUNCATION_BYTES_DELTA = 0.05`:** el valor original (0.10)
se fijó mirando un solo caso observado que difería en ~34% (18.955 B vs.
28.806 B, ambos con `input_tokens = 4095`) y produjo un falso negativo
medido sobre tráfico real: dos requests de OpenCode contra un Ollama local
(`llama3.2:3b`, `num_ctx = 4096`) reportaron EXACTAMENTE 4095 tokens de
prompt con bodies de 77.579 B y 84.161 B — una diferencia real de
truncamiento del 7,8%, por debajo del 10% exigido, que el detector dejaba
pasar. `0.05` cubre ambos casos reales observados (7,8% y ~34%) con margen,
y sigue muy por encima de la banda de ruido de serialización (fracciones de
punto porcentual).

### 7.5. Guardas estadísticas (y por qué existen)

- **`MIN_GROUP_SAMPLE = 5` muestras VÁLIDAS por métrica**, no solo el tamaño
  del grupo. Antes de flaggear `TTFT`, `SLOW` o `MISS`, hace falta que al
  menos 5 filas del grupo tengan el dato que esa métrica necesita (excluyendo
  `None`). `ERR` está exento: no depende de estadística. Motivo: con muestras
  chicas el desvío estándar no significa nada y cualquier fila parece
  atípica — un panel que grita "outlier" desde la segunda fila enseña a
  ignorarlo.
- **Desvío estándar poblacional** (divisor `n`, no `n-1`): coherente con que
  acá se compara la ventana completa observada, no una muestra de una
  población más grande.
- Un grupo con desvío estándar **cero o no finito** no produce ningún
  marcador estadístico: sin variación real, cualquier flag sería ruido.
- Las filas **sin streaming** (`total_ms - ttft_ms <= 0`) se excluyen por
  completo del throughput de generación — de la media del grupo Y del propio
  cálculo del marcador — para no contaminar la media con filas que no miden
  lo mismo.
- Los valores `None` se excluyen de la media/desvío de su métrica; nunca se
  coercionan a `0` (un `0` real distorsionaría el cálculo tanto como uno
  falso).
- **`TRUNC` es la EXCEPCIÓN a `MIN_GROUP_SAMPLE`.** No es un test estadístico
  (no calcula media ni desvío): la prueba es una igualdad exacta de tokens
  más una diferencia de tamaño de body que ya de por sí prueba el tope, y esa
  prueba es igual de válida con 2 muestras que con 50. Exigir 5 filas acá
  escondería el caso real que motivó este detector (dos probes bastan). Filas
  sin `input_tokens` o sin `context_measured_bytes` se EXCLUYEN del análisis
  de `TRUNC` (no se tratan como cero), y una fila SOLA con un total de tokens
  que nadie más repite NUNCA se flaggea — un solo dato no prueba nada, podría
  ser genuinamente un prompt grande.

### 7.6. URL de `/requests`

Se deriva de la URL de `/stats` ya resuelta, con esta prioridad:

1. env `OXIDEGATE_REQUESTS_URL` (override explícito)
2. la URL de `/stats` ya resuelta, con el sufijo `/stats` reemplazado por
   `/requests` — así ambos endpoints apuntan al mismo host/puerto sin
   duplicar configuración
3. si la URL de `/stats` no termina en `/stats` (p. ej. vino de un `--url`
   atípico), no hay forma segura de derivarla: cae al default
   `http://127.0.0.1:{OXIDEGATE_PORT|8080}/requests`

### 7.7. Degradación si `/requests` no está disponible

`/requests` es un endpoint más nuevo que `/stats`: un proxy de build
anterior puede no tenerlo todavía. Si el fetch falla, el monitor:

- conserva el último snapshot bueno de `recent_requests` (no lo vacía),
- muestra el estado del fallo en el título del panel,
- deja el resto de los paneles (tabla de agregados, ANTES/DESPUÉS,
  sparklines) funcionando con total normalidad, porque el poll de `/stats` y
  el de `/requests` son independientes entre sí.

## 8. Panel de tools por servidor (`s`)

Consume el mismo `GET /requests` que el panel de requests recientes (§7),
pero mira un campo distinto: `tools_by_server` (desglose de
`context_tools_bytes` por servidor MCP — ver `docs/telemetry-per-request.md`
§4.2 para el contrato completo del campo). Responde una pregunta que el
panel de requests no responde por sí solo: *"¿cuánto de mi contexto es un
servidor MCP que ni uso, y cuánto bajaría si lo desconecto?"*

Se alterna con la tecla `s`, **INDEPENDIENTE** de `p` (panel de requests) y
de `c` (columnas de ese panel): las tres teclas controlan estados
ortogonales, cualquier combinación de visibilidad es válida.

### 8.1. Fuente de datos

El panel no promedia ni acumula nada: toma la fila MÁS RECIENTE de
`/requests` cuyo `tools_by_server` sea no-nulo y no vacío, y muestra
exactamente esa fila. El título del panel indica `HH:MM:SS` y modelo de esa
fila fuente, para que quede claro a qué petición puntual corresponde el
desglose.

Si ninguna fila califica — proxy de una build anterior a este campo, o
ninguna petición reciente declaró herramientas — el panel muestra una única
línea explicativa en vez de una caja vacía.

Una fila con `tools_by_server: []` (declaró explícitamente cero servidores)
NO califica como fuente: es un dato real, pero no el que este panel necesita
mostrar (ver la distinción `null` vs. `[]` en `docs/telemetry-per-request.md`
§4.2). El panel sigue buscando hacia atrás hasta encontrar una fila con
desglose real.

### 8.2. Columnas

| Columna | Qué muestra |
|---|---|
| `servidor` | Etiqueta de display del servidor (`(native)`, `claude_ai_Gmail`, …) |
| `kind` | `native` / `mcp` / `others` |
| `tools` | Cantidad de herramientas de ese servidor |
| `bytes` | Bytes de ese servidor, formato compacto (`format_bytes`, decimal) |
| `% tools` | `bytes / context_tools_bytes * 100`, un decimal. `-` si `context_tools_bytes` es `null` (**nunca** `0.0`) |
| `Δ baseline` | Ver §8.3 |

Las filas llegan en el MISMO orden en que el proxy entrega `tools_by_server`
(bytes descendente) — el panel no reordena.

Dos filas de resumen cierran la tabla, separadas visualmente del detalle por
servidor:

| Fila | Qué muestra |
|---|---|
| `overhead` | `tools_overhead_bytes`: brackets/comas del array, wrapper de Gemini, herramientas huérfanas — ver `docs/telemetry-per-request.md` §4.2 |
| `TOTAL` | `context_tools_bytes` — y, en la columna `Δ baseline`, el delta TOTAL agregado (la cifra que responde "¿cuánto bajé en total?") |

Se cumple siempre: `sum(bytes de cada servidor) + overhead == TOTAL`.

### 8.3. `Δ baseline` — el punto de este panel

Extiende el baseline que ya existe (tecla `b`, §3): al marcarlo, además de
los contadores de `/stats`, el monitor también saca una foto de
`tools_by_server` (servidor → bytes) de la fila fuente vigente en ese
instante (§8.1). `r` borra ambas fotos a la vez.

Flujo completo:

1. Con el cliente conectado a todos sus servidores MCP de siempre, pulse
   `b`. El panel de tools por servidor congela esa foto.
2. Reinicie el cliente **sin** el servidor MCP que se quiere dejar de pagar en
   cada request (p. ej. sacando Google Calendar de la config de MCP).
3. Genere una petición nueva. El panel se actualiza con la fila fuente más
   reciente.
4. Observe la columna `Δ baseline`: el servidor retirado aparece con
   `bytes: 0 B`, `tools: 0` y su delta NEGATIVO completo (p. ej.
   `-21.0 kB`) — sigue LISTADO, no desaparece de la tabla. Un servidor que
   simplemente deja de aparecer no dice nada por sí solo; uno que aparece con
   `0 B` y un delta negativo enorme es la confirmación visual de que la
   palanca funcionó.
5. La fila `TOTAL` da la cifra agregada (p. ej. `-55.1 kB`): cuánto bajó el
   contexto total de herramientas por request, de una sola vez.

Reglas de signo y de aparición/desaparición:

- Servidor sin cambios ⇒ delta `0 B` (no `-`: es un dato real, no un hueco).
- Servidor nuevo (no estaba en el baseline) ⇒ delta POSITIVO completo, igual
  a sus bytes actuales.
- Servidor desaparecido (estaba en el baseline, ya no está) ⇒ fila sintética
  con `bytes: 0`, `tools: 0`, delta NEGATIVO completo — listada DESPUÉS de
  los servidores presentes, ordenada entre sí por peso de baseline
  descendente.
- Sin baseline marcado ⇒ toda la columna `Δ baseline` (fila por fila y
  `TOTAL`) se muestra `-`.

La lógica de este cálculo vive en `diff_against_baseline` (`src/bin/monitor.rs`),
función PURA sin dependencias de ratatui ni de HTTP, testeada exhaustivamente
por separado (casos: sin baseline, servidor aparecido, desaparecido, sin
cambios, y el orden del resultado).

### 8.4. Cómo leer las señales

- **`(native)` domina el total** ⇒ ese es el PISO que no baja por más
  servidores MCP que se desconecten: son las herramientas propias del cliente,
  no de un MCP. Ninguna palanca de configuración de MCP toca este número.
- **Un servidor `mcp` grande que nunca aparece invocado en el tráfico real**
  (cruzarlo con el panel de requests, §7, para ver qué herramientas se llaman
  de hecho) ⇒ candidato directo a sacar de la config de MCP del proyecto:
  sus bytes se pagan en TODAS las requests, se use o no.
- **Un servidor con delta negativo grande y sostenido tras `b` + reinicio del
  cliente** ⇒ la palanca funcionó; es la confirmación que este panel existe
  para dar.

### 8.5. Modo headless (`--once`)

Igual que las vistas `Latency`/`Context` del panel de requests, `--once`
imprime esta tabla en texto plano (misma fuente, mismas columnas). La
columna `Δ baseline` sale siempre `-`: no hay sesión interactiva en la que
apretar `b`.

## 9. Panel de cuota de suscripción Codex (`u`)

Consume el mismo `GET /requests` (§7) que los otros paneles de detalle, pero
mira un campo distinto: `codex_quota` — el estado de cuota de la cuenta en el
momento de ESA petición puntual, presente únicamente cuando la petición se
enrutó al backend de Codex vía OAuth (suscripción, no API key). Responde:
*"¿cuánto me queda de cuota, y cuándo resetea?"*

Se alterna con la tecla `u`, **INDEPENDIENTE** de `p`/`c`/`s`: las cuatro
teclas controlan estados ortogonales, cualquier combinación de visibilidad es
válida.

### 9.1. Fuente de datos y por qué es un panel dedicado

La cuota es un gauge a nivel de CUENTA — idéntico para todos los modelos que
lleguen por el backend de Codex en un instante dado — no una columna más por
fila. Por eso vive en su propio panel (`Paragraph` con líneas, no una
`Table`), y no en el ciclo de columnas `c` del panel de requests (§7.1): ese
ciclo cambia qué columnas se ven POR FILA, y la cuota no tiene filas.

El panel toma la fila MÁS RECIENTE de `/requests` cuyo `codex_quota` sea
no-nulo (`find_quota_source_row`, `src/bin/monitor.rs`) y muestra
exactamente esa fila. El título indica `HH:MM:SS` y modelo de esa fila
fuente. Si ninguna fila califica — todo el tráfico reciente es Anthropic,
Gemini u OpenAI vía API key, o el proxy es anterior a esta captura — el panel
muestra una única línea explicativa, nunca una caja vacía ni un gauge al 0%.

### 9.2. Qué muestra

Todo campo ausente se renderiza como `—` o se OMITE por completo; nunca se
fabrica un `0%` ni un valor por defecto (mismo criterio "ausente ≠ cero" que
el resto del monitor).

| Línea | Contenido | Regla de ausencia |
|---|---|---|
| Plan y límite | `plan: <plan_type> · límite: <active_limit>` | Cada campo `—` si falta |
| Ventana primaria | Barra de texto (`█`/`·`, ancho fijo) + `<n>% · ventana <minutos>m` | `—` sin barra si `primary_used_percent` es `null` |
| Ventana secundaria | Igual formato que la primaria | **Se OMITE por completo** si `secondary_window_minutes` es `null`/`0` — en esta cuenta llega vacía, y mostrar `—` para algo que la cuenta ni define sería ruido |
| Countdown de reset | `resetea en 6d 8h` (dos unidades más significativas, d/h/m) | `resetea ahora` si ya pasó; `—` si no hay ninguna fuente de reset (ver §9.3) |
| Créditos | `créditos: ilimitados` o `créditos: <balance>` | **Se OMITE** salvo que `credits_has_credits == true` |

### 9.3. Countdown de reset

Prioridad de fuente: `primary_reset_at` (timestamp unix absoluto) si está
presente; si no, se reconstruye desde el `timestamp` RFC 3339 de la fila
fuente más `primary_reset_after_seconds` (el valor de esta cabecera es
relativo al INSTANTE DE CAPTURA, no a ahora). Si ninguna de las dos fuentes
está disponible, `—` sin countdown fabricado. El "ahora" se calcula con
`chrono::Utc::now()` en cada redraw (~250 ms): el countdown avanza en vivo
sin ningún tick adicional.

### 9.4. Separación estricta de `cost_estimate_usd`

El panel no deriva nada de la cuota, ni la mezcla con coste en dólares: la
cuota es un porcentaje de ventana de un plan de precio fijo, `cost_estimate_usd`
es un importe calculado para tráfico de API key. Son dos monedas
independientes en la misma fila — ver `src/telemetry/codex_quota.rs` para la
garantía estructural completa.

### 9.5. Modo headless (`--once`)

`--once` imprime esta tabla en texto plano después de la de tools por
servidor (§8.5), con el mismo pipeline puro (`find_quota_source_row` +
`quota_lines`) y las mismas reglas de ausencia — sin sesión interactiva no
hay nada que ocultar.

## 10. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/stats.rs` | `ModelStatsRow` con sumas/counts crudas (además de promedios) — sin cambios de comportamiento, solo más campos expuestos |
| `src/telemetry/recent.rs` | `RecentRequests` — buffer FIFO acotado de las últimas 200 peticiones individuales |
| `src/telemetry/codex_quota.rs` | `CodexQuota` — captura cruda de las 12 cabeceras `x-codex-*` de cuota de suscripción; `RequestRow::codex_quota`/`CodexQuotaRow` (en `src/bin/monitor.rs`) espejan este contrato para el panel de cuota (§9) |
| `src/middleware/requests.rs` | `handle_requests` — el handler HTTP de `GET /requests` |
| `src/provider/mod.rs` | `ToolServerBytes`/`ToolServerKind` — el contrato de tipos del proxy que `RequestRow`/`ToolServerRow` (en `src/bin/monitor.rs`) espejan para el desglose de `tools_by_server` |
| `src/bin/monitor.rs` | Binario TUI independiente: fetch por HTTP de `/stats` y `/requests`, estado (baseline, historial, selección, buffer de requests), detección de outliers (incluida la detección de truncamiento por tope de tokens, `TRUNC`, ver §7.4), cálculo de delta de ventana, de delta de tools por servidor y de gauge de cuota (funciones puras testeadas aparte), render con `ratatui` |
| `src/telemetry/pricing.rs` (`CacheAccounting`) | Fuente de verdad SERVIDOR-SIDE de la contabilidad de caché por proveedor (`Separate` para Anthropic, `Subset` para OpenAI/Gemini). `prompt_tokens_total` en `src/bin/monitor.rs` (§7.3.1) DUPLICA esta semántica a propósito — el binario `monitor` no puede importarla (el crate no expone `lib.rs`) — así que un cambio acá exige actualizar también `prompt_tokens_total` |
| `docs/telemetry-by-model.md` | Contrato del endpoint `GET /stats` que este monitor consume |
| `docs/telemetry-per-request.md` | Contrato del endpoint `GET /requests` que alimenta el panel de detalle, el panel de tools por servidor y el panel de cuota |

## 11. Límites conocidos

- **El fetch de `/requests` (y el de `/stats`) es bloqueante, en el mismo
  hilo que dibuja la TUI y lee el teclado.** Ambos usan
  `reqwest::blocking::Client` con timeout de 3 segundos. Si el proxy está
  caído, el error es rápido y no se nota. Pero si el proxy está vivo y
  **lento** (no caído), el poll se queda esperando hasta el timeout — y
  durante ese tiempo el monitor no redibuja ni procesa teclas. Es una
  degradación de fluidez acotada (nunca más de ~3s por ciclo de poll), no un
  cuelgue permanente ni un panic, pero vale saberlo antes de asumir que la
  UI es siempre instantánea.
- El panel de requests recientes muestra como máximo las últimas 200
  peticiones (`RECENT_CAPACITY` en `src/telemetry/recent.rs`) y se pierde al
  reiniciar el proxy — ver `docs/telemetry-per-request.md` §5.
