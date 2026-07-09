# OxideGate

> Proxy local en Rust que se sienta entre tus clientes de IA (gentle-ai,
> agentes, SDKs, Claude Code) y los proveedores (Anthropic, OpenAI, Gemini).
> **Mide** cada petición —coste, tokens, latencia— por proveedor **y por
> modelo**, y empieza a **optimizar** el tráfico sin romper la transparencia.

El principio, no negociable:

> **No se puede optimizar lo que no se mide.**

Primero medimos cada petición real (Nivel 1). Sobre esa medición construimos las
optimizaciones (Nivel 2: caché, dedup, enrutado por coste) y comprobamos su
impacto **en vivo**, comparando el antes y el después.

---

## Estado actual

| Capa | Qué hace | Estado |
|---|---|---|
| **Nivel 1 — Telemetría** | Una fila por petición con tokens/coste exactos (del `usage` real), TTFT, latencia total y tokens/seg. Validado en vivo para los 3 proveedores. | ✅ |
| **Adaptadores por proveedor** | Cada proveedor (Anthropic, OpenAI chat/responses, Gemini) aislado detrás del trait `Provider`: dueño de su request y de su `usage`. | ✅ |
| **Coste cache-aware** | Itemiza tokens de caché (`cache_read`/`cache_write`) y cobra cada uno a su tarifa; `pricing.rs` es la única fuente de verdad. | ✅ |
| **Optimizador · Palanca A** | Fuerza el prompt caching de Anthropic (inyecta `cache_control`) para clientes que no cachean. Detrás de un flag, apagado por defecto. | ✅ |
| **Agregación por modelo** | `GET /stats` devuelve, en vivo, señales por `(proveedor, modelo)`: cache-hit, redundancia, coste, latencias. | ✅ |
| **Monitor TUI** | Dashboard de terminal en tiempo real con vista **antes/después** (baseline) para ver el impacto de cada optimización. | ✅ |
| **Detalle por request** | `GET /requests` + panel `p` del monitor: las últimas 200 peticiones individuales en vivo, con detección de outliers (error, cache-miss, TTFT lento, generación lenta). | ✅ |

---

## Arranque rápido

El proyecto tiene **tres binarios**: `oxidegate` (el proxy), `monitor` (el
dashboard) y `bench` (barrida de benchmark controlada).

```sh
# 1. Levantar el proxy. Por defecto escucha en 8080; usá OXIDEGATE_PORT si está
#    ocupado (en la máquina de desarrollo se usa 8899).
OXIDEGATE_PORT=8899 cargo run --bin oxidegate

# 2. Apuntar tu cliente a OxideGate en vez de al proveedor, p. ej.:
#    ANTHROPIC_BASE_URL=http://localhost:8899/v1
#    (OxideGate reenvía la petición intacta y la mide de paso.)

# 3. Ver la telemetría agregada por modelo, en vivo:
curl localhost:8899/stats

# 4. O el monitor de terminal en tiempo real (misma OXIDEGATE_PORT que el proxy):
OXIDEGATE_PORT=8899 cargo run --bin monitor
```

### Rutas espejo

| Ruta | Proveedor |
|---|---|
| `POST /v1/messages` | Anthropic |
| `POST /v1/chat/completions` | OpenAI (Chat Completions) |
| `POST /v1/responses` | OpenAI (Responses API) |
| `POST /v1beta/*` | Google Gemini |
| `GET  /stats` | Agregación por modelo (JSON) |
| `GET  /requests` | Últimas 200 peticiones individuales, en vivo (JSON) |

### Variables de entorno

| Variable | Para qué | Default |
|---|---|---|
| `OXIDEGATE_PORT` | Puerto local del proxy (y del monitor) | `8080` |
| `ANTHROPIC_API_BASE` / `OPENAI_API_BASE` / `GEMINI_API_BASE` | Host de cada proveedor | API pública de cada uno |
| `OXIDEGATE_FORCE_CACHE` | Palanca A: fuerza el prompt caching de Anthropic | `false` (apagado) |
| `OXIDEGATE_STATS_URL` | URL que consulta el monitor para `/stats` | `http://127.0.0.1:{OXIDEGATE_PORT}/stats` |
| `OXIDEGATE_REQUESTS_URL` | URL que consulta el monitor para `/requests` | derivada de `OXIDEGATE_STATS_URL` (sufijo `/stats` → `/requests`), o `http://127.0.0.1:{OXIDEGATE_PORT}/requests` |

La telemetría se escribe en `~/.config/oxidegate/telemetry.jsonl` (una línea
JSON por petición), fuera del camino crítico del request.

---

## Ver una mejora (antes/después)

El monitor es la forma de comprobar que una optimización sirve:

1. Levantá el proxy **sin** la optimización y mandá tráfico.
2. En el monitor, pulse **`b`** para marcar el *baseline*.
3. Reiniciá el proxy con la optimización (p. ej. `OXIDEGATE_FORCE_CACHE=true`).
4. Mirá el panel **Δ desde baseline**: el `cache-hit` subiendo, el coste/token
   bajando, los `tok/s` — el "después" limpio, sin que el "antes" lo diluya.

Teclas: `q` salir · `b` baseline · `r` reset · ↑/↓ elegir modelo ·
`p` panel por petición · `c` cambiar de vista (latencia / contexto).
`cargo run --bin monitor -- --once` da la foto en texto plano (headless).

---

## Bajar el impuesto de contexto

La primera optimización que reveló la medición no está en el código de este
repo: está en la configuración del cliente. Los esquemas de herramientas son
el grueso del body, se reenvían enteros en cada turno y no decrecen nunca.

Medido con este mismo proxy, sonda idéntica, comparando peticiones del mismo
tamaño de historial:

| Configuración | `tools` | Ahorro |
|---|---|---|
| 4 servidores MCP (Gmail, Drive, Calendar, Engram) | 159.100 B | — |
| Solo Engram | 103.701 B | **−55.399 B por petición** |
| Ningún MCP (piso de herramientas nativas) | 86.198 B | −72.902 B |

Los tres conectores de Google cuestan el 76% del peaje de MCP y no se usan
para nada en un proxy de Rust. Este repo trae `.claude/mcp-lean.json` con solo
Engram:

```sh
claude --strict-mcp-config --mcp-config .claude/mcp-lean.json
```

Dos advertencias que cuestan caro si se ignoran:

- **El archivo por sí solo no hace nada.** Hace falta `--strict-mcp-config`,
  porque los conectores de Google vienen de la cuenta de claude.ai, no de un
  archivo local: una config de proyecto SUMA servidores, no los quita.
- **No lo llames `.mcp.json`.** Ese nombre se auto-carga, y entonces Engram
  quedaría cargado dos veces (el del plugin y el del archivo) además de los
  tres de Google. Peor que no hacer nada.

El efecto se comprueba con el propio monitor: tecla `p`, luego `c`, y se
observa la columna `tools`. Es el circuito completo — la medición señala la
oportunidad, la configuración la ejecuta, el monitor comprueba que sirvió.

---

## Arquitectura

```
cliente ──HTTP──▶  OxideGate  ──HTTPS──▶  proveedor
                      │
        middleware/proxy.rs  (transporte genérico)
                      │  prepare() / extract_usage()
              provider/*.rs  (dialecto por proveedor)
                      │
          telemetry/metered.rs  (mide: TTFT, usage, coste)
                      │
        ┌─────────────┴──────────────┐
        ▼                            ▼
 telemetry.jsonl            telemetry/stats.rs  (agregado por modelo, RAM)
 (fila a fila)                       │
                                GET /stats  ◀── src/bin/monitor.rs (TUI)
```

Convenciones del proyecto: **documentación total** (`//!` por archivo, `///`
por función con su contrato) y **responsabilidad única estricta** por módulo.

---

## Documentación

| Doc | Tema |
|---|---|
| [`docs/telemetry-level-1.md`](docs/telemetry-level-1.md) | Qué mide el Nivel 1 y por qué; la trampa del token entre proveedores |
| [`docs/provider-adapters.md`](docs/provider-adapters.md) | El trait `Provider` y el corte por proveedor |
| [`docs/optimizer-prompt-cache.md`](docs/optimizer-prompt-cache.md) | Palanca A: forzado de prompt caching de Anthropic |
| [`docs/optimizer-dedup.md`](docs/optimizer-dedup.md) | Palanca B: dedup de respuestas por `prompt_hash` (descartada para tráfico conversacional, con evidencia) |
| [`docs/context-tax.md`](docs/context-tax.md) | El impuesto de contexto: descomposición medida de costo y latencia de una sesión real de agente, y el piso del harness |
| [`docs/telemetry-by-model.md`](docs/telemetry-by-model.md) | El endpoint `GET /stats` y qué señala cada métrica |
| [`docs/telemetry-per-request.md`](docs/telemetry-per-request.md) | El endpoint `GET /requests`: detalle en vivo por petición, la invariante de privacidad y el límite de 200 filas |
| [`docs/monitor-tui.md`](docs/monitor-tui.md) | El monitor de terminal en tiempo real |
| [`docs/benchmark.md`](docs/benchmark.md) | El harness de benchmark (`bench`) |

---

## Roadmap

**Hecho** ✅ — telemetría Nivel 1, adaptadores por proveedor, coste cache-aware,
Palanca A (forzado de caché), agregación por modelo (`/stats`), monitor TUI.

**Descartado** ⛔ (con evidencia, para tráfico conversacional)
- **Palanca B — dedup por `prompt_hash`.** Medido contra tráfico real de
  agente: `redundancy_rate` es 0.0 por construcción (el hash se calcula
  sobre el body completo, y `messages` crece en cada turno), el input fresco
  que podría ahorrarse es solo 3.0% del costo, y Claude Code siempre
  streamea (el v1 exigía `stream=false`). Detalle completo en
  [`docs/optimizer-dedup.md`](docs/optimizer-dedup.md) §0. El diseño queda
  vigente para otra forma de tráfico: requests idénticos no-streaming
  (reintentos, CI, batch, fan-out de subagentes).

**Pendiente**
- **Decomponer `prompt_bytes` por componente** (`system` / `tools` /
  historial / turno actual) en vez de un número plano — es el paso que falta
  para responder "de lo que pago, cuánto es trabajo y cuánto es ceremonia".
  Ver [`docs/context-tax.md`](docs/context-tax.md) §8.
- **Segunda barrida de benchmark** con output largo (throughput de generación).
- **Endurecer `telemetry.jsonl`** para reabrirlo si se rota o se borra.
- **Precios reales por modelo** — deuda archivada: los ratios de caché ya son
  correctos; los precios-base son placeholders y, para el objetivo (ahorrar
  tokens y latencia), la aproximación alcanza.

> Hallazgo central que guía las prioridades: el overhead del harness domina el
> coste. Claude Code inyecta ~7.368 tokens de contexto por llamada; un "Responde
> ok" cuesta ~20.000× lo mismo crudo. La palanca real es el **conteo de tokens y
> la latencia**, no la precisión del precio.
