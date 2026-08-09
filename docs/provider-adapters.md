# Adaptadores por proveedor — el trait `Provider`

> Estado: aplicado y verificado. Cada proveedor —Anthropic, OpenAI (Chat,
> Responses y Codex), Gemini y **ollama nativo**— vive aislado en su propio
> módulo y posee su dialecto de principio a fin.

---

## 1. El problema que resuelve

En el Nivel 1 los tres proveedores nacieron incrustados en dos archivos, y el
conocimiento de cada uno quedó **partido en dos lugares**:

- El **request** (cómo se arma la URL, dónde viven `model` y `stream`, si hay que
  mutar el body) vivía en `middleware/proxy.rs`.
- La **respuesta** (qué nombres tiene el `usage`: `input_tokens` vs
  `prompt_tokens` vs `promptTokenCount`) vivía hardcodeada en un único método
  `extract_usage` dentro de `telemetry/metered.rs`.

Eso rompía la responsabilidad única: `proxy.rs` hacía de router, de transporte y
además conocía los tres dialectos; `metered.rs` mezclaba la mecánica de medición
con el vocabulario de cada API. Agregar un proveedor o afinar un dato obligaba a
tocar código entrelazado en sitios distintos.

> **Regla del corte:** un adaptador de verdad posee las DOS puntas del dialecto —
> el request Y la respuesta. Si solo se mueve una, el proveedor sigue viviendo a
> medias en la capa genérica.

---

## 2. El trait

Cada proveedor implementa un contrato único que encapsula ambas puntas. Nació
con tres métodos y hoy tiene ocho; **cinco de ellos no tienen implementación
por defecto**, y eso es una decisión de diseño, no un descuido (ver §5).

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Construye el request saliente desde el entrante:
    /// URL destino, ruta, modelo, flag de stream y body (mutado si hace falta).
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing;

    /// Actualiza los contadores de tokens leyendo el `usage` con los nombres
    /// de campo de ESTE proveedor.
    fn extract_usage(&self, value: &Value, usage: &mut Usage);

    /// Acumula las invocaciones de herramienta que aparezcan. SIN DEFAULT.
    fn extract_tool_use(&self, value: &Value, calls: &mut ToolCalls);

    /// `true` si el método de arriba está implementado de verdad. SIN DEFAULT.
    fn captura_invocaciones(&self) -> bool;

    /// Extrae el JSON de UNA línea del stream, o `None` si no lo lleva.
    /// Es lo que distingue SSE de NDJSON. SIN DEFAULT.
    fn payload_de_linea<'a>(&self, linea: &'a str) -> Option<&'a str>;

    /// Descompone el body por componente (system, tools, historial…).
    /// SIN DEFAULT.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown>;

    /// `(nombre, valor)` de cada herramienta declarada en el body.
    /// SIN DEFAULT.
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>>;
}
```

Tipos de apoyo:

- `Incoming { path, query, body }` — lo que el handler sabe del request
  entrante. Cubre tanto rutas basadas en body (Anthropic/OpenAI/ollama) como en
  path (Gemini).
- `Outgoing { url, route, upstream, model, stream, prompt_hash, prompt_bytes,
  body, … }` — la petición ya resuelta y lista para reenviar, con todo lo que
  la métrica necesita saber de antemano.
- `Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
  load_us, prompt_eval_us, eval_us, … }` — acumulador. Los campos de caché se
  guardan **crudos**, tal como los reporta cada proveedor (sin normalizar ni
  restar de `input_tokens`); quién sabe si la caché es subconjunto del input o
  va aparte es `telemetry/pricing.rs`, no este struct ni la capa de medición.
  Los tres `*_us` solo los reporta un motor **local**: son lo único que separa
  cargar el modelo de inferir con él.

---

## 3. El reparto de responsabilidades

```
   ┌─────────────┐   prepare()    ┌──────────────┐
   │  proxy.rs   │ ─────────────▶ │ provider/*.rs│
   │ (transporte │                │  (dialecto)  │
   │  genérico)  │ ◀───────────── │              │
   └─────────────┘  extract_usage └──────────────┘
          │                              ▲
          │ send_and_meter               │
          ▼                              │
   ┌─────────────┐   delega usage        │
   │ metered.rs  │ ──────────────────────┘
   │ (medición   │
   │   pura)     │
   └─────────────┘
```

| Módulo              | Única responsabilidad                                                      |
| ------------------- | -------------------------------------------------------------------------- |
| `middleware/proxy.rs` | Transporte genérico: leer body, delegar en `prepare`, reenviar y medir. **No conoce ningún proveedor concreto.** |
| `telemetry/metered.rs` | Mecánica de medición: TTFT, buffer de líneas SSE, coste, emisión idempotente. Delega la forma del `usage` en `provider.extract_usage`. |
| `provider/anthropic.rs` | Ruta `/v1/messages`; `usage` en raíz o anidado bajo `message`. **El único que captura invocaciones de herramienta.** |
| `provider/openai.rs`    | `OpenAiChat` (`/v1/chat/completions`, inyecta `stream_options.include_usage` en streaming), `OpenAiResponses` (`/v1/responses`, sin inyección, `usage` bajo `response`) y `OpenAiCodexResponses` (`/v1/codex/responses`, mismo dialecto vía OAuth de suscripción). |
| `provider/gemini.rs`    | Ruta comodín `/v1beta/*`; modelo y método en la URL; `usageMetadata`.   |
| `provider/ollama.rs`    | Rutas `/api/generate` y `/api/chat`; **NDJSON, no SSE**; `stream` por defecto **true**; publica `load_us`/`prompt_eval_us`/`eval_us`. |

Seis instancias `static`, una por dialecto: `ANTHROPIC`, `OPENAI_CHAT`,
`OPENAI_RESPONSES`, `OPENAI_CODEX_RESPONSES`, `GEMINI`, `OLLAMA`.

Y una tabla que resume en qué se diferencian, que es justo lo que el trait
obliga a declarar:

| | forma del stream | `captura_invocaciones` |
|---|---|:---:|
| Anthropic | SSE (`payload_sse`) | ✅ |
| OpenAI Chat / Responses / Codex | SSE (`payload_sse`) | ❌ |
| Gemini | SSE (`payload_sse`) | ❌ |
| **ollama** | **NDJSON** (línea entera) | ❌ |

Un `❌` en la segunda columna **no** significa «no hubo invocaciones»: significa
«ese dialecto no se ha capturado todavía contra tráfico real». La fila publica
`None`, no un `Some` con listas vacías, y esa distinción es el contrato — ver
§5.

Los handlers de `proxy.rs` son finos: cada uno instancia el proveedor de su ruta
y llama al pipeline compartido `send_and_meter`. `MeteredBody` sostiene un
`&'static dyn Provider` (los proveedores son structs de tamaño cero, expuestos
como instancias `static`) y le pide la extracción del `usage` a medida que el
stream fluye.

---

## 4. Por qué importó para lo que vino después

Este corte no fue cosmético: **desbloqueó la itemización de caché**, ya
resuelta. Capturar los tokens de caché — antes no itemizados, lo que
sobreestimaba el coste — resultó ser un cambio local a cada proveedor:

- Anthropic suma `cache_read_input_tokens` → `cache_read_tokens` y
  `cache_creation_input_tokens` → `cache_write_tokens`, APARTE de
  `input_tokens` (así los reporta la API).
- Gemini suma `cachedContentTokenCount` → `cache_read_tokens`, SUBCONJUNTO de
  `promptTokenCount` (no se resta: `input_tokens` queda crudo).
- OpenAI (Chat y Responses) suma `*_tokens_details.cached_tokens` →
  `cache_read_tokens`, también SUBCONJUNTO del input.

Cada proveedor extrae estos campos crudos dentro de su propio `extract_usage`,
ampliando `Usage` (ver sección 2). Ni `proxy.rs` ni `metered.rs` se enteran:
solo reenvían los cuatro contadores hacia `telemetry::pricing`. Ahí — y
únicamente ahí — vive el conocimiento de si la caché de una familia es
subconjunto del input (Gemini, OpenAI) o va aparte (Anthropic), evitando el
doble conteo al calcular `estimate_cost_usd`. Ese es el retorno de haber
puesto el dialecto donde corresponde: la itemización de caché no tocó ni el
transporte genérico ni la mecánica de medición, solo los adaptadores y la
tabla de precios.

---

## 5. La regla del sin-default: que no compile obliga a decidir

Cinco de los ocho métodos del trait **no tienen implementación por defecto**, y
cada vez que se añadió uno se tomó la misma decisión a propósito.

La tentación es obvia. Un `Default` que devuelva `None`, o una lista vacía, o
`false`, hace que añadir un proveedor cueste dos métodos en vez de siete.
El problema es **qué publica ese proveedor mientras nadie mira**:

| Método | Lo que heredaría un default | Lo que significaría en la fila |
|---|---|---|
| `decompose` | `None` | «no se pudo desglosar» — indistinguible de un body raro |
| `tool_entries` | `None` | «no declaró herramientas» — indistinguible de la verdad |
| `extract_tool_use` | cuerpo vacío | «no se invocó nada» — indistinguible de la verdad |
| `captura_invocaciones` | `false` (o peor, `true`) | el recomendador MCP contaría «servidor sin usar» cada petición |
| `payload_de_linea` | SSE | **cero tokens en silencio** contra cualquier dialecto que no sea SSE |

Ninguno de esos casos **falla**. Todos publican un número plausible, y un
número plausible no se investiga. Es exactamente el modo de fallo que este
proyecto persigue en todas partes: no el error ruidoso, sino el dato falso que
nadie mira dos veces.

Que no compile es la única forma de que la decisión se tome **en el momento de
añadir el proveedor**, por quien sabe cómo es su dialecto, y no seis meses
después mirando una columna rara.

> **El coste real de la regla**: añadir `payload_de_linea` obligó a tocar los
> cinco proveedores existentes aunque cuatro de ellos hacen exactamente lo
> mismo. Ese es el precio, y se paga a sabiendas.

Para que ese precio no se convierta en cuatro copias que puedan divergir, la
regla SSE vive en **un solo sitio**:

```rust
pub fn payload_sse(linea: &str) -> Option<&str> {
    let payload = linea.trim().strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" { return None; }
    Some(payload)
}
```

Cada dialecto de nube la llama con una línea (`super::payload_sse(linea)`).
Compartir la implementación y **seguir obligando a declararla** son cosas
distintas, y aquí se hacen las dos.

---

## 6. Qué forzó el NDJSON, y por qué importó

`payload_de_linea` es el método más reciente y el que mejor ilustra por qué el
corte tenía que ser así.

Los cuatro dialectos de nube mandan **SSE**: `data: {json}`, línea a línea. El
escáner de `telemetry/metered.rs` daba eso por sentado y exigía el prefijo
`data:` a todo el mundo.

Ollama nativo manda **NDJSON**: un objeto JSON por línea, **sin prefijo**, y
los totales viajan en la última línea, la del `done: true`.

Contra ese dialecto, el escáner habría ignorado **todas** las líneas y
publicado **cero tokens**. No un error, no un 500: un cero. Y un cero en
`output_tokens` es indistinguible de una respuesta que de verdad no generó
nada.

La salida fácil habría sido una bandera en el escáner:

```rust
// NO se hizo, y por esto:
struct UsageScanner { is_ndjson: bool, /* … */ }
```

`telemetry/metered.rs` se declara **«mecánica pura de medición: no conoce el
dialecto de ningún proveedor concreto»**. Un `is_ndjson` ahí rompe justo esa
frase, y la rompe de la peor manera: no con un módulo que sabe de un proveedor,
sino con un booleano que sabe de dos y no tiene sitio donde crecer cuando
aparezca el tercero.

El conocimiento del dialecto se fue donde ya vivían `extract_usage` y
`decompose`. El escáner quedó con una línea menos de la que tenía:

```rust
let Some(payload) = self.provider.payload_de_linea(text) else { return; };
```

Y una decisión más de dialecto que ollama obligó a tomar, con el mismo
criterio: **el default de `stream` es `true`**, al revés que en OpenAI. Darlo
por `false` haría que el escáner leyera un NDJSON entero como un solo JSON y no
encontrara el `usage` de nadie. Hay test.

---

## 7. Lo que el corte sigue sin resolver

- **`decompose` no puede compartirse, pero el criterio de historial sí.** Qué
  es «historial» y qué es «último turno» no puede depender del proveedor, así
  que esa parte vive en un helper compartido (`split_history_and_last_turn`) y
  cada `decompose` lo llama. Reimplantarlo por dialecto es la forma más fácil
  de que dos proveedores midan cosas distintas con el mismo nombre.
- **`captura_invocaciones` es deuda declarada, no diseño.** Cuatro de los seis
  proveedores devuelven `false` porque su dialecto de tool-use no se ha
  capturado todavía contra tráfico real. El campo existe para que esa deuda sea
  visible en la fila en vez de quedarse en un TODO.
