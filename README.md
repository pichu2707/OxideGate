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

### Variables de entorno

| Variable | Para qué | Default |
|---|---|---|
| `OXIDEGATE_PORT` | Puerto local del proxy (y del monitor) | `8080` |
| `ANTHROPIC_API_BASE` / `OPENAI_API_BASE` / `GEMINI_API_BASE` | Host de cada proveedor | API pública de cada uno |
| `OXIDEGATE_FORCE_CACHE` | Palanca A: fuerza el prompt caching de Anthropic | `false` (apagado) |
| `OXIDEGATE_STATS_URL` | URL que consulta el monitor | `http://127.0.0.1:{OXIDEGATE_PORT}/stats` |

La telemetría se escribe en `~/.config/oxidegate/telemetry.jsonl` (una línea
JSON por petición), fuera del camino crítico del request.

---

## Ver una mejora (antes/después)

El monitor es la forma de comprobar que una optimización sirve:

1. Levantá el proxy **sin** la optimización y mandá tráfico.
2. En el monitor, apretá **`b`** para marcar el *baseline*.
3. Reiniciá el proxy con la optimización (p. ej. `OXIDEGATE_FORCE_CACHE=true`).
4. Mirá el panel **Δ desde baseline**: el `cache-hit` subiendo, el coste/token
   bajando, los `tok/s` — el "después" limpio, sin que el "antes" lo diluya.

Teclas: `q` salir · `b` baseline · `r` reset · ↑/↓ elegir modelo.
`cargo run --bin monitor -- --once` da la foto en texto plano (headless).

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
| [`docs/telemetry-by-model.md`](docs/telemetry-by-model.md) | El endpoint `GET /stats` y qué señala cada métrica |
| [`docs/monitor-tui.md`](docs/monitor-tui.md) | El monitor de terminal en tiempo real |
| [`docs/benchmark.md`](docs/benchmark.md) | El harness de benchmark (`bench`) |

---

## Roadmap

**Hecho** ✅ — telemetría Nivel 1, adaptadores por proveedor, coste cache-aware,
Palanca A (forzado de caché), agregación por modelo (`/stats`), monitor TUI.

**Pendiente**
- **Palanca B — dedup por `prompt_hash`.** Servir respuesta cacheada ante
  peticiones idénticas (0 tokens, ~0 latencia). El `redundancy_rate` del monitor
  chiva dónde conviene. Requiere una exploración de corrección antes de codear:
  cachear salidas de un LLM es semánticamente delicado (no-determinismo,
  staleness, corrección de agentes).
- **Segunda barrida de benchmark** con output largo (throughput de generación).
- **Endurecer `telemetry.jsonl`** para reabrirlo si se rota o se borra.
- **Precios reales por modelo** — deuda archivada: los ratios de caché ya son
  correctos; los precios-base son placeholders y, para el objetivo (ahorrar
  tokens y latencia), la aproximación alcanza.

> Hallazgo central que guía las prioridades: el overhead del harness domina el
> coste. Claude Code inyecta ~7.368 tokens de contexto por llamada; un "Responde
> ok" cuesta ~20.000× lo mismo crudo. La palanca real es el **conteo de tokens y
> la latencia**, no la precisión del precio.
