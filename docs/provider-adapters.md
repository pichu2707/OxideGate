# Adaptadores por proveedor — el trait `Provider`

> Estado: refactor aplicado y verificado (build verde, tests migrados, sin
> regresiones). Cada proveedor (Anthropic, OpenAI, Gemini) vive aislado en su
> propio módulo y posee su dialecto de principio a fin.

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

Cada proveedor implementa un contrato único que encapsula ambas puntas:

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Construye el request saliente desde el entrante:
    /// URL destino, ruta, modelo, flag de stream y body (mutado si hace falta).
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing;

    /// Actualiza los contadores de tokens leyendo el `usage` con los nombres
    /// de campo de ESTE proveedor.
    fn extract_usage(&self, value: &serde_json::Value, usage: &mut Usage);
}
```

Tipos de apoyo:

- `Incoming { path, query, body }` — lo que el handler sabe del request entrante.
  Cubre tanto rutas basadas en body (Anthropic/OpenAI) como en path (Gemini).
- `Outgoing { url, route, upstream, model, stream, prompt_hash, prompt_bytes, body }`
  — la petición ya resuelta y lista para reenviar, con todo lo que la métrica
  necesita saber de antemano.
- `Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens }`
  — acumulador de tokens. Los campos de caché se guardan CRUDOS, tal como los
  reporta cada proveedor (sin normalizar ni restar de `input_tokens`); quién
  sabe si la caché es subconjunto del input o va aparte es `telemetry/pricing.rs`,
  no este struct ni la capa de medición.

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
| `provider/anthropic.rs` | Ruta `/v1/messages`; `usage` en raíz o anidado bajo `message`.          |
| `provider/openai.rs`    | `OpenAiChat` (`/v1/chat/completions`, inyecta `stream_options.include_usage` en streaming) y `OpenAiResponses` (`/v1/responses`, sin inyección, `usage` bajo `response`). |
| `provider/gemini.rs`    | Ruta comodín `/v1beta/*`; modelo y método en la URL; `usageMetadata`.   |

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
