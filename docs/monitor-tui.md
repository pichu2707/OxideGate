# Monitor TUI — Velocidad por modelo, en vivo, con ANTES/DESPUÉS

> Herramienta: `src/bin/monitor.rs`. Cliente de terminal que consume
> `GET /stats` y muestra, en tiempo real, el efecto de una optimización
> (p. ej. forzar `cache_control`) sobre el throughput y la latencia por
> modelo.

---

## 1. Qué es y qué NO es

Es un **cliente HTTP separado del proxy**: pollea `GET /stats` cada ~1
segundo, igual que haría `curl` en loop, y pinta lo que recibe. No lee
`telemetry.jsonl`, no conoce el acumulador interno (`src/telemetry/stats.rs`)
más allá del contrato JSON del endpoint, y no toca la captura de métricas.
Se puede matar y volver a levantar sin afectar al proxy — es un observador,
no una dependencia.

Requisito: **OxideGate tiene que estar corriendo** con `/stats` disponible
(ver `docs/telemetry-by-model.md`). Si no lo está, el monitor no crashea: lo
avisa en pantalla y sigue reintentando cada poll.

## 2. Cómo se lanza

```bash
cargo run --bin monitor
```

O ya compilado:

```bash
cargo build --release --bin monitor
./target/release/monitor
```

### URL del endpoint

En orden de prioridad:

1. flag `--url <url>`
2. env `OXIDEGATE_STATS_URL`
3. `http://127.0.0.1:{OXIDEGATE_PORT}/stats` (`OXIDEGATE_PORT` default `8080`,
   el mismo default que usa el proxy — ver `src/config.rs`)

```bash
# proxy en el puerto default
cargo run --bin monitor

# proxy en otro puerto
OXIDEGATE_PORT=8899 cargo run --bin monitor

# URL explícita
cargo run --bin monitor -- --url http://127.0.0.1:8899/stats
```

### Modo headless: `--once`

```bash
cargo run --bin monitor -- --once
```

Hace UN solo fetch de `/stats`, imprime una tabla de texto plano (sin raw
mode, sin pantalla alternada) y sale con código `0`. Sirve para scripts,
CI, o para chequear rápido sin entrar a la TUI. Igual que el modo
interactivo, si el proxy está caído no crashea: imprime `proxy no
disponible en {url}` y sale limpio.

## 3. El flujo ANTES/DESPUÉS (el punto central de la herramienta)

La pregunta que responde: *"¿la palanca que acabo de prender realmente
mejoró la velocidad/latencia de ESTE modelo?"* Sin esto, tendrías que
comparar promedios acumulados desde el arranque del proxy — contaminados por
todo el tráfico previo.

Pasos:

1. Levantá el proxy con la optimización **apagada** (p. ej. `force_cache`
   off — ver `docs/optimizer-prompt-cache.md`).
2. Generá algo de tráfico normal para ese modelo.
3. En el monitor, elegí el modelo con `↑`/`↓` y apretá **`b`** para marcar el
   baseline (contadores crudos acumulados en ese instante).
4. Prendé la optimización (p. ej. `OXIDEGATE_FORCE_CACHE=true` y reiniciá el
   proxy, o el mecanismo que corresponda).
5. Seguí generando tráfico. El panel **ANTES/DESPUÉS** muestra el delta
   desde el baseline: throughput de la ventana (tok/s), TTFT de la ventana,
   cache-hit de la ventana, Δcoste, Δrequests, Δoutput_tokens y error% de la
   ventana — todo calculado sobre lo que pasó **después** de marcar `b`, no
   sobre el histórico completo.
6. `r` resetea el baseline si querés volver a arrancar la medición.

### Por qué no se promedian dos promedios

`/stats` ya expone `avg_ttft_ms` y `avg_tokens_per_sec`, pero promediar el
valor viejo y el nuevo sería matemáticamente incorrecto: el número de
requests que aportó a cada promedio pudo cambiar entre polls. Por eso el
snapshot ahora expone también las **sumas y counts crudos** (`ttft_ms_sum`,
`ttft_ms_count`, `total_ms_sum`, `tokens_per_sec_sum`,
`tokens_per_sec_count`, `errors` — ver §5) y el monitor calcula el promedio
de la ventana como `Δsuma / Δcount`, que sí es correcto.

## 4. Teclas

| Tecla | Acción |
|---|---|
| `q` / `Esc` | Salir |
| `b` | Marcar baseline (para el panel ANTES/DESPUÉS) |
| `r` | Resetear baseline |
| `↑` / `↓` | Elegir el modelo (fila resaltada, afecta el panel ANTES/DESPUÉS y los sparklines) |

## 5. Layout de la pantalla

1. **Header**: título, URL del endpoint, estado del último fetch ("ok · N
   modelos" o "proxy no disponible en..."), y edad del baseline ("baseline
   hace 12s" o "sin baseline — apretá 'b'").
2. **Tabla principal**, una fila por `(upstream, model)`, TOTAL acumulado
   desde que el proxy arrancó: `MODELO | REQ | tok/s | TTFT ms | cache-hit |
   coste $ | redun%`. Fila seleccionada resaltada.
3. **Panel ANTES/DESPUÉS**: delta de ventana del modelo seleccionado desde
   el baseline (ver §3). Si no hay baseline, muestra el aviso para marcarlo.
4. **Sparklines**: throughput (tok/s) y TTFT (ms) del modelo seleccionado a
   lo largo del tiempo, últimas ~120 muestras (~2 minutos a 1 poll/seg).
5. **Footer**: recordatorio de teclas.

## 6. Enhance del snapshot (`ModelStatsRow`)

Cambio aditivo y retrocompatible en `src/telemetry/stats.rs`: `ModelStatsRow`
ahora expone, además de los promedios que ya tenía, las sumas/counts crudas
que los originan:

| Campo nuevo | Qué es |
|---|---|
| `ttft_ms_sum` | Suma cruda de `ttft_ms` acumulada |
| `ttft_ms_count` | Cantidad de requests que aportaron TTFT (puede ser < `requests`) |
| `total_ms_sum` | Suma cruda de `total_ms` (count == `requests`) |
| `tokens_per_sec_sum` | Suma cruda de `tokens_per_sec` |
| `tokens_per_sec_count` | Cantidad de requests que aportaron `tokens_per_sec` |
| `errors` | Cantidad cruda de requests con `status >= 400` |

Ningún campo existente cambió de significado ni de tipo; cualquier
consumidor de `/stats` que ya exista sigue funcionando igual.

## 7. Dónde vive cada cosa

| Archivo | Responsabilidad |
|---|---|
| `src/telemetry/stats.rs` | `ModelStatsRow` con sumas/counts crudas (además de promedios) — sin cambios de comportamiento, solo más campos expuestos |
| `src/bin/monitor.rs` | Binario TUI independiente: fetch por HTTP, estado (baseline, historial, selección), cálculo de delta de ventana (funciones puras testeadas aparte) y render con `ratatui` |
| `docs/telemetry-by-model.md` | Contrato del endpoint `GET /stats` que este monitor consume |
