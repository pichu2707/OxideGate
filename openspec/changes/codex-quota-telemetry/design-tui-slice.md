# Diseño: Cuota Codex en el monitor TUI — Rebanada de superficie

## Resumen

Esta rebanada muestra el estado de cuota de suscripción de Codex (ya presente
en `/requests` desde la rebanada 1) en el monitor TUI, como un **panel
dedicado, toggleable con una tecla nueva**, alimentado por la fila más reciente
de `/requests` cuyo `codex_quota` sea no-nulo. Toca ÚNICAMENTE `src/bin/monitor.rs`
(+ sus structs de deserialización) y `docs/monitor-tui.md`. No toca el proxy,
los providers ni el pricing.

## Ajuste de plan (decidido, no se reabre)

La rebanada 2 originalmente propuesta (agregación de cuota en `/stats`) se
**omite**. La cuota es un **gauge a nivel de CUENTA**: es idéntica para todos
los modelos que llegan por el backend de Codex en un instante dado (es el
estado de la cuenta, no del modelo). Agregarla por modelo en `/stats` repetiría
el mismo número en cada fila sin aportar información. Por eso se va directo a la
superficie del monitor, leyendo la cuota cruda de `/requests` — la única fuente
que ya la expone.

## Decisión de arquitectura: forma de presentación

### Elección: panel dedicado toggleable con tecla nueva

Se agrega un **cuarto panel toggleable** (junto a requests `p` y tools `s`),
controlado por una tecla nueva, que renderiza el gauge de cuota de la fila
fuente más reciente.

### Alternativas consideradas

| Opción | Descripción | Por qué se descarta |
|--------|-------------|---------------------|
| (a) Tercera vista del ciclo `c` (Latency → Context → Quota) | Sumar `Quota` al enum `RequestsView` | **Error de categoría.** El ciclo `c` cambia QUÉ COLUMNAS de la tabla POR FILA se ven; las filas siguen siendo las mismas peticiones. La cuota no es una tabla por fila: es un gauge único de cuenta, sin filas. Meterla en `c` rompería el modelo mental de "mismas filas, otras columnas" que documenta `docs/monitor-tui.md §7.1`, y secuestraría el espacio del panel de requests. |
| (b) Línea de header/status persistente | Añadir la cuota al header de 3 líneas | El header describe el estado de CONEXIÓN del monitor (URL, último fetch, edad del baseline), no el estado de un proveedor. La cuota es específica de Codex: para la mayoría del tráfico (Anthropic, Gemini, OpenAI vía API key) no existe, y ocuparía espacio de header fijo con un "—" permanente. |
| (c) **Panel dedicado toggleable (elegida)** | Cuarto panel independiente, tecla nueva | Reusa EXACTAMENTE el precedente ya probado del panel de tools (`s`): panel independiente, alimentado por la fila más reciente de `/requests` que califica, con degradación a una sola línea explicativa cuando ninguna fila califica. La cuota encaja en ese patrón sin inventar nada nuevo. |

### Justificación

La opción (c) es la única que respeta los tres invariantes del monitor:

1. **Precedente estructural.** El panel de tools por servidor (`draw_tools_panel`,
   tecla `s`) ya es un panel toggleable independiente que: (i) busca la fila
   fuente más reciente que califica (`find_tools_source_row`), (ii) degrada con
   gracia a una línea explicativa cuando ninguna califica, (iii) reserva su
   espacio de layout SOLO cuando está visible. La cuota replica ese patrón
   campo por campo — el mismo `find_*_source_row`, la misma degradación, la
   misma independencia de `p`/`c`/`s`.

2. **Responsabilidad única.** La cuota es su propio dato (estado de cuenta), en
   su propio panel, con su propia tecla. No contamina el ciclo `c` (que es de
   columnas por fila) ni el header (que es de conexión).

3. **Coste de contexto acotado.** El layout ya empuja constraints
   condicionalmente por panel visible (`ui`, líneas ~1517-1523): agregar un
   panel toggleable más es un `if app.show_quota_panel { constraints.push(...) }`
   y un `idx += 1`, sin lógica especial por caso — el patrón del `idx` que
   avanza ya está documentado como preparado para "un tercer panel toggleable".

### Tecla

`u` (mnemónico: **u**so de cuota). Teclas ya tomadas: `q`/`Esc` (salir), `b`
(baseline), `r` (reset), `↑`/`↓` (selección), `p` (requests), `c` (ciclo de
vista), `s` (tools). `q` (quota/quit) y `c` (cuota/cycle) ya están ocupadas por
su inicial, por eso se elige `u`. La tecla se registra en el `match key.code`
del loop principal (línea ~1464) y se lista en el footer y en la tabla de
teclas de `docs/monitor-tui.md §4`.

### Visibilidad por defecto: visible

`show_quota_panel: true` por defecto, igual que `show_requests_panel` y
`show_tools_panel`. Razones: (i) consistencia con los otros dos paneles reduce
carga cognitiva; (ii) el punto de la rebanada es SUPERFICIAR la cuota — un panel
oculto por defecto arriesga que la función nunca se descubra; (iii) la
degradación a una sola línea honesta es autoexplicativa para quien no usa Codex;
(iv) el footer ya enseña la tecla para ocultarlo. El panel solo reserva espacio
de layout cuando está visible, así que quien no lo quiera lo apaga con `u` una
vez.

## Fuente de datos

### Punto de lookup exacto

La cuota NO requiere ningún cambio en el ciclo de poll/parse. Se lee de
`app.recent_requests` en el momento del DRAW, exactamente como hace el panel de
tools. El snapshot de `/requests` ya vive en `App::recent_requests` (poblado por
`poll_requests`, línea ~1343); el panel de cuota solo lo consulta al dibujar.

Función pura nueva, hermana de `find_tools_source_row` (línea ~982):

    /// Fila MÁS RECIENTE de `rows` cuyo `codex_quota` sea `Some`. `rows` llega
    /// en orden cronológico (más viejo primero), así que se recorre desde el
    /// final. `None` si ninguna fila trae cuota: todo el tráfico del buffer es
    /// no-Codex (Anthropic, Gemini, OpenAI vía API key) o el proxy es anterior
    /// a la rebanada 1.
    fn find_quota_source_row(rows: &[RequestRow]) -> Option<&RequestRow>

Implementación: `rows.iter().rev().find(|r| r.codex_quota.is_some())`. A
diferencia del panel de tools (que descarta `Some(vec![])` como "cero real"),
acá `Some(_)` SIEMPRE califica: un `CodexQuota` presente ES el estado de cuota,
no hay un análogo del vector vacío.

Se llama en `draw_quota_panel` (TUI) y en `print_quota_table` (`--once`), igual
que `find_tools_source_row` se llama en `draw_tools_panel` y `print_tools_table`.

### Struct de deserialización

`RequestRow` (línea ~310) gana un campo:

    codex_quota: Option<CodexQuotaRow>,

Y se define un struct espejo local `CodexQuotaRow` con los 12 campos, mismo
patrón que `ToolServerRow` espeja `provider::ToolServerBytes`. El monitor es un
binario INDEPENDIENTE (no hay `lib.rs`), así que no puede importar
`telemetry::CodexQuota`; define su propio espejo con `#[derive(Deserialize)]`.

**Sin riesgo de `deny_unknown_fields`:** el monitor NO usa ese atributo en
ningún struct (verificado: `RequestRow` ya tolera campos desconocidos hoy —
serde ignora las claves extra por defecto). Y como `codex_quota` es `Option`,
un proxy anterior a la rebanada 1 que no manda la clave deja el campo en `None`
sin atributos adicionales — mismo contrato que `prepare_us` y `tools_by_server`.

Los 12 campos del espejo se documentan como un bloque conciso que apunta a
`RecentRequest::codex_quota`/`telemetry::CodexQuota` para el contrato completo
(mismo recurso que ya usa `RequestRow` para el bloque de contexto: un comentario
de bloque con puntero, NO 12 párrafos de doc), para no inflar el presupuesto de
líneas con la convención de documentación total sobre 12 campos crudos:

| Campo espejo | Tipo |
|--------------|------|
| `plan_type`, `active_limit`, `credits_balance` | `Option<String>` |
| `primary_used_percent`, `secondary_used_percent`, `primary_window_minutes`, `secondary_window_minutes`, `primary_reset_after_seconds` | `Option<u64>` |
| `primary_reset_at`, `secondary_reset_at` | `Option<i64>` |
| `credits_has_credits`, `credits_unlimited` | `Option<bool>` |

## Cómo se obtiene "ahora" para el countdown

El monitor ya depende de `chrono` (lo usa en `format_time`, línea ~1957:
`chrono::DateTime::parse_from_rfc3339`). El "ahora" de wall-clock para el
countdown se obtiene con:

    chrono::Utc::now().timestamp()   // -> i64, segundos unix

No hace falta ningún tick nuevo: el loop redibuja cada ~250ms (el
`event::poll(Duration::from_millis(250))` de la línea ~1457), así que el
countdown se recalcula en cada draw y avanza en vivo sin maquinaria extra. El
countdown es una función del estado (`reset_at` de la fila fuente) y del reloj
(`Utc::now()`), nunca se cachea.

## Qué se renderiza — spec campo a campo

El panel es un `Paragraph` con borde (mismo widget base que `draw_before_after`),
no una `Table`: la cuota es un gauge de líneas, no filas tabulares. Título:
`" cuota codex · fuente HH:MM:SS <modelo> "` con `format_time(&source.timestamp)`
y `source.model`, igual que el título del panel de tools indica su fila fuente.

**Regla de honestidad transversal (no negociable):** todo campo ausente se
renderiza como `—` o se OCULTA por completo; NUNCA se fabrica un `0%`, un `0` ni
un valor por defecto. Es el mismo criterio "ausente ≠ cero" que ya gobierna todo
el monitor (`opt_bytes`, `opt_fixed`, `deferred_cell`).

| Elemento | Fuente | Regla de render |
|----------|--------|-----------------|
| Plan y límite | `plan_type`, `active_limit` | `"plan: <plan_type> · límite: <active_limit>"`. Cada campo ausente → `—`. |
| Ventana primaria | `primary_used_percent`, `primary_window_minutes` | `"primaria: [barra] <n>% · ventana <window>"`. Si `primary_used_percent` es `None`: `—` sin barra. La barra se construye con un helper de texto (bloques `█` llenos + `·` vacíos, ancho fijo ~12-16 celdas), porcentaje clamp a `0..=100`. |
| Ventana secundaria | `secondary_used_percent`, `secondary_window_minutes` | **Se renderiza SOLO si `secondary_window_minutes` es `Some(m)` con `m > 0`.** En esta cuenta la ventana secundaria llega vacía/ausente (`0` o `None`), así que la línea se OMITE por completo — no se muestra `—`, directamente no aparece. |
| Countdown de reset | `primary_reset_at` (preferido), fallback `primary_reset_after_seconds` + `timestamp` de la fila | `"resetea en 6d 8h"` (formato humano). Ver detalle abajo. |
| Créditos | `credits_has_credits`, `credits_balance`, `credits_unlimited` | Se renderiza SOLO si `credits_has_credits == Some(true)`. Si `credits_unlimited == Some(true)`: `"créditos: ilimitados"`. Si no: `"créditos: <credits_balance>"` (o `—` si el balance falta). Si `credits_has_credits` es `None`/`Some(false)`: se OMITE la línea. |

### Countdown de reset (detalle)

Prioridad de fuente, todas honestas:

1. **`primary_reset_at` presente** (timestamp unix absoluto): `remaining =
   primary_reset_at - Utc::now().timestamp()`. Es la fuente preferida porque es
   absoluta y no envejece con el buffer.
2. **`primary_reset_at` ausente, `primary_reset_after_seconds` presente:** el
   valor es relativo AL INSTANTE DE CAPTURA, no a ahora. Pero la fila fuente
   trae su propio `timestamp` (RFC 3339), así que se reconstruye el instante
   absoluto de reset: `reset_instant = parse(timestamp) + reset_after_seconds`,
   y `remaining = reset_instant - Utc::now().timestamp()`. Sigue siendo honesto
   y no arrastra el drift de mostrar un "faltan X" congelado.
3. **Ambos ausentes:** `—`, sin countdown fabricado.

Formato humano: `remaining <= 0` → `"resetea ahora"`; si no, se descompone en
`d`/`h`/`m` mostrando las dos unidades más significativas (`"6d 8h"`, `"3h 12m"`,
`"45m"`). Helper puro y testeable sin terminal.

### Nota de redondeo de enteros

`used_percent` llega como entero (`4`, `0`). A nivel de gauge esto es correcto:
ES el estado de la cuenta en ese instante, no un delta derivado. Se renderiza
tal cual (`"4%"`, `"0%"`). La salvedad de redondeo del delta marginal
(propuesta, rebanada 5) NO aplica acá: este panel muestra el estado acumulado,
no la diferencia entre filas.

### Degradación sin tráfico Codex

Cuando `find_quota_source_row` devuelve `None` (todo el buffer es no-Codex, o
proxy anterior a la rebanada 1), el panel muestra una única línea explicativa
dentro de su borde, NUNCA una caja vacía ni un gauge al 0%:

    "sin datos de cuota (ninguna petición reciente usó el backend de Codex, o
     el proxy es anterior a la captura de cuota)"

Es el mismo patrón exacto que `draw_tools_panel` para el caso sin fuente
(líneas ~1764-1775).

## Modo headless (`--once`)

`run_once` (línea ~155) gana una llamada a `print_quota_table(&rows)` después de
`print_tools_table`, mismo pipeline puro (`find_quota_source_row` + los mismos
formatters). Sin sesión interactiva no hay nada especial que ocultar: imprime el
gauge en texto plano o la línea de "sin datos de cuota". El countdown usa el
mismo `Utc::now()`.

## Flujo de datos

    GET /requests ──▶ App::recent_requests (poll_requests, sin cambios)
                              │  (en el draw)
                              ▼
                 find_quota_source_row(&recent_requests)
                              │  Some(&RequestRow con codex_quota)
                              ▼
              draw_quota_panel / print_quota_table
                 ├─ plan/límite ────────── plan_type, active_limit
                 ├─ barra primaria ─────── primary_used_percent (+ window)
                 ├─ barra secundaria ───── SOLO si secondary_window_minutes>0
                 ├─ countdown ──────────── primary_reset_at | (timestamp + after_seconds)
                 │                          menos Utc::now().timestamp()
                 └─ créditos ───────────── SOLO si credits_has_credits==Some(true)

## Cambios de archivos

| Archivo | Acción | Descripción |
|---------|--------|-------------|
| `src/bin/monitor.rs` | Modificar | Campo `codex_quota` en `RequestRow` + struct espejo `CodexQuotaRow`; `find_quota_source_row` (pura); helpers de barra, countdown humano y líneas de cuota; `draw_quota_panel`; `print_quota_table`; campo `show_quota_panel` + `toggle_quota_panel` en `App`; tecla `u` en el `match`; constraint condicional en `ui`; línea del footer; llamada en `run_once` |
| `docs/monitor-tui.md` | Modificar | Sección nueva del panel de cuota (fuente, columnas/líneas, degradación, countdown, redondeo); fila `u` en la tabla de teclas §4; ítem en el layout §5; nota en el footer |

## Invariante de responsabilidad única (garantía estructural)

- El panel de cuota SOLO lee `codex_quota` de la fila fuente. No toca el ciclo
  `c` (columnas por fila), ni el header (conexión), ni el panel de tools.
- No hay ninguna función que combine cuota con `cost_estimate_usd`, con bytes de
  contexto ni con throughput. La cuota es porcentaje de ventana, jamás dólares —
  el panel no tiene ningún campo en USD que pudiera confundirse.
- El monitor no deriva nada de la cuota: la muestra cruda. La atribución
  marginal (delta entre filas) es una rebanada posterior y vive en otra capa.

## Estrategia de pruebas

| Capa | Qué | Cómo |
|------|-----|------|
| Unit | `find_quota_source_row`: elige la fila más reciente con `codex_quota` Some; `None` si todas son no-Codex; ignora filas `None` intercaladas más nuevas | `Vec<RequestRow>` sintéticos, mismo patrón que los tests de `find_tools_source_row` |
| Unit | Countdown humano: `<=0` → "resetea ahora"; descomposición d/h/m; fallback vía `timestamp + reset_after_seconds` | función pura, valores fijos |
| Unit | Barra de texto: clamp `0..=100`, ancho fijo, llenos/vacíos correctos | función pura |
| Unit | Deserialización: `RequestRow` con `codex_quota` presente y con la clave ausente (proxy viejo → `None`) sobrevive el parseo sin `deny_unknown_fields` | `serde_json::from_str` sobre JSON sintético |

## Presupuesto de revisión (400 líneas)

Estimación:

| Bloque | Líneas aprox. |
|--------|---------------|
| Struct espejo `CodexQuotaRow` (12 campos + doc de bloque con puntero) | 30-45 |
| Campo `codex_quota` en `RequestRow` + doc | 5-8 |
| `find_quota_source_row` + doc | 10-12 |
| Helpers (barra, countdown humano, builder de líneas) + doc | 40-60 |
| `draw_quota_panel` + doc | 55-75 |
| `print_quota_table` (`--once`) + doc | 30-40 |
| `App`: campo + toggle + `new()` + tecla + constraint + footer | 15-20 |
| Tests | 60-90 |
| `docs/monitor-tui.md` (sección + tabla teclas + layout + footer) | 45-65 |
| **Total** | **~290-415** |

**Dentro del presupuesto, pero AJUSTADO.** El riesgo está en la convención de
documentación total sobre los 12 campos del espejo y en la densidad de los
tests. Mitigación ya prevista: documentar los 12 campos del espejo como un
bloque conciso con puntero a `RecentRequest::codex_quota` (igual que `RequestRow`
ya hace con el bloque de contexto), NO 12 párrafos. Si aun así se pasa de 400,
la palanca es recortar `print_quota_table` de `--once` a una rebanada de
seguimiento (el valor central es la TUI interactiva), no la documentación del
contrato por campo.

## No-objetivos de esta rebanada

- Sin coste nocional (comparación a precios de API GPT-5): rebanada posterior,
  requiere tocar `pricing.rs`.
- Sin atribución marginal por petición (deltas de `used_percent` entre filas):
  rebanada posterior, con su salvedad de redondeo de enteros.
- Sin cambios en `/stats` (la agregación por modelo se descartó por redundante).
- Sin cambios en el proxy, los providers ni el pricing.

## Preguntas abiertas

- [ ] Confirmar la tecla `u` con el usuario (alternativa neutral: `k`). No
  bloquea el diseño; es una elección de mnemónico.
- [ ] Ancho exacto de la barra de texto (12 vs 16 celdas): a calibrar contra el
  ancho real del panel en el layout; no bloquea.
