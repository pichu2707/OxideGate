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
OXIDEGATE_PORT=8899 cargo run --bin oxidegate-bench
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

## 4. Anthropic / Claude Code — por qué NO entra en la barrida limpia

El benchmark automático solo cubre proveedores con **API key** (Gemini, OpenAI).
Anthropic con **Claude Max** usa OAuth de suscripción, que no se scriptea con
key; la única vía es tráfico real vía `claude -p` (headless) redirigido por el
proxy. Lo probamos, y el resultado dejó claro que **no sirve como barrida
comparable** — pero reveló algo más importante (ver §5.1).

Una sola llamada `ANTHROPIC_BASE_URL=http://localhost:8899 claude -p "Responde
solo: ok"` produjo:

```
model=claude-opus-4-8   prompt_bytes=166 328   input_tokens=7 368   output=4
```

Es decir: un prompt de 3 palabras se volvió un request de **166 KB / 7 368
tokens** sobre **opus**. Claude Code inyecta su system prompt + tools + contexto
en CADA petición. Consecuencias para el benchmark:

- **Piso de ~7 000 tokens**: Anthropic nunca alcanza el extremo chico del barrido
  (18-750 tokens de Gemini/OpenAI). No es apples-to-apples.
- **Modelo distinto de tier**: usa `opus` (buque insignia), no un modelo barato
  como `gpt-4o-mini` / `gemini-flash`.

Por eso la comparativa de proveedores se cierra con **Gemini + OpenAI** (misma
metodología, tiers comparables). Lo que Claude Code SÍ nos da es otra medición,
igual de valiosa: el **coste real de usar Claude Code** (§5.1).

> Nota operativa: un `claude -p` suelto anda bien, pero un **loop** de muchos
> (15 procesos Claude Code seguidos) tumba la terminal por acumulación de
> recursos. Si se quiere una serie, hacerla de a poco y con `sleep` amplio.

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
de ellos); output chico a propósito; n=3 (ruido en TTFT); Anthropic queda fuera
a propósito (ver §4 y §5.1).

### 5.1 El coste real de Claude Code (harness overhead)

El intento de medir Anthropic reveló el insight central del proyecto. La MISMA
tarea trivial ("Responde ok"), medida de dos formas:

| Vía | Modelo | input_tok | TTFT | Coste aprox. |
|---|---|---|---|---|
| Claude Code (`claude -p`) | opus-4-8 | 7 368 | 4 634 ms | **~$0.111** |
| API cruda | gpt-4o-mini | 20 | 321 ms | **~$0.000005** |

La misma tarea cuesta **~20 000× más** por Claude Code. Y NO es (solo) por el
modelo: es el **overhead del harness** — 7 368 tokens de system prompt + tools +
contexto que el agente arrastra en CADA llamada, más el uso de `opus`.

> **La conclusión que importa para optimizar coste de agentes:** lo que domina
> el coste no es el prompt del usuario, es el **contexto que el agente arrastra
> por llamada**. Optimizar ahí (contexto más chico, modelo más barato para
> tareas simples, caché) rinde mucho más que optimizar el prompt.

## 6. Gotchas (aprendidos en vivo)

- **Modelos de "thinking" (Gemini 2.5+) devuelven cuerpo VACÍO** si el
  `maxOutputTokens` es chico: gastan el presupuesto pensando y no queda para el
  output (ni `usageMetadata`). Fix: `generationConfig.thinkingConfig.thinkingBudget = 0`.
  *(Medir CON thinking es una caracterización aparte, muy valiosa — se conecta
  con `thoughtsTokenCount`.)*
- **`gemini-2.0-flash` da 404** en algunas keys: usar `GEMINI_MODEL` con uno
  disponible (`gemini-2.5-flash`). Los `gemini-3.x` del CLI son del backend de
  Code Assist, no de la API pública.
- **No borrar `telemetry.jsonl` con OxideGate corriendo**: el server abre el
  archivo una sola vez al arrancar; si se borra, sigue escribiendo al inodo
  fantasma y las nuevas filas se pierden. El harness usa un offset de filas, así
  que no hace falta borrar nada.

## 7. Pendiente

- **Segunda barrida: output largo** (throughput de generación + coste de salida).
- **Verificar precios reales** por modelo (hoy son defaults editables).
- **Anthropic vía API key** (no Max/OAuth): recién ahí entra en la barrida limpia
  y comparable con Gemini/OpenAI. El track de "coste real de Claude Code" (§5.1)
  es aparte y complementario.
- Endurecer OxideGate para reabrir el archivo de telemetría si se rota/borra.
