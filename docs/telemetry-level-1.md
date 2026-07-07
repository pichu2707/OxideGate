# Nivel 1 — Telemetría de OxideGate

> Estado: implementado y verificado (Anthropic + OpenAI). Gemini pendiente.
> Cubre qué medimos en el primer paso, para qué sirve y qué es cada dato.

---

## 1. Qué es el Nivel 1 y para qué sirve

El Nivel 1 es la **capa de medición**: el cimiento sobre el que se construye
todo lo demás (caché, enrutado por coste, detección de peticiones absurdas).

El principio es simple y no negociable:

> **No se puede optimizar lo que no se mide.**

Antes de decidir "este prompt se puede cachear" o "esta carga conviene mandarla
a un modelo más barato", necesitamos datos reales: cuántos tokens cuesta cada
petición, cuánto tarda, y si se repite. El Nivel 1 produce exactamente eso: una
fila de telemetría por cada petición que atraviesa el proxy.

### Cómo llega el dato: proxy explícito, no un MITM

OxideGate se sienta **en medio** de gentle-ai y el proveedor (Claude, OpenAI…),
pero **no** es un "man in the middle" en el sentido de seguridad:

- gentle-ai **sabe** que habla con OxideGate — le apuntamos su base URL a
  `http://localhost:8080/v1` a propósito.
- No se rompe ningún TLS: la conexión de gentle-ai termina en OxideGate de forma
  legítima, y OxideGate abre su propia conexión TLS hacia el proveedor.

Es el patrón de un **proxy explícito / API gateway** (como LiteLLM o Helicone):
un intermediario de confianza al que el cliente elige pasar. El precio de leer
el tráfico en claro es una responsabilidad de seguridad: la API key del
proveedor pasa por nosotros en las cabeceras — **nunca se loguea ni se persiste**.

```
  gentle-ai ──HTTP──▶ OxideGate ──HTTPS──▶ api.anthropic.com / api.openai.com
                          │
                          └──▶ telemetry.jsonl  (una fila por petición)
```

### Precondición: si no pasa por nosotros, no hay dato

El wrapper de medición puede ser perfecto, pero **si gentle-ai no enruta su
tráfico a través de OxideGate, la telemetría queda vacía**. Hay que apuntar la
base URL del proveedor (p. ej. `ANTHROPIC_BASE_URL`) a `http://localhost:8080/v1`.
Ese redireccionamiento es el interruptor que enciende todo el Nivel 1.

---

## 2. El nudo técnico: medir sin estorbar

El camino crítico de una petición es sagrado: cualquier milisegundo que le
sumemos se lo sumamos a la latencia que gentle-ai le devuelve al usuario. Por eso
el diseño respeta dos reglas:

1. **La escritura a disco va fuera del camino crítico.** El handler solo hace un
   `send` a un canal en memoria (no bloquea); una task en background serializa a
   JSONL y escribe. El I/O de log nunca se suma a la latencia.
   → `src/telemetry/logger.rs`

2. **La respuesta se mide al vuelo, sin bufferizar.** Como las respuestas son
   *streaming* (SSE), no podemos esperar a tenerla entera. Envolvemos el stream
   en un observador (`MeteredBody`) que reenvía cada chunk **intacto** hacia el
   cliente mientras, de paso, marca tiempos y escanea los tokens. La métrica se
   emite cuando el stream se cierra (o si el cliente se desconecta antes).
   → `src/telemetry/metered.rs`

---

## 3. Qué mide cada campo (`RequestMetric`)

La métrica agrupa **tres ejes** que en agentes están correlacionados: un prompt
grande sube el coste **y** empeora la latencia. Medirlos juntos es lo que deja
ver esa correlación.

Los campos son opcionales (`null` en el JSON) cuando el dato puede faltar de
forma legítima — preferimos un hueco honesto a un cero falso.

### Eje 1 — Identidad (para detectar redundancias)

| Campo | Qué es | Para qué |
|---|---|---|
| `timestamp` | Instante de emisión (RFC 3339, UTC) | Ordenar y agrupar en el tiempo |
| `route` | Ruta local del proxy (`/v1/messages`…) | Saber qué API se usó |
| `upstream` | Proveedor destino (`anthropic`, `openai`) | Segmentar por proveedor |
| `model` | Modelo pedido, leído del body | Coste y comparación por modelo |
| `prompt_hash` | Huella (hash de 64 bits) del body | **Misma huella ⇒ mismo prompt.** Base para detectar peticiones duplicadas o redundantes |
| `stream` | ¿El cliente pidió streaming? | Interpretar bien latencia y `usage` |

> `prompt_hash` es un hash **no criptográfico** (rápido, barato). No busca
> resistencia a colisiones, solo "mismo prompt ⇒ misma huella" para deduplicar.

### Eje 2 — Coste

| Campo | Qué es | Para qué |
|---|---|---|
| `prompt_bytes` | Tamaño en bytes del body | Sombra barata del tamaño (ver aviso abajo) |
| `input_tokens` | Tokens de entrada **exactos**, del `usage` del proveedor | Coste real de entrada |
| `output_tokens` | Tokens de salida **exactos**, del `usage` del proveedor | Coste real de salida |
| `cost_estimate_usd` | Coste estimado en USD según tabla de precios | **El único dato comparable entre proveedores** |

> **Aviso: `prompt_bytes` NO son tokens.** Los bytes son una sombra aproximada.
> Lo que factura el proveedor son tokens, y por eso los medimos exactos desde el
> `usage`. `prompt_bytes` se conserva solo como referencia barata.

### Eje 3 — Latencia

| Campo | Qué es | Para qué |
|---|---|---|
| `status` | Código HTTP devuelto al cliente | Distinguir éxitos de fallos |
| `ttft_ms` | **Time To First Token**: ms hasta el primer chunk | La latencia que de verdad siente el usuario en streaming |
| `total_ms` | ms desde la petición hasta cerrar el stream | Latencia total |
| `tokens_per_sec` | Tokens de salida / tramo de generación (`total − ttft`) | Velocidad de generación del modelo |

---

## 4. La trampa del token (crítico para comparar proveedores)

> **1 token de Anthropic ≠ 1 token de Gemini ≠ 1 token de OpenAI.**

Cada proveedor usa **su propio tokenizador**, con vocabularios distintos:

| Proveedor | Tokenizador |
|---|---|
| OpenAI | `tiktoken` (BPE, `o200k_base`) |
| Anthropic | Propietario |
| Gemini | SentencePiece |

El **mismo texto** produce **conteos de tokens diferentes** en cada uno. La
consecuencia de diseño es directa:

- `input_tokens` / `output_tokens` son unidades **locales de cada proveedor**.
  **No se suman ni promedian entre proveedores distintos.**
- `cost_estimate_usd` (USD) es el **único denominador común** para comparar
  proveedores, porque el precio de cada uno ya está calibrado sobre su propio
  token. Para decidir "¿me conviene Claude o Gemini para esta carga?", se compara
  **coste**, nunca tokens crudos.
- La latencia (`ttft_ms`, `total_ms`) también es comparable entre proveedores.

---

## 5. Estado por proveedor

| Proveedor | Ruta | Framing | Campos `usage` | Estado |
|---|---|---|---|---|
| Anthropic | `/v1/messages` | SSE `data:` | `message_start.usage.input_tokens`, `message_delta.usage.output_tokens` | ✅ implementado |
| OpenAI | `/v1/chat/completions` | SSE `data:` | `usage.prompt_tokens`, `usage.completion_tokens` | ✅ implementado |
| Gemini | *(sin ruta)* | array JSON / `:streamGenerateContent` | `usageMetadata.promptTokenCount`, `candidatesTokenCount` | ⛔ pendiente |

### Detalles por proveedor

- **Anthropic** manda el `usage` en el stream **por defecto**: `input_tokens` en
  el evento `message_start`, y `output_tokens` (acumulado) en el `message_delta`
  final.
- **OpenAI** **no** manda `usage` en streaming salvo que el request traiga
  `stream_options.include_usage = true`. Como somos un proxy transparente y el
  cliente no lo pone, **OxideGate lo inyecta** en la petición saliente a OpenAI.
  Es la única mutación que hacemos al request; la respuesta se reenvía intacta.
- **Gemini** es **otra API**: endpoint distinto, framing de stream distinto
  (puede ser array JSON, no `data:` puro) y nombres de campo distintos. Requiere
  su propia ruta y su propio parser. Es el punto de extensión natural.

> **Hacia dónde tiende esto:** un **adaptador por proveedor** que declare su
> endpoint, su framing de stream y su mapeo de campos `usage`. Hoy hay 2
> proveedores incrustados; el adaptador es el refactor natural cuando entre el 3º.

---

## 6. Simplificaciones y deuda conocida (fase 1)

- **No filtramos el chunk de `usage` de OpenAI** del stream de vuelta al cliente.
  Strippearlo cruzando fronteras de chunk es frágil y aporta poco (los clientes
  ignoran el chunk con `choices: []`). Solo mutamos el request, la respuesta va
  intacta.
- **Precios editables, no oficiales.** `src/telemetry/pricing.rs` trae valores
  por defecto aproximados. Hay que mantenerlos sincronizados con la tarifa real.
  Modelo desconocido ⇒ `cost_estimate_usd = null` (nunca un número inventado).
- **Tokens de caché de Anthropic sin contar.** Claude reporta
  `cache_creation_input_tokens` y `cache_read_input_tokens` (los cacheados se
  facturan más barato). Aún no los leemos, así que en peticiones con *prompt
  caching* el coste sale **inflado**. Deuda, no bug.
- **API key en claro.** Pasa por las cabeceras. En `localhost` el riesgo es bajo,
  pero al escalar fuera de la máquina local hay que blindarlo. Nunca loguear ni
  persistir la key.

---

## 7. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/logger.rs` | `RequestMetric` (los datos) + `TelemetrySink` (escritura async fuera del camino crítico) |
| `src/telemetry/metered.rs` | `MeteredBody` — envuelve el stream, mide TTFT, escanea `usage`, emite la métrica |
| `src/telemetry/pricing.rs` | Tabla de precios por modelo y cálculo de `cost_estimate_usd` |
| `src/middleware/proxy.rs` | Parsea el request, inyecta `include_usage`, envuelve la respuesta |

La salida se escribe en `~/.config/oxidegate/telemetry.jsonl`, una fila JSON por
petición.
