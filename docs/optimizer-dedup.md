# Optimizador · Palanca B — dedup de respuestas por `prompt_hash`

> Estado: **descartada para tráfico conversacional/de agente** (Claude Code y
> similares), con evidencia medida — ver §0. El diseño de más abajo **no se
> borra**: queda como registro del razonamiento y sigue siendo válido para
> otras formas de tráfico (ver §0, último punto). La Palanca A
> (`docs/optimizer-prompt-cache.md`) sigue en producción y no se ve afectada
> por este descarte.

---

## 0. Por qué se descarta para tráfico conversacional (medido, no predicho)

Este diseño se escribió antes de medir tráfico real de agente contra
`claude-opus-4-8`. Con datos en mano (ver `docs/context-tax.md`), las tres
razones que en su momento eran hipótesis ahora son hechos:

- **`redundancy_rate` es 0.0 por construcción, no por accidente.** El
  `prompt_hash` se calcula con `fingerprint()` sobre el `incoming.body`
  COMPLETO (`src/provider/mod.rs`), y cada proveedor lo llama con el body
  entero (ver `provider/anthropic.rs`, `provider/openai.rs`,
  `provider/gemini.rs`). En una conversación el array `messages` crece en
  cada turno — el body de un turno nunca es byte-idéntico al de otro. Dos
  requests de la misma conversación **nunca** producen el mismo
  `prompt_hash`. No es que no haya habido suerte: la clave de dedup está
  definida sobre algo que por diseño cambia todos los turnos.
- **El techo teórico es minúsculo.** El input fresco (lo único que un dedup
  exacto podría evitar volver a pagar) es el **3.0%** del costo medido de la
  sesión (`docs/context-tax.md` §2). Un dedup perfecto ahorraría, en el
  mejor caso teórico, 3 centavos por dólar — y en la práctica, cero, porque
  ningún hash se repite.
- **El corte v1 exigía `stream = false`, y Claude Code siempre streamea.**
  El 100% del tráfico de agente medido usa `stream: true`. Aunque el
  `redundancy_rate` no fuera cero, el v1 tal como está diseñado abajo
  quedaría inelegible para el 100% de ese tráfico de todas formas.

**Dónde SÍ podría pagar esta palanca:** tráfico que repite requests
idénticos, no-streaming — reintentos, corridas de CI, clasificación o
scoring en batch, y fan-out de subagentes que comparten un prompt idéntico.
En esos escenarios el body no crece turno a turno (no hay "turno"), así que
la premisa que mata el caso conversacional no aplica. Si esa forma de
tráfico aparece, este diseño sigue siendo el punto de partida correcto.

---

## 1. Qué es y cuál es el techo

Si entra una petición **idéntica** a una que ya respondimos, servir la respuesta
guardada sin tocar al proveedor: **0 tokens, ~0 latencia**. Es el ahorro máximo
posible — no abaratamos la llamada, la eliminamos.

Ya tenemos la señal para saber si vale la pena: `redundancy_rate` por modelo en
`GET /stats` (y en el monitor). Si en tu tráfico ronda cero, esta palanca no
sirve; si es alto, es el mayor win del proyecto. **Medí primero.**

---

## 2. Por qué es delicado (y por qué A era seguro y ésta no)

La Palanca A mantenía la semántica: el proveedor **seguía generando respuesta
fresca**, solo abaratábamos el prefijo. La Palanca B **deja de reenviar** y
devuelve una respuesta vieja. Eso mete landmines reales:

| Riesgo | Por qué importa |
|---|---|
| **No-determinismo** | Con `temperature > 0`, dos peticiones idénticas quieren respuestas *distintas*. Servir la vieja es incorrecto: el cliente pidió variedad. |
| **Staleness** | Una respuesta cacheada puede estar vencida (datos con fecha, estado que cambió). |
| **Corrección de agentes** | En un loop de agente, servir una respuesta vieja puede romper el loop en silencio (el agente cree que reintentó y obtuvo lo mismo). |
| **Streaming** | Reproducir una respuesta guardada como stream SSE es trabajo extra y frágil. |
| **Bufferizar** | Guardar una respuesta exige leerla entera — choca con el principio "nunca bufferizar" del proxy (crítico para SSE). |

La conclusión de estos riesgos define el corte: el primer slice es **chico,
opt-in y conservador**. Preferimos dedupear poco y bien a dedupear mucho y romper
un agente sin que se note.

---

## 3. El corte SEGURO del primer slice

### Elegibilidad (todas las condiciones, o no se dedupea)
1. **Flag encendido.** `OXIDEGATE_DEDUP` (env), **apagado por defecto** — igual
   que la Palanca A. OxideGate sigue siendo medidor transparente salvo opt-in.
2. **No-streaming.** Solo peticiones con `stream = false`. Una respuesta
   no-streaming es un JSON único, chico: bufferizarla es barato y NO rompe el
   camino SSE. El streaming queda FUERA del v1 (ver §5).
3. **Determinismo explícito.** Solo si el body trae `temperature == 0`. Con
   `temperature` ausente el default del proveedor no es cero (varía), así que no
   lo asumimos: sin `temperature: 0` explícito, no se dedupea. Es el cliente
   quien señala "esto es determinista, la misma entrada debe dar la misma
   salida".

### Clave de caché
`(upstream, model, route, prompt_hash)`. El `prompt_hash` ya se calcula sobre el
body ORIGINAL completo (ver `provider::fingerprint`), así que captura también
`temperature`, `tools`, y todo lo demás: dos peticiones con la misma clave son
byte-idénticas en su cuerpo. Añadimos `upstream/model/route` para que jamás
colisione una respuesta entre modelos o rutas distintas.

### TTL y tamaño
- **TTL corto** (default 60s, configurable): acota la staleness. Una entrada
  vencida se ignora y se re-consulta al proveedor.
- **LRU acotado** (nº de entradas con cap): la caché no crece sin límite en un
  proceso de larga vida (misma disciplina que el cap de huellas de `/stats`).

### Qué se guarda y cómo se sirve
- **Guardar:** en una petición elegible que fue al proveedor, bufferizamos la
  respuesta (status + cuerpo, sin cabeceras hop-by-hop) y la guardamos bajo su
  clave con su `stored_at`.
- **Servir:** si entra una petición elegible con la clave presente y fresca,
  devolvemos el cuerpo guardado sin tocar al proveedor, con una cabecera
  marcadora `x-oxidegate-cache: hit` para que sea observable.

---

## 4. Integración (dónde y cómo)

- **Punto de intercepción:** `middleware/proxy.rs::run`. Ahí ya tenemos
  `prompt_hash`, `upstream`, `model`, `route`, `stream` y el body, ANTES de
  `send_and_meter`. Si la petición es elegible y hay hit fresco → devolver la
  respuesta cacheada y registrar la métrica; si no → seguir el camino normal y,
  al salir, guardar la respuesta si era elegible.
- **Dónde vive la caché:** `src/optimizer/cache.rs` (hoy placeholder) cobra
  vida: `ResponseCache` detrás de un `Arc<RwLock<_>>` (o mapa concurrente) en
  `AppState`. La lógica de elegibilidad es una **función pura y testeable**
  (recibe stream/temperature/flag → bool), separada del transporte.
- **Responsabilidad única:** la elegibilidad y el store/serve viven en el
  optimizador; `proxy.rs` solo pregunta "¿hay hit?" y "guardá esto". El
  transporte no aprende reglas de caché.
- **Telemetría — el antes/después:** sumar `served_from_cache: bool` a
  `RequestMetric` (y a la agregación de `/stats`). Un hit servido se registra
  con `total_ms ~0` y `cost 0`: el monitor lo muestra al instante en el panel
  Δ-baseline. Ese es el circuito completo: `redundancy_rate` te dice dónde hay
  oportunidad → prendés `OXIDEGATE_DEDUP` → el monitor te muestra el ahorro.

---

## 5. Fuera del primer slice (explícito, para no sobre-diseñar)

- **Streaming.** Reproducir SSE guardado (replay) es un slice propio, después.
- **`temperature > 0`.** El cliente pidió variedad; no se dedupea nunca.
- **Herramientas con efectos secundarios.** Una respuesta con `tool_use` puede
  disparar acciones aguas abajo; queda fuera hasta analizarlo aparte.
- **Dedup semántico / near-duplicate.** Solo match EXACTO por `prompt_hash` en
  el v1. Nada de similitud aproximada.
- **Invalidación explícita.** Solo TTL en el v1; sin API de purga.

---

## 6. Checklist para implementar mañana

1. `config.rs`: flag `OXIDEGATE_DEDUP` (default off) + TTL configurable.
2. `optimizer/cache.rs`: `ResponseCache` (clave → `{status, body, stored_at}`),
   con TTL + LRU acotado; función pura `is_eligible(stream, temperature, flag)`.
   Tests de la elegibilidad y del vencimiento por TTL.
3. `state.rs`: colgar la `ResponseCache` compartida en `AppState`.
4. `proxy.rs::run`: leer `temperature` del body; si elegible + hit fresco →
   servir cacheado (métrica `served_from_cache=true`, `x-oxidegate-cache: hit`);
   si no → reenviar y, si elegible, guardar la respuesta bufferizada.
5. Telemetría: `served_from_cache: bool` en `RequestMetric` + `stats.rs` +
   `monitor` (columna/Δ de hits servidos).
6. `docs/`: actualizar este doc a "implementado" y sumar la fila a la tabla del
   README.

> Recordatorio de secuencia: antes de implementar, **mirar `redundancy_rate` en
> tráfico real**. Si no hay redundancia, no hay nada que dedupear.
