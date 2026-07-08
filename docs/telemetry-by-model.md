# Agregación en vivo por modelo — `GET /stats`

> Estado: implementado y con tests unitarios (`src/telemetry/stats.rs`). Agrega
> en memoria lo que el Nivel 1 (`docs/telemetry-level-1.md`) ya mide por fila;
> no cambia la captura ni agrega ningún campo nuevo a `RequestMetric`.

---

## 1. Qué es y para qué sirve

El dato **siempre fue por-modelo**: cada fila de `RequestMetric` (una por
petición) ya trae `model: Option<String>` junto con sus tokens, coste y
latencia (ver `docs/telemetry-level-1.md` §3). Lo que faltaba era una forma de
preguntar, **en caliente**, "¿qué modelo conviene optimizar ahora mismo?" sin
tener que procesar el `telemetry.jsonl` a mano.

`GET /stats` responde exactamente eso: un snapshot en vivo, agregado por
`(proveedor, modelo)`, actualizado en el mismo instante en que cada fila se
escribe a disco. No es un análisis nuevo — es la misma medición, sumada.

```
gentle-ai ──▶ OxideGate ──▶ proveedor
                 │
                 ├──▶ telemetry.jsonl        (fila a fila, sin cambios)
                 └──▶ StatsRegistry (RAM)    (agregado por modelo, en vivo)
                            │
                            ▼
                      GET /stats  (snapshot JSON)
```

---

## 2. Cómo consultarlo

```sh
curl localhost:8899/stats
```

(ajustar el puerto al que use tu instancia — `8080` por defecto, `8899` si
está ocupado). Devuelve `200 OK` con `content-type: application/json`, sin
autenticación: el proxy bindea en `127.0.0.1` y el snapshot no expone
secretos ni prompts, solo agregados y conteos de huellas.

### Ejemplo de salida

```json
[
  {
    "upstream": "anthropic",
    "model": "claude-opus-4-1",
    "requests": 42,
    "input_tokens": 210000,
    "output_tokens": 18500,
    "cache_read_tokens": 165000,
    "cache_write_tokens": 12000,
    "avg_input_tokens": 5000.0,
    "avg_output_tokens": 440.48,
    "cache_hit_rate": 0.427,
    "cache_forced_rate": 0.9047619047619048,
    "cost_usd": 3.87,
    "avg_cost_usd": 0.0921,
    "avg_ttft_ms": 812.3,
    "min_ttft_ms": 340.1,
    "max_ttft_ms": 2100.7,
    "ttft_ms_sum": 34116.6,
    "ttft_ms_count": 42,
    "avg_total_ms": 4210.9,
    "total_ms_sum": 176857.8,
    "avg_tokens_per_sec": 38.6,
    "tokens_per_sec_sum": 1621.2,
    "tokens_per_sec_count": 42,
    "error_rate": 0.0,
    "errors": 0,
    "distinct_prompts": 9,
    "redundant_requests": 33,
    "redundancy_rate": 0.7857142857142857,
    "redundancy_saturated": false
  },
  {
    "upstream": "openai",
    "model": "gpt-4o-mini",
    "requests": 7,
    "input_tokens": 8400,
    "output_tokens": 1200,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0,
    "avg_input_tokens": 1200.0,
    "avg_output_tokens": 171.43,
    "cache_hit_rate": 0.0,
    "cache_forced_rate": 0.0,
    "cost_usd": 0.0031,
    "avg_cost_usd": 0.00044,
    "avg_ttft_ms": 0.0,
    "min_ttft_ms": null,
    "max_ttft_ms": null,
    "ttft_ms_sum": 0.0,
    "ttft_ms_count": 0,
    "avg_total_ms": 980.2,
    "total_ms_sum": 6861.4,
    "avg_tokens_per_sec": 0.0,
    "tokens_per_sec_sum": 0.0,
    "tokens_per_sec_count": 0,
    "error_rate": 0.14285714285714285,
    "errors": 1,
    "distinct_prompts": 7,
    "redundant_requests": 0,
    "redundancy_rate": 0.0,
    "redundancy_saturated": false
  }
]
```

Filas ordenadas por `requests` descendente: lo que más tráfico tiene aparece
primero. Un modelo sin `model` en el request (falló antes de leerlo, o el
body no lo traía) se agrupa bajo `"model": "unknown"` — no se pierde la fila,
sigue aportando señal de error/latencia al proveedor.

---

## 3. Qué señala cada métrica (para decidir qué optimizar)

| Señal | Qué mirar | Qué indica |
|---|---|---|
| `cache_hit_rate` bajo | cerca de `0.0` con tráfico repetido | El contexto **no** se está sirviendo desde caché. Candidato a forzar `cache_control` (palanca A del optimizador, ver `docs/optimizer-prompt-cache.md`) |
| `cache_forced_rate` | comparado contra `cache_hit_rate` | Si ya está en `1.0` pero `cache_hit_rate` sigue bajo, forzar el breakpoint no alcanza — el problema es otro (prompt cambia demasiado, TTL vencido, etc.) |
| `redundancy_rate` alto | cerca de `1.0` | Muchas peticiones repiten la misma huella de prompt. Candidato a deduplicación o a una capa de caché de respuesta (palanca B) |
| `redundancy_saturated: true` | — | El cap de huellas distintas por modelo (50.000) se llenó: `redundant_requests` es una **cota inferior**, no el valor exacto. Sigue siendo señal útil, pero no absoluta |
| `avg_input_tokens` alto frente a `avg_output_tokens` | proporción muy desbalanceada | Overhead de contexto: se manda mucho más de lo que se genera. Candidato a recortar system prompt, historial, o herramientas declaradas de más |
| `error_rate` > 0 | cualquier valor no-cero | Ese modelo/proveedor está fallando; conviene mirar el `telemetry.jsonl` crudo para esos `status` |
| `avg_cost_usd` entre modelos | comparar entre filas | Único denominador comparable entre proveedores (ver "la trampa del token" en `docs/telemetry-level-1.md` §4) — nunca comparar `avg_input_tokens` entre proveedores distintos |
| `avg_ttft_ms` / `avg_tokens_per_sec` | por modelo | Latencia percibida y velocidad de generación; útil para decidir si un modelo más rápido compensa un coste algo mayor |

---

## 4. Cómo se calcula (diseño interno)

- **`src/telemetry/stats.rs`** es puro: no conoce axum, solo `RequestMetric`.
  `StatsRegistry::ingest` actualiza en `O(1)` por request (sumas, contadores,
  min/max); `StatsRegistry::snapshot` deriva las tasas y promedios recién en
  el momento de leer, nunca antes.
- **`src/middleware/stats.rs`** es el único archivo que conoce axum en esta
  cadena: expone `GET /stats`, toma un read-lock breve sobre el registro
  compartido, y serializa el snapshot a JSON.
- El registro vive en `Arc<RwLock<StatsRegistry>>`, compartido entre la task
  de drenaje de telemetría (que escribe) y el handler de `/stats` (que lee).
  Ambos toman el lock de forma síncrona y lo sueltan **antes** de cualquier
  punto de suspensión (`.await`): nunca se sostiene un lock a través de I/O
  asíncrono.
- **Redundancia acotada en memoria.** Detectar duplicados exige recordar
  huellas de prompt ya vistas, pero ese mapa no puede crecer sin límite en un
  proceso de larga vida. Cada modelo tiene un cap de 50.000 huellas distintas
  (`MAX_DISTINCT_PROMPTS_PER_MODEL`); al llenarse, las huellas nuevas dejan de
  admitirse (las ya vistas se siguen contando) y se marca
  `redundancy_saturated: true`. El número de redundancia queda como **cota
  inferior honesta**, nunca como un valor inflado ni un riesgo de memoria sin
  techo.
- **El mapa de filas `(proveedor, modelo)` NO tiene cap.** El `model` sale del
  body del request, así que en teoría un cliente que varíe ese campo podría
  crecer el registro sin límite. En la práctica queda acotado: los IDs de
  modelo son un conjunto finito y pequeño (decenas, no millones), y el proxy
  bindea en `127.0.0.1` (mismo modelo de confianza que el resto: el operador no
  se ataca a sí mismo). Si algún día el proxy sirviera tráfico no confiable,
  este mapa necesitaría su propio cap.

---

## 5. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/stats.rs` | `StatsRegistry`, `ModelStatsRow`, `StatsSnapshot` — agregación pura, sin axum |
| `src/telemetry/logger.rs` | `TelemetrySink::spawn` alimenta el registro en la misma task que escribe el JSONL; `TelemetrySink::stats()` expone el `Arc<RwLock<StatsRegistry>>` |
| `src/middleware/stats.rs` | `handle_stats` — el handler HTTP de `GET /stats` |
| `src/main.rs` | Registra la ruta `/stats` en el `Router` |
