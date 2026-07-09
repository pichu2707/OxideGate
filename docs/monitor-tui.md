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
cargo run --bin monitor
```

O ya compilado:

```bash
cargo build --release --bin monitor
./target/release/monitor
```

### URL del endpoint

En orden de prioridad:

1. flag `--url <url>`
2. env `OXIDEGATE_STATS_URL`
3. `http://127.0.0.1:{OXIDEGATE_PORT}/stats` (`OXIDEGATE_PORT` default `8080`,
   el mismo default que usa el proxy — ver `src/config.rs`)

```bash
# proxy en el puerto default
cargo run --bin monitor

# proxy en otro puerto
OXIDEGATE_PORT=8899 cargo run --bin monitor

# URL explícita
cargo run --bin monitor -- --url http://127.0.0.1:8899/stats
```

### Modo headless: `--once`

```bash
cargo run --bin monitor -- --once
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

1. Levantá el proxy con la optimización **apagada** (p. ej. `force_cache`
   off — ver `docs/optimizer-prompt-cache.md`).
2. Generá algo de tráfico normal para ese modelo.
3. En el monitor, elegí el modelo con `↑`/`↓` y apretá **`b`** para marcar el
   baseline (contadores crudos acumulados en ese instante).
4. Prendé la optimización (p. ej. `OXIDEGATE_FORCE_CACHE=true` y reiniciá el
   proxy, o el mecanismo que corresponda).
5. Seguí generando tráfico. El panel **ANTES/DESPUÉS** muestra el delta
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

## 5. Layout de la pantalla

1. **Header**: título, URL del endpoint, estado del último fetch ("ok · N
   modelos" o "proxy no disponible en..."), y edad del baseline ("baseline
   hace 12s" o "sin baseline — apretá 'b'").
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
6. **Footer**: recordatorio de teclas.

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

| Vista | Para qué sirve | Es la que ves con... |
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
| `tax%` | `context_tax_ratio * 100`, un decimal — `(system + tools + history) / total` | **Cercano a 100% ⇒ casi todo lo que mandás ya lo habías mandado antes.** Es la "tasa" que pagás por turno solo para repetir contexto; un `tax%` alto con `cache-hit` bajo (ver vista `Latency`/tabla principal) es la peor combinación posible |
| `prep_us` | Microsegundos que el proxy pasó dentro de `Provider::prepare` (parseo + `decompose` + mutación opcional) | Overhead propio de OxideGate, NO incluye leer el body del socket ni el round-trip al proveedor — si esto crece con el tamaño del body, el parseo/decompose es el cuello de botella, no la red |
| `outlier` | Igual que en `Latency` | — |

Los bytes se muestran en formato compacto vía `format_bytes` (ver
`src/bin/monitor.rs`): **convención DECIMAL** (base 1000, no binaria
KiB/MiB) — `159123 B` se ve como `159.1 kB`, `281 B` se ve tal cual. Se
eligió decimal porque mide tamaño de un JSON re-serializado, no bloques de
memoria alineados a potencias de 2.

`tax%` se muestra como `-` (nunca `0.0`) cuando `context_tax_ratio` es
`None` — mismo criterio de "ausente ≠ cero" que el resto del panel.

### 7.4. Marcadores de outlier

Aplican por igual a AMBAS vistas (`Latency` y `Context`): la clasificación de
outliers no depende de qué columnas estás mirando.

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

Una fila puede llevar más de un marcador a la vez (p. ej. `ERR+TTFT`): no se
colapsa a uno solo porque eso escondería información real.

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

## 8. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/stats.rs` | `ModelStatsRow` con sumas/counts crudas (además de promedios) — sin cambios de comportamiento, solo más campos expuestos |
| `src/telemetry/recent.rs` | `RecentRequests` — buffer FIFO acotado de las últimas 200 peticiones individuales |
| `src/middleware/requests.rs` | `handle_requests` — el handler HTTP de `GET /requests` |
| `src/bin/monitor.rs` | Binario TUI independiente: fetch por HTTP de `/stats` y `/requests`, estado (baseline, historial, selección, buffer de requests), detección de outliers y cálculo de delta de ventana (funciones puras testeadas aparte), render con `ratatui` |
| `docs/telemetry-by-model.md` | Contrato del endpoint `GET /stats` que este monitor consume |
| `docs/telemetry-per-request.md` | Contrato del endpoint `GET /requests` que alimenta el panel de detalle |

## 9. Límites conocidos

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
