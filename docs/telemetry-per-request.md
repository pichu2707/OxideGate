# Detalle en vivo por petición — `GET /requests`

> Estado: implementado y con tests unitarios (`src/telemetry/recent.rs`).
> Proyecta en memoria lo que el Nivel 1 (`docs/telemetry-level-1.md`) ya mide
> por fila; no cambia la captura ni agrega ningún campo nuevo a
> `RequestMetric`.

---

## 1. Qué es y para qué sirve

`GET /stats` (`docs/telemetry-by-model.md`) agrega por `(proveedor, modelo)`:
sirve para responder "¿qué modelo conviene optimizar?", pero un promedio
esconde la petición puntual que se disparó, la que tuvo un cache-miss aislado
o la que tardó 8 segundos en el primer token. Para ver **esa** fila en vivo,
hasta ahora había que abrir `telemetry.jsonl` a mano.

`GET /requests` responde eso: las últimas peticiones individuales,
proyectadas para lectura rápida, en orden cronológico (más vieja primero).

```
gentle-ai ──▶ OxideGate ──▶ proveedor
                 │
                 ├──▶ telemetry.jsonl        (fila a fila, persistente)
                 └──▶ RecentRequests (RAM)   (últimas 200 filas, en vivo)
                            │
                            ▼
                      GET /requests  (snapshot JSON)
```

---

## 2. Cómo consultarlo

```sh
curl localhost:8899/requests
```

(ajustar el puerto al que use tu instancia). Devuelve `200 OK` con
`content-type: application/json`, sin autenticación: el proxy bindea en
`127.0.0.1`, igual que `/stats`.

### Ejemplo de salida

```json
[
  {
    "timestamp": "2026-07-09T14:02:11.483Z",
    "route": "/v1/messages",
    "upstream": "anthropic",
    "model": "claude-opus-4-1",
    "stream": true,
    "status": 200,
    "input_tokens": 5000,
    "output_tokens": 412,
    "cache_read_tokens": 4200,
    "cache_write_tokens": 0,
    "cost_estimate_usd": 0.0891,
    "cache_control_forced": false,
    "ttft_ms": 780.4,
    "total_ms": 3210.9
  },
  {
    "timestamp": "2026-07-09T14:02:14.117Z",
    "route": "/v1/messages",
    "upstream": "anthropic",
    "model": "claude-opus-4-1",
    "stream": true,
    "status": 200,
    "input_tokens": 5000,
    "output_tokens": 398,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0,
    "cost_estimate_usd": 0.1620,
    "cache_control_forced": false,
    "ttft_ms": 2450.7,
    "total_ms": 5980.2
  }
]
```

La segunda fila del ejemplo es exactamente el tipo de anomalía que `/stats`
no puede mostrar: mismo modelo, mismo tamaño de input, pero sin
`cache_read_tokens` y con TTFT casi 3 veces más alto. Vista sola en un
promedio, se diluye entre el resto del tráfico.

Filas ausentes de dato (p. ej. `ttft_ms` en una petición sin streaming) se
serializan como `null`, nunca como `0`: un dato ausente y un cero real son
cosas distintas (ver `docs/telemetry-level-1.md`).

---

## 3. La invariante de privacidad (léase antes de exponer este endpoint)

`RecentRequest` — el tipo que serializa cada fila — **no tiene** los campos
`prompt_hash` ni `prompt_bytes`. No es un filtro en tiempo de ejecución que
alguien pueda desactivar por error: es una garantía de compilación, porque el
campo directamente no existe en el struct. `telemetry.jsonl` sí guarda
`prompt_hash` por fila (para poder correlacionar redundancia offline), pero
esa huella nunca llega a la API HTTP.

Esto mirroriza la misma invariante que ya documenta
`docs/telemetry-by-model.md` para los agregados de `/stats` y que impone
`src/middleware/stats.rs`: el proxy no expone huellas de prompt por HTTP,
haya o no autenticación de por medio.

---

## 4. Qué señala cada campo

| Campo | Qué es | Cómo leerlo |
|---|---|---|
| `timestamp` | Instante en que se emitió la métrica (RFC 3339, UTC) | Ordena el buffer; el consumidor decide si invierte para "más nuevo arriba" |
| `route` | Ruta local que atendió el request (`/v1/messages`, …) | Distingue el dialecto de proveedor cuando hay varias rutas activas |
| `upstream` | Proveedor destino (`anthropic`, `openai`, …) | Junto con `model`, la clave de agrupación para comparar contra pares |
| `model` | Modelo solicitado, o `null` si no venía en el body | Un `null` sostenido en el tiempo suele indicar clientes mal configurados |
| `stream` | `true` si el cliente pidió SSE | Sin streaming, `ttft_ms` no aplica — ver `total_ms` en su lugar |
| `status` | Código HTTP devuelto al cliente | `>= 400` es la señal de error más barata de todas: no necesita comparación con nada |
| `input_tokens` / `output_tokens` | Tokens exactos reportados por el proveedor | `null` si el proveedor no los reportó (p. ej. request fallido antes de leer `usage`) |
| `cache_read_tokens` / `cache_write_tokens` | Tokens servidos o escritos a caché | Una fila con `cache_read_tokens` en `0`/`null` en medio de una conversación larga que sí cachea es un miss caro y aislado |
| `cost_estimate_usd` | Coste estimado en USD según `pricing.rs` | `null` si no fue calculable |
| `cache_control_forced` | `true` si OxideGate inyectó el breakpoint de `cache_control` (Palanca A) | Sirve para correlacionar si la palanca estaba activa en esa fila puntual |
| `ttft_ms` | Time To First Token en ms | `null` si no aplica (sin streaming); un valor mucho más alto que el resto del mismo modelo es la señal de latencia percibida |
| `total_ms` | Latencia total, del request al cierre de la respuesta | Junto con `ttft_ms`, permite derivar el tiempo de generación (`total_ms - ttft_ms`) fuera del endpoint |

Ninguno de estos campos es nuevo: todos ya existían en `RequestMetric`
(Nivel 1). Este endpoint no mide nada distinto, solo expone en vivo lo que
antes solo llegaba a disco.

---

## 5. Límite de memoria: 200 filas, y se pierden al reiniciar

`RECENT_CAPACITY = 200` (`src/telemetry/recent.rs`): el buffer es un
`VecDeque` que nunca guarda más de 200 requests. Al llegar la 201, se
desaloja la más vieja (`pop_front`) — memoria acotada y constante en un
proceso de larga vida, sin necesidad de configurar nada.

**Esto vive únicamente en RAM.** A diferencia de `telemetry.jsonl`, que
persiste en disco y sobrevive a un reinicio del proxy, `/requests` se vacía
por completo cada vez que el proceso de OxideGate se reinicia. Si necesitás
el historial completo, o algo más viejo que las últimas 200 peticiones, la
única fuente confiable es `telemetry.jsonl`.

| | `GET /requests` | `GET /stats` | `telemetry.jsonl` |
|---|---|---|---|
| Nivel de detalle | por petición individual | agregado por `(proveedor, modelo)` | por petición individual |
| Ventana | últimas 200 peticiones | todo el histórico del proceso | todo el histórico, en disco |
| Persistencia | en memoria, se pierde al reiniciar | en memoria, se pierde al reiniciar | persistente en disco |
| `prompt_hash` | nunca (no existe el campo) | nunca | sí, por fila |
| Para qué sirve | ver la fila atípica puntual | decidir qué modelo optimizar | análisis offline, auditoría, recuperar historial completo |

---

## 6. Cómo se calcula (diseño interno)

- **`src/telemetry/recent.rs`** es puro: no conoce axum, solo `RequestMetric`
  y su propia proyección `RecentRequest`. `RecentRequests::ingest` agrega al
  final del `VecDeque` (orden cronológico) y desaloja la más vieja si se
  supera `RECENT_CAPACITY`; `RecentRequests::snapshot` devuelve una copia
  independiente del estado actual, sin decidir orden de presentación (eso
  queda del lado del consumidor — ver el panel del monitor en
  `docs/monitor-tui.md`).
- **`src/middleware/requests.rs`** es el único archivo de esta cadena que
  conoce axum: expone `GET /requests`, toma un read-lock breve sobre el
  buffer compartido, clona el snapshot y lo serializa a JSON. El lock se
  suelta **antes** de cualquier punto de suspensión (`.await`), igual que
  hace `middleware/stats.rs` con `StatsRegistry`.
- El buffer vive en `Arc<RwLock<RecentRequests>>`, alimentado por la MISMA
  task de drenaje en segundo plano que ya alimenta `StatsRegistry`
  (`src/telemetry/logger.rs`). No hay una segunda ruta de instrumentación:
  cada `RequestMetric` que se escribe a disco es también la que alimenta este
  buffer, en el mismo lugar y al mismo tiempo. Esto mantiene la captura
  **fuera del camino crítico del request** — igual que el resto de la
  telemetría.

---

## 7. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/recent.rs` | `RecentRequests`, `RecentRequest` — buffer FIFO acotado y proyección, sin axum |
| `src/telemetry/logger.rs` | `TelemetrySink::spawn` alimenta el buffer en la misma task que escribe el JSONL; `TelemetrySink::recent()` expone el `Arc<RwLock<RecentRequests>>` |
| `src/middleware/requests.rs` | `handle_requests` — el handler HTTP de `GET /requests` |
| `src/main.rs` | Registra la ruta `/requests` en el `Router` |
