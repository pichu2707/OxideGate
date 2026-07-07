# Benchmark — Caracterización de proveedores por tamaño de input

> Herramienta: `src/bin/bench.rs`. Mide cómo escalan TTFT, latencia y coste con
> el tamaño del INPUT, a igualdad de condiciones, comparando proveedores.

---

## 1. Qué es y por qué

La telemetría (nivel 1) mide el **uso real** que pasa por el proxy. El benchmark
es lo complementario: una **barrida controlada** que dispara peticiones de tamaño
creciente a cada proveedor, en las MISMAS condiciones, para poder comparar
manzanas con manzanas:

> *"A 20k tokens de input, ¿quién arranca más rápido y quién es más barato?"*

Un proxy mide lo que fluye; el benchmark **fabrica** tráfico controlado para
caracterizar. Los dos se complementan.

## 2. Metodología

- **Aísla el efecto del INPUT**: output fijo y chico (`max_tokens` ≈ 8-16) para
  que la generación no contamine la medición. La velocidad de *generación*
  (output largo) es otra barrida, aparte.
- **A través de OxideGate**: el benchmark pega al proxy (`localhost:8899`), así
  es el propio OxideGate quien mide — misma pipeline que en producción.
- **Tamaños barridos** (bytes de relleno): `0, 1k, 5k, 20k, 50k`.
- **Relleno neutro** (lorem ipsum) con un **prefijo único por corrida** para
  reventar la caché sin alterar el byte-count del bucket.
- **N repeticiones** por tamaño (default 3), se promedia.
- **Thinking desactivado** en modelos que lo soportan (Gemini 2.5+): ver gotcha
  abajo.

El analizador agrupa por `(proveedor, prompt_bytes)` — el byte-count es la
variable **controlada**, estable entre repeticiones.

## 3. Cómo correrlo

Las keys se leen de un `.env` en la raíz (cargado con `dotenvy`). **`.env` está
en `.gitignore` — nunca se sube.**

```
# .env
API_KEY_GEMINI=AIza...
API_KEY_OPENAI=sk-...
GEMINI_MODEL=gemini-2.5-flash
```

Con OxideGate corriendo:

```bash
OXIDEGATE_PORT=8899 cargo run --bin bench
```

Variables (todas opcionales salvo al menos una key):

| Variable | Default | Qué hace |
|---|---|---|
| `API_KEY_GEMINI` | — | Habilita la barrida de Gemini |
| `API_KEY_OPENAI` | — | Habilita la barrida de OpenAI |
| `OXIDEGATE_PORT` | `8899` | Puerto de OxideGate |
| `BENCH_REPEATS` | `3` | Repeticiones por tamaño |
| `GEMINI_MODEL` | `gemini-2.0-flash` | Modelo Gemini |
| `OPENAI_MODEL` | `gpt-4o-mini` | Modelo OpenAI |

El harness dispara la barrida, espera el flush de telemetría, y lee
`~/.config/oxidegate/telemetry.jsonl` para imprimir la tabla comparativa.

## 4. Anthropic (Claude Max) — a mano

El benchmark automático solo cubre proveedores con **API key** (Gemini, OpenAI).
Anthropic con **Claude Max** usa OAuth de suscripción, que no se scriptea con
key. Se alimenta con tráfico real vía `claude -p` (headless) redirigido — una
petición por invocación, uso legítimo de Claude Code por el proxy:

```bash
export ANTHROPIC_BASE_URL=http://localhost:8899
for size in 0 1000 5000 20000 50000; do
  for run in 1 2 3; do
    filler=$(yes 'lorem ipsum dolor sit amet consectetur adipiscing elit ' \
      | tr -d '\n' | head -c "$size")
    claude -p "$run-$RANDOM Responde únicamente con: ok. $filler" >/dev/null 2>&1
    sleep 2
  done
done
```

Sus filas caen en la misma telemetría; el analizador las agrupa por
`input_tokens` medidos. **Caveats**: Claude Code añade su system prompt (el piso
de tokens sube), el *prompt caching* puede deflactar repeticiones (variá el
relleno), y correr esto junto a otra sesión de Max puede dar `429` por
concurrencia (se filtran por `status:200`).

## 5. Primeros hallazgos

`gemini-2.5-flash` (thinking off) vs `gpt-4o-mini`, barrida de input, output
chico, n=3:

| Proveedor | prompt_bytes | input_tokens | ttft_ms | total_ms | cost_usd |
|---|---|---|---|---|---|
| gemini | 169 | 18 | 397.8 | 398.4 | 0.000008 |
| gemini | 1 169 | 166 | 333.4 | 334.1 | 0.000052 |
| gemini | 5 169 | 747 | 406.6 | 407.1 | 0.000227 |
| gemini | 20 169 | 2 930 | 417.7 | 418.8 | 0.000881 |
| gemini | 50 169 | 7 292 | 571.6 | 572.2 | 0.002190 |
| openai | 122 | 20 | 320.8 | 827.7 | 0.000004 |
| openai | 1 122 | 168 | 296.2 | 752.4 | 0.000026 |
| openai | 5 122 | 749 | 299.1 | 944.7 | 0.000114 |
| openai | 20 122 | 2 932 | 260.8 | 846.9 | 0.000441 |
| openai | 50 122 | 7 294 | 413.2 | 1 219.2 | 0.001095 |

Lectura:

1. **TTFT casi insensible al tamaño del input** (los dos): de 18 a 7 292 tokens,
   apenas se mueve. El prefill es barato; el input no es el driver de la latencia
   en este rango (se nota recién a ~7k tokens).
2. **Gemini termina mucho más rápido de punta a punta**: `total ≈ ttft` (manda
   todo, usage incluido, casi en un tiro). OpenAI Responses arrastra 500-800 ms
   extra para cerrar el stream, aun con output igual de chico.
3. **`gpt-4o-mini` ≈ 2× más barato que `gemini-2.5-flash` por token de input**
   (a 7 292 tok: $0.00110 vs $0.00219). Para cargas input-heavy y sensibles a
   coste, OpenAI gana acá.
4. **Los tokenizadores casi coincidieron** (18≈20, 7292≈7294) — casualidad de
   este relleno (lorem ipsum), NO una regla; con texto real divergen.

**Caveats de esta corrida**: precios son placeholders editables (el 2× depende
de ellos); output chico a propósito; n=3 (ruido en TTFT); falta Anthropic.

## 6. Gotchas (aprendidos en vivo)

- **Modelos de "thinking" (Gemini 2.5+) devuelven cuerpo VACÍO** si el
  `maxOutputTokens` es chico: gastan el presupuesto pensando y no queda para el
  output (ni `usageMetadata`). Fix: `generationConfig.thinkingConfig.thinkingBudget = 0`.
  *(Medir CON thinking es una caracterización aparte, muy valiosa — se conecta
  con `thoughtsTokenCount`.)*
- **`gemini-2.0-flash` da 404** en algunas keys: usar `GEMINI_MODEL` con uno
  disponible (`gemini-2.5-flash`). Los `gemini-3.x` del CLI son del backend de
  Code Assist, no de la API pública.
- **No borres `telemetry.jsonl` con OxideGate corriendo**: el server abre el
  archivo una sola vez al arrancar; si lo borrás, sigue escribiendo al inodo
  fantasma y las nuevas filas se pierden. El harness usa un offset de filas, así
  que no hace falta borrar nada.

## 7. Pendiente

- **Segunda barrida: output largo** (throughput de generación + coste de salida).
- **Verificar precios reales** por modelo (hoy son defaults editables).
- **Anthropic** integrado en la comparativa (vía el loop de la sección 4).
- Endurecer OxideGate para reabrir el archivo de telemetría si se rota/borra.
