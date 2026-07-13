# OxideGate

> Proxy local en Rust que se sienta entre los clientes de IA (gentle-ai,
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
| **Perillas de velocidad** | Captura `requested_effort`, `requested_speed` y `served_speed` (`output_config.effort` y `speed` de Anthropic) por petición, expuestas en `GET /requests` y en el monitor. | ✅ |

---

## Instalación

```sh
brew install pichu2707/tap/oxidegate
```

Instala dos ejecutables: **`oxidegate`** (el proxy) y **`oxidegate-monitor`** (el
dashboard de terminal). Hay un tercero, `oxidegate-bench`, que es una barrida de
benchmark para desarrollo y **no se instala**: no tiene nada que hacer en el PATH
de nadie.

Desde el código: `cargo run --bin oxidegate`.

---

## Arranque rápido

```sh
# 1. Levantar el proxy. Por defecto escucha en 8080; usar OXIDEGATE_PORT si ese
#    puerto está ocupado — lo está más a menudo de lo que parece.
OXIDEGATE_PORT=8899 oxidegate

# 2. Apuntar el cliente a OxideGate en vez de al proveedor:
export ANTHROPIC_BASE_URL=http://127.0.0.1:8899

# 3. Usar el agente como siempre. OxideGate reenvía la petición INTACTA y la mide
#    de paso. Después:
curl 127.0.0.1:8899/stats     # agregado por modelo
oxidegate-monitor             # o el dashboard en vivo (misma OXIDEGATE_PORT)
```

> **`ANTHROPIC_BASE_URL` va SIN `/v1`.** El cliente le agrega la ruta él mismo
> (`/v1/messages`). Si se agrega el `/v1`, la petición sale a `/v1/v1/messages` y
> el proxy devuelve **404**. Es el error más fácil de cometer y el más difícil de
> diagnosticar, porque parece que la herramienta no funciona.

### Y una advertencia por adelantado

Poner **cualquier** `ANTHROPIC_BASE_URL` que no sea el de Anthropic hace que
Claude Code **deje de diferir sus esquemas MCP** y los mande todos de golpe.
OxideGate es uno de esos base URL. Es decir: **parte de los bytes que se ven
medidos existen porque el medidor está en el camino.**

No es una hipótesis: está medido con grupo de control y servidor sonda en
[`docs/optimizer-tool-search.md`](docs/optimizer-tool-search.md) §3.
[`oxidegate-lens`](https://github.com/pichu2707/oxidegate-lens) lo indica en el
propio reporte, en vez de presentar un ahorro que no existe.

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

1. Levante el proxy **sin** la optimización y mande tráfico.
2. En el monitor, pulse **`b`** para marcar el *baseline*.
3. Reinicie el proxy con la optimización (p. ej. `OXIDEGATE_FORCE_CACHE=true`).
4. Observe el panel **Δ desde baseline**: el `cache-hit` subiendo, el coste/token
   bajando, los `tok/s` — el "después" limpio, sin que el "antes" lo diluya.

Teclas: `q` salir · `b` baseline · `r` reset · ↑/↓ elegir modelo ·
`p` panel por petición · `c` cambiar de vista (latencia / contexto).
`cargo run --bin oxidegate-monitor -- --once` da la foto en texto plano (headless).

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

Fuera de la TUI, [`oxidegate-lens`](https://github.com/pichu2707/oxidegate-lens)
imprime el mismo desglose en una tabla, con el ahorro por petición de cada
servidor MCP. Es un proyecto aparte que solo **lee** `GET /stats` y
`GET /requests`: la medición vive aquí, la presentación allá.

### La segunda palanca: `--tools`, no `--disallowedTools`

> **Advertencia que conviene no ignorar: `--disallowedTools` NO reduce el
> body.** Es una puerta de permiso, no de payload: el esquema completo de
> la herramienta se sigue enviando y se sigue pagando en cada turno, el
> modelo lo sigue leyendo; lo único que cambia es que tiene prohibido
> ejecutarla. Medido: `--disallowedTools "Bash" "Edit" "Write"` ahorra
> −421 B sobre 86.198 B de `tools` (0,5%). La palanca que sí controla el
> array de esquemas es `--tools <lista>`: con ella, los mismos 86.198 B
> bajan a 4.371 B (−94,9%) usando solo `Read` y `Bash`. Detalle completo
> y las cuatro sondas en [`docs/context-tax.md`](docs/context-tax.md) §5.

Apilando las dos palancas (config de MCP + `--tools`) sobre el mismo probe:

```
  Claude Code, sin cambios          224.653 B
  + --strict-mcp-config, sin MCP    149.221 B   (-33,6%)
  + --tools Read,Bash                51.540 B   (-77,1%)
```

El 77% del body es removible SI la tarea no necesita más que leer y correr
comandos. El costo es real: sin `Edit`, `Write` ni delegación a subagentes,
un agente así no puede editar código ni buscar por patrón. Es el trade-off
de tener un agente con capacidad de actuar, no algo para desactivar sin
pensarlo — pero no toda tarea necesita esa capacidad completa.

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
| [`docs/findings.md`](docs/findings.md) | Punto de entrada: qué se probó, qué se descartó y qué se retractó, organizado por conclusión |
| [`docs/telemetry-level-1.md`](docs/telemetry-level-1.md) | Qué mide el Nivel 1 y por qué; la trampa del token entre proveedores |
| [`docs/provider-adapters.md`](docs/provider-adapters.md) | El trait `Provider` y el corte por proveedor |
| [`docs/optimizer-prompt-cache.md`](docs/optimizer-prompt-cache.md) | Palanca A: forzado de prompt caching de Anthropic |
| [`docs/optimizer-dedup.md`](docs/optimizer-dedup.md) | Palanca B: dedup de respuestas por `prompt_hash` (descartada para tráfico conversacional, con evidencia) |
| [`docs/optimizer-claude-md.md`](docs/optimizer-claude-md.md) | El `CLAUDE.md` lean: −29.509 B/petición medidos en el cable, y un A/B de comportamiento (la delegación sobrevive al lean; el guardado proactivo no es medible en modo `-p`) |
| [`docs/context-tax.md`](docs/context-tax.md) | El impuesto de contexto: descomposición medida de costo y latencia de una sesión real de agente, y el piso del harness |
| [`docs/telemetry-by-model.md`](docs/telemetry-by-model.md) | El endpoint `GET /stats` y qué señala cada métrica |
| [`docs/telemetry-per-request.md`](docs/telemetry-per-request.md) | El endpoint `GET /requests`: detalle en vivo por petición, la invariante de privacidad y el límite de 200 filas |
| [`docs/speed.md`](docs/speed.md) | Tokens y tiempo son monedas distintas: por qué el TTFT no correlaciona con nada medido, y las dos palancas que sí mueven el tok/s |
| [`docs/monitor-tui.md`](docs/monitor-tui.md) | El monitor de terminal en tiempo real |
| [`docs/benchmark.md`](docs/benchmark.md) | El harness de benchmark (`bench`) |

---

## Roadmap

**Hecho** ✅ — telemetría Nivel 1, adaptadores por proveedor, coste cache-aware,
Palanca A (forzado de caché), agregación por modelo (`/stats`), monitor TUI,
**decomposición de `prompt_bytes` por componente** (`system` / `tools` /
historial / turno actual, campos `context_*_bytes` en `RequestMetric`) —
usada para medir el efecto de `--tools` en
[`docs/context-tax.md`](docs/context-tax.md) §5.

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
- **Segunda barrida de benchmark** con output largo (throughput de generación).
- **Endurecer `telemetry.jsonl`** para reabrirlo si se rota o se borra.
- **Precios reales por modelo** — deuda archivada: los ratios de caché ya son
  correctos; los precios-base son placeholders y, para el objetivo (ahorrar
  tokens y latencia), la aproximación alcanza.

> Hallazgo central que guía las prioridades: el overhead del harness domina el
> coste. Claude Code inyecta ~7.368 tokens de contexto por llamada; un "Responde
> ok" cuesta ~20.000× lo mismo crudo. La palanca real es el **conteo de tokens y
> la latencia**, no la precisión del precio.
