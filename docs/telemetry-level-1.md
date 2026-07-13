# Nivel 1 — Telemetría de OxideGate

> Estado: los 3 proveedores (Anthropic, Gemini, OpenAI) validados en vivo con
> tokens/coste reales. Cubre qué medimos en el primer paso, para qué sirve y qué
> es cada dato.

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
| `tokens_per_sec` | Tokens de salida / tramo de generación (`total − ttft`) | Velocidad de generación del modelo. **Solo en streaming**: en no-streaming todo llega de golpe (`ttft ≈ total`) y el número se dispara, así que se anula a `null` |

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
| Anthropic | `/v1/messages` | SSE `data:` | `message_start.usage.input_tokens`, `message_delta.usage.output_tokens` | ✅ validado en vivo |
| OpenAI (Responses) | `/v1/responses` | SSE `data:` | `response.usage.input_tokens`, `output_tokens` | ✅ validado en vivo |
| OpenAI (Chat) | `/v1/chat/completions` | SSE `data:` | `usage.prompt_tokens`, `usage.completion_tokens` | 🟡 validado contra servidor OpenAI-compatible (§5.1); `api.openai.com` pendiente |
| Gemini | `/v1beta/*` (comodín) | SSE `data:` (`?alt=sse`) | `usageMetadata.promptTokenCount`, `candidatesTokenCount` | ✅ validado en vivo |

### Detalles por proveedor

- **Anthropic** manda el `usage` en el stream **por defecto**: `input_tokens` en
  el evento `message_start`, y `output_tokens` (acumulado) en el `message_delta`
  final. Modelo y `stream` van en el **body** JSON.
- **OpenAI** tiene dos superficies:
  - **Responses API** (`/v1/responses`, la que usan los clientes modernos):
    reporta `usage` **por defecto**, anidado bajo `response` en el evento
    `response.completed`. Modelo y `stream` van en el body; no se inyecta nada.
    **Validado en vivo** con API key real (`api.openai.com`).
    **Corrección (captura real de hoy):** una captura de tráfico en vivo
    (gpt-5.5 vía el backend de ChatGPT/Codex, `chatgpt.com/backend-api/codex`)
    mostró que el extractor compartido (`extract_openai_usage`,
    `src/provider/openai.rs`) leía únicamente los nombres de campo de Chat
    Completions (`prompt_tokens`/`completion_tokens`), no los de Responses
    (`input_tokens`/`output_tokens`): el request devolvía `200 OK` pero
    `input_tokens`/`output_tokens` quedaban en `null` en `/requests`. La caché
    (`input_tokens_details.cached_tokens`) sí se leía bien. Ya está corregido:
    el extractor ahora reconoce ambos nombres (`.or_else`, mismo patrón que ya
    usaba la extracción de caché). Sigue pendiente el desglose de
    `output_tokens_details.reasoning_tokens` (§6).
  - **Chat Completions** (`/v1/chat/completions`): **no** manda `usage` en
    streaming salvo que el request traiga `stream_options.include_usage = true`;
    como el cliente no lo pone, **OxideGate lo inyecta** (única mutación).
    **Validado en vivo con grupo de control** contra un servidor
    OpenAI-compatible (Ollama), cliente real (OpenCode) — ver §5.1. **Falta**
    repetirlo contra `api.openai.com` con API key: el mecanismo está probado,
    el proveedor concreto no.
- **Codex / suscripción de ChatGPT — SÍ se puede medir (medido en vivo, §5.3).**
  Codex autenticado con cuenta ChatGPT (`auth_mode: "chatgpt"`, sin API key) pega
  al **backend de Codex** (`chatgpt.com/backend-api/codex`), NO a
  `api.openai.com`. La variable de entorno `OPENAI_BASE_URL` **no** lo redirige
  (se comprobó: Codex y OpenCode con login de ChatGPT la ignoran). Pero el token
  OAuth SÍ es válido contra ese backend, y OxideGate puede ponerse en medio como
  proxy explícito apuntando su `OPENAI_API_BASE` a `chatgpt.com/backend-api/codex`
  y haciendo que el cliente enrute por él. Ver §5.3 para la medición completa de
  `gpt-5.5` con tokens reales. **Lo que sigue sin poder medirse** es
  `api.openai.com` con este token: el OAuth de plan ChatGPT tiene scopes
  limitados (`GET /models` responde `403 insufficient permissions`), así que la
  API pública sigue necesitando su propia API key.
- **Gemini** rompe varios supuestos y por eso necesitó ruta y parser propios:
  - **El modelo va en la URL**, no en el body:
    `/v1beta/models/{model}:{método}`. `streamGenerateContent` ⇒ streaming;
    `generateContent` ⇒ no-streaming. Se extrae con `parse_gemini_path`.
  - **Ruta comodín** `/v1beta/*` que **preserva path + query** al reenviar (la
    query lleva `alt=sse` y a veces la API key). El destino es solo el host
    (`generativelanguage.googleapis.com`), configurable con `GEMINI_API_BASE`.
  - **Framing SSE**: el CLI de Gemini pide `?alt=sse`, así que la respuesta son
    líneas `data:` como los otros dos — el mismo escáner sirve.
  - **Campos**: `usageMetadata.promptTokenCount` (input) y `candidatesTokenCount`
    (output).
  - **Auth**: por header `x-goog-api-key` o query `?key=` (se preservan ambos).

> **Validación contra ground-truth:** los tokens medidos para Gemini se cruzaron
> contra el resumen "Model Usage" que el propio CLI imprime al cerrar sesión, y
> coincidieron **exactamente** (input y output, en streaming y no-streaming,
> sobre 3 requests y 2 modelos). La extracción está confirmada contra la fuente.

> **Hacia dónde tiende esto:** un **adaptador por proveedor** que declare su
> endpoint, su framing de stream y su mapeo de campos `usage`. Hoy los 3
> proveedores están incrustados en `proxy.rs` (Gemini entró como *bolt-on*); el
> adaptador es el refactor limpio ya acordado como siguiente paso.

### 5.1. Chat Completions: la validación con grupo de control

El problema de validar esta ruta es que la afirmación a probar —"OxideGate
inyecta `stream_options.include_usage` y por eso puede leer los tokens"— no se
puede comprobar leyendo el `usage` que reporta el propio OxideGate. Eso es un
círculo, no una prueba.

La salida es que **un servidor OpenAI-compatible no manda `usage` en streaming
si nadie se lo pide**. Eso convierte la inyección en algo observable desde
fuera:

| | Petición | Resultado |
|---|---|---|
| **Control 1** | Directo al servidor, `stream: true`, SIN `stream_options` | **0 chunks de `usage`** |
| **Control 2** | Directo al servidor, CON `include_usage` explícito | Llega `usage` en el chunk final |
| **Test** | Por OxideGate, body SIN `stream_options` | **El cliente VE llegar `usage`** |

Si el cliente nunca pidió `usage` y aun así le llega, solo hay una explicación:
el proxy lo inyectó en el cable. El control 1 descarta que el servidor lo mande
por su cuenta.

Queda el segundo eslabón —que el extractor lea bien lo que llegó—, y se cierra
contrastando contra un lector independiente: los tokens que **el cliente** vio
en el chunk SSE frente a los que **OxideGate** escribió en `GET /requests`.
Medido: `29 / 9` en ambos, exacto.

Con cliente real (OpenCode `1.17.18` sobre bun, vía `@ai-sdk/openai-compatible`)
la ruta responde `200 OK` —la inyección no rompe al cliente— y el `decompose` y
el `tools_by_server` salen correctos: 37 herramientas nativas, 48.080 B de
esquemas, `tax%` 99,9.

> **Lo que esto NO prueba.** El servidor de la prueba fue un **Ollama local**
> (`OPENAI_API_BASE=http://localhost:11434/v1`), no `api.openai.com`. Queda
> probado el **mecanismo** —inyección, framing SSE, extractor, `decompose`— y
> queda pendiente que *la API pública de OpenAI* honre la inyección igual. Para
> eso hace falta una API key de `platform.openai.com`: la suscripción de ChatGPT
> no sirve, porque su OAuth ignora `OPENAI_BASE_URL` (ver el punto de Codex más
> arriba).

### 5.2. Efecto colateral: los modelos locales truncan en silencio

La misma prueba destapó algo que no se buscaba. Ollama `llama3.2:3b` corre con
`num_ctx` 4096 por defecto, y el body de un agente real lo desborda de largo.
Dos peticiones de OpenCode con bodies **distintos** —77.579 B y 84.161 B—
reportaron **exactamente 4.095 tokens de prompt las dos**, con `200 OK`. El
modelo nunca vio la mayor parte de los 48 kB de esquemas de herramientas, y
nadie avisó.

Es el escenario para el que existe el marcador `TRUNC` del monitor, y la medida
sirvió para descubrir que su umbral estaba mal calibrado: ver
[`docs/monitor-tui.md`](monitor-tui.md) §7.4.

### 5.3. Medir la suscripción de ChatGPT (OAuth) — sin API key

El supuesto de partida era que el tráfico con login de cuenta ChatGPT no se
podía medir. Es falso. Sí se puede, y no hace falta API key ni MITM: basta con
usar OxideGate como el proxy explícito que ya es.

**Qué NO funciona (comprobado):** la variable `OPENAI_BASE_URL`. Ni Codex ni el
provider `openai` de OpenCode la respetan cuando el auth es de cuenta ChatGPT —
tienen el endpoint del backend clavado. Apuntar la variable al proxy hace que la
petición conteste bien pero **sin pasar por OxideGate**: no queda fila.

**Qué SÍ funciona.** El backend de Codex (`chatgpt.com/backend-api/codex`) acepta
el token OAuth de la cuenta. Y Codex permite declarar un *provider* propio que
reutiliza ese login (`requires_openai_auth`) contra un `base_url` cualquiera. Se
apunta ese `base_url` a OxideGate, y OxideGate reenvía al backend de Codex:

```sh
# 1. Proxy con el backend de Codex como upstream de la superficie OpenAI
OXIDEGATE_PORT=8899 \
  OPENAI_API_BASE=https://chatgpt.com/backend-api/codex \
  oxidegate

# 2. Codex enrutado por el proxy, reutilizando su propio login de ChatGPT
codex exec \
  -c 'model_provider="oxi"' \
  -c 'model_providers.oxi.name="oxi"' \
  -c 'model_providers.oxi.base_url="http://127.0.0.1:8899/v1"' \
  -c 'model_providers.oxi.wire_api="responses"' \
  -c 'model_providers.oxi.requires_openai_auth=true' \
  --model gpt-5.5 "..."
```

Codex adjunta al request todas sus cabeceras reales (`authorization`,
`chatgpt-account-id`, `originator`, `session-id`, `x-codex-*`), que viajan
intactas por el proxy hasta el backend. Medido en vivo, una petición de
`gpt-5.5`:

| Campo | Valor |
|---|---|
| `status` | `200` |
| `input_tokens` | 19.381 |
| `output_tokens` | 61 |
| `cache_read_tokens` | 5.504 |
| `ttft_ms` | 738 |
| `total_ms` | 3.470 |

Esta medición destapó un bug real de extracción (§6): el extractor de OpenAI
compartido leía los nombres de campo de Chat Completions (`prompt_tokens` /
`completion_tokens`), que la Responses API **no** usa —manda `input_tokens` /
`output_tokens`—, así que los conteos salían `null` pese al `200`. Corregido: el
extractor prueba ambos dialectos.

> **El único hueco que queda.** `cost_estimate_usd` sale `null` porque `gpt-5.5`
> aún no está en `pricing.rs` (deuda conocida: los precios son placeholders). Los
> tokens, que son lo que de verdad se factura, son exactos.

---

## 6. Simplificaciones y deuda conocida (fase 1)

- **No filtramos el chunk de `usage` de OpenAI** del stream de vuelta al cliente.
  Strippearlo cruzando fronteras de chunk es frágil y aporta poco (los clientes
  ignoran el chunk con `choices: []`). Solo mutamos el request, la respuesta va
  intacta.
- **Precios editables, no oficiales.** `src/telemetry/pricing.rs` trae valores
  por defecto aproximados. Hay que mantenerlos sincronizados con la tarifa real.
  Modelo desconocido ⇒ `cost_estimate_usd = null` (nunca un número inventado).
- **Tokens de caché sin itemizar — RESUELTO.** Detectado en una prueba real de
  `gemini-3.5-flash` (**24.433 de 63.531 tokens de input, ~38%, fueron lecturas
  de caché**), donde `input_tokens` (exacto, coincide con el CLI) se preciaba
  entero a tarifa full y `cost_estimate_usd` sobreestimaba. Cada adaptador
  extrae ahora los campos crudos de caché (`Usage.cache_read_tokens` /
  `cache_write_tokens`, ver `docs/provider-adapters.md` §4):
  - Anthropic: `cache_read_input_tokens` / `cache_creation_input_tokens`
    (APARTE del input).
  - Gemini: `cachedContentTokenCount` (SUBCONJUNTO del input).
  - OpenAI: `prompt_tokens_details.cached_tokens` /
    `input_tokens_details.cached_tokens` (SUBCONJUNTO del input). La
    Responses API además expone `input_tokens_details.cache_write_tokens`,
    que se extrae hacia `Usage.cache_write_tokens`; Chat Completions no tiene
    equivalente de cache-write.

  `telemetry::pricing::estimate_cost_usd` es el único que conoce si la caché
  de una familia es subconjunto del input o va aparte, y precia cada porción
  a su tarifa reducida sin doble contar.
- **Tokens de "thinking" de Gemini sin sumar.** `thoughtsTokenCount` se factura
  (a tarifa de output) y hoy no se contempla. Mismo bucket de deuda de coste.
- **`output_tokens_details.reasoning_tokens` de OpenAI Responses sin
  extraer.** Los modelos de razonamiento (familia GPT-5.x) facturan estos
  tokens a tarifa de output. No se extraen todavía: son habitualmente un
  SUBCONJUNTO de `output_tokens` (ya contado), y `Usage` no tiene hoy un
  campo dedicado para desglosarlos sin arriesgar doble conteo en
  `telemetry::pricing`. Mismo bucket de deuda que el "thinking" de Gemini,
  arriba.
- **Precio de Gemini flash-lite genérico.** `gemini-*-flash-lite` cae en el
  precio de la familia `flash` (más caro que el *lite* real): coste levemente
  sobreestimado hasta afinar la tabla.
- **API key en claro.** Pasa por las cabeceras. En `localhost` el riesgo es bajo,
  pero al escalar fuera de la máquina local hay que blindarlo. Nunca loguear ni
  persistir la key.

---

## 7. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/config.rs` | URLs destino por proveedor (`target_*_url`), puerto, carpeta de datos |
| `src/telemetry/logger.rs` | `RequestMetric` (los datos) + `TelemetrySink` (escritura async fuera del camino crítico) |
| `src/telemetry/metered.rs` | `MeteredBody` — envuelve el stream, mide TTFT, escanea `usage` (3 formatos), emite la métrica |
| `src/telemetry/pricing.rs` | Tabla de precios por modelo y cálculo de `cost_estimate_usd` |
| `src/middleware/proxy.rs` | Handlers por ruta + `send_and_meter` compartido (descarta `Accept-Encoding`, envuelve la respuesta); para Gemini, ruta comodín + `parse_gemini_path` |

La salida se escribe en `~/.config/oxidegate/telemetry.jsonl`, una fila JSON por
petición.

---

## 8. Cableado: cómo enrutar cada cliente por OxideGate

OxideGate solo mide lo que **pasa por él**. Cada cliente se redirige apuntando su
base-URL del proveedor al puerto local (por defecto `8080`; si está ocupado,
`OXIDEGATE_PORT=8899`). El proxy reenvía al proveedor real de forma transparente,
así que la autenticación (API key u OAuth) viaja intacta y funciona igual.

| Proveedor / cliente | Variable de entorno | Valor |
|---|---|---|
| Anthropic (Claude Code, incl. Claude Max/OAuth) | `ANTHROPIC_BASE_URL` | `http://localhost:8899` |
| Gemini (`@google/gemini-cli`, API key) | `GOOGLE_GEMINI_BASE_URL` | `http://localhost:8899` |
| OpenAI (clientes con override) | `OPENAI_BASE_URL` / `OPENAI_API_BASE` | `http://localhost:8899/v1` |

Verificado en vivo: Claude Max (OAuth) **respeta** `ANTHROPIC_BASE_URL`, y el CLI
de Gemini respeta `GOOGLE_GEMINI_BASE_URL`. Levantar dos sesiones de Claude Max a
la vez dispara `429` por límite de concurrencia de la suscripción.
