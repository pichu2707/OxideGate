# Velocidad — la otra moneda, y por qué no es la misma palanca que tokens

> Todo lo demás que este proyecto documentó hasta ahora optimiza **tokens**
> (coste, ventana de contexto, rate limits). Esta página es sobre **TIEMPO**.
> Son dos ejes distintos, no dos nombres para lo mismo, y optimizar uno **no**
> optimiza el otro — a veces se oponen: reducir el prefijo ahorra tokens y no
> acelera nada medible (ver §2). Quien busque "cómo hago esto más rápido"
> tiene que leer esta página, no `docs/context-tax.md`.

---

## 1. Dónde está el tiempo

De `docs/context-tax.md` §3, sobre la sesión medida el 2026-07-09:

- Generación (streaming): **82%** del tiempo ocupado.
- TTFT (time-to-first-token): **18%**.

Medición nueva de esta página, sobre las peticiones reales a `claude-opus-4-8`
en `~/.config/oxidegate/telemetry.jsonl` (metodología y población exactas en
§2):

| métrica | mínimo | mediana | máximo |
|---|---|---|---|
| TTFT | 1.237 ms | 1.884 ms | 9.846 ms |
| Throughput de generación (tok/s) | 3,9 | 69,3 | 330,3 |

---

## 2. El TTFT no es una palanca — MEDIDO

Población: las peticiones a `claude-opus-4-8` con `status = 200` en
`telemetry.jsonl`, **excluyendo** los dos sondeos de comparación de
`--effort` de §3 (que se miden aparte, con n=2, y no dicen nada como
muestra) y la única petición rechazada por rate limit (`status = 429`, ver
`docs/context-tax.md` §3) — esa fila no generó ni un token y su "TTFT" de
347 ms mide cuánto tardó el proveedor en devolver el rechazo, no una
respuesta real. Quedan **87 peticiones**.

Correlación de Pearson del TTFT contra cada variable capturada, sobre esas
87 peticiones:

| variable | r |
|---|---|
| prefijo total (`input + cache_read + cache_write`) | +0,10 |
| `cache_write` (escritura en frío) | −0,01 |
| `cache_read` (relectura) | +0,08 |
| `input` fresco | −0,06 |
| `output_tokens` (control: no debería influir) | +0,08 |

Ninguna explica nada — todas quedan por debajo de |r| = 0,11, el mismo orden
que el `output_tokens` de control, que por construcción no debería
correlacionar con nada. El TTFT lo determinan la cola y la carga del
proveedor, que el proxy no ve y no puede tocar.

**Consecuencia que corrige una intuición razonable:** reducir el prefijo
ahorra tokens, ventana de contexto y rate limits, pero NO acelera de forma
medible. El prefill de tokens ya cacheados es barato. Las dos monedas no se
mueven juntas.

> **Corrección.** El mensaje del commit `13023dd` cita un primer cálculo de
> estas correlaciones —prefijo +0,190, `cache_write` +0,010— hecho sobre una
> población que incluía la petición rechazada con `429`. Sobre la población
> limpia de arriba, el prefijo baja a +0,113 y `cache_write` cambia de signo a
> −0,037. La conclusión no se mueve: ninguna variable explica el TTFT. Las
> cifras válidas son las de la tabla, no las del commit.

---

## 3. Las dos palancas de velocidad, y dónde se accionan

**`--effort <nivel>`** (`low`, `medium`, `high`, `xhigh`, `max`). Los tokens
de pensamiento son tokens de salida: se generan, se pagan, y sobre todo se
esperan. Menos `effort` ⇒ menos pensamiento ⇒ menos tiempo de generación.

- Flag del CLI: `claude --effort low -p "..."`, o `--effort` para la sesión.
- MEDIDO: Claude Code envía `output_config: {"effort": "high"}` por defecto.
  Lo confirman dos métodos independientes: el body capturado con un sumidero
  HTTP local lleva esa clave, y la sonda sin ningún flag registra
  `requested_effort = "high"` en `GET /requests`. Con `--effort low`, la misma
  sonda registra `"low"`.

  > **Trampa de lectura del JSONL.** De las 90 filas de `claude-opus-4-8` en
  > `telemetry.jsonl`, solo **2** llevan la clave `requested_effort`: las 88
  > restantes son de builds anteriores al commit `13023dd`, donde el campo
  > todavía no existía. La **ausencia de la clave** significa "el proxy no lo
  > capturaba", no "el cliente no lo envió". Confundir las dos cosas lleva a
  > concluir, falsamente, que Claude Code no manda `effort`. Al filtrar el
  > JSONL, conviene comprobar `'requested_effort' in row` antes de leer su
  > valor.
- Observación honesta, con la muestra que hay: esas dos sondas de la misma
  frase dieron `gen_ms` (`total_ms − ttft_ms`) de 74 ms con `high` (5 tokens
  de salida) y 24 ms con `low` (4 tokens de salida). La dirección es la
  esperada, pero con salidas de cuatro/cinco tokens esto NO es una medición
  del efecto, solo una comprobación de que la captura funciona. Se dice así,
  sin adornos.

**Fast mode (`speed: "fast"`)**. Documentado por Anthropic: hasta ~2,5×
más tokens por segundo de salida, a precio premium, sobre Opus 4.8 y 4.7.
Tiene su propio rate limit, separado del estándar.

- En Claude Code se activa con el comando interactivo `/fast`.
- Es la única palanca que ataca directamente el 82% de §1.
- No está disponible en Amazon Bedrock, Vertex AI ni Microsoft Foundry
  (documentado por Anthropic; OxideGate no enruta tráfico hacia esos tres
  hoy, así que no hay forma de confirmarlo desde este repo).
- ESTADO EN ESTE PROYECTO: **no observado todavía**. Ni `requested_speed`
  (la clave `speed` de la raíz del body) ni `served_speed`
  (`usage.speed` de la respuesta) aparecen en ninguna de las 90 peticiones
  capturadas: el tráfico de este proyecto corre entero en velocidad
  estándar.

---

## 4. Qué captura OxideGate (implementado, commit `13023dd`)

Tres campos nuevos, expuestos en `GET /requests` y en la vista `Latency`
del monitor (columnas `effort`, `spd_req`, `spd_got`):

| campo | de dónde sale | significado |
|---|---|---|
| `requested_effort` | `output_config.effort` del body | El nivel de esfuerzo pedido |
| `requested_speed` | `speed` de la raíz del body | `"fast"` si el cliente pidió fast mode |
| `served_speed` | `usage.speed` de la respuesta | La velocidad con la que el proveedor sirvió de hecho |

Por qué `requested_speed` y `served_speed` son campos SEPARADOS: el fast
mode tiene su propio rate limit, así que una petición puede pedir `fast` y
ser servida en `standard`. Un solo campo escondería exactamente el fallo
que este par existe para delatar.

`served_speed` está DOCUMENTADO por Anthropic pero NO OBSERVADO todavía en
el tráfico de este proyecto (§3). Un `None` significa "no reportado", nunca
"estándar" — mismo criterio de "ausente ≠ cero" que el resto de la
telemetría (ver `docs/monitor-tui.md` §7.2).

Son dialecto de Anthropic: OpenAI y Gemini devuelven `None` a propósito,
con la razón escrita en el código (`src/provider/openai.rs`,
`src/provider/gemini.rs`).

---

## 5. Cómo medir el antes/después

1. Levante el proxy: `OXIDEGATE_PORT=8899 cargo run --bin oxidegate`
2. Levante el monitor en otra terminal: `OXIDEGATE_PORT=8899 cargo run --bin monitor`
3. Apunte el cliente al proxy: `ANTHROPIC_BASE_URL=http://localhost:8899 claude`
4. Genere tráfico normal y pulse `b` en el monitor para marcar el baseline.
5. Active la palanca: `/fast` dentro de la sesión de Claude Code, o
   reinicie con `--effort low`.
6. Genere tráfico equivalente y observe el panel `Δ desde baseline`: la
   columna `tok/s` es la que responde. `p` abre el panel por petición; las
   columnas `effort`, `spd_req` y `spd_got` confirman que la palanca llegó
   al cable.

Regla metodológica, tomada del resto del proyecto (ver §2): comparar
únicamente peticiones con tareas equivalentes. El `tok/s` de un turno de
cuatro tokens no dice nada; hace falta salida larga para que el throughput
signifique algo.

---

## 6. Lo que NO acelera (con puntero a la evidencia)

| descartado | cifra | evidencia |
|---|---|---|
| Reducir el prefijo (tokens) | ahorra coste, no tiempo — r=+0,10 (§2) | esta página, §2 |
| Compresión de bytes (gzip) | el modelo tokeniza el texto descomprimido; solo ahorra ~7 ms de subida en fibra sobre ~280 KB | `docs/findings.md` §E |
| Optimizar el transporte MCP | 0,68 ms de mediana (salto JSON-RPC por stdio) contra un turno real de 11.123 ms | `docs/findings.md` §E |
| El overhead del propio proxy | `prepare_us` va de 43 µs a 15.135 µs — el 0,67% de una petición típica | `docs/findings.md` §E |
| Hilos paralelos | compran reloj de pared, lo pagan en tokens de prefijo por hilo | `docs/context-tax.md` §3, `docs/findings.md` §E |

---

## Ver también

- `docs/context-tax.md` — descomposición medida de coste y latencia de una sesión real (§3, la base de esta página)
- `docs/findings.md` — qué se probó, qué se descartó y qué se retractó, por conclusión
- `docs/monitor-tui.md` §7.2 — las columnas `effort`, `spd_req`, `spd_got` en el panel de requests recientes
