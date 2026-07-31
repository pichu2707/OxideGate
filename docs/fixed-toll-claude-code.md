# El peaje fijo de una sesión — 69.613 B antes de escribir nada

> Medido el 2026-07-31 sobre Claude Code 2.1.220, en una máquina concreta.
> **Coste cero**: captura del cuerpo con un servidor local que nunca llama al
> proveedor. Los números son de esa máquina; el método es reproducible en
> cualquier otra y dará otros números.

---

## 1. Qué se midió

Cuánto texto inyecta el harness **antes de que el usuario escriba una sola
palabra**. No cuánto ocupa en disco: cuánto viaja en el cuerpo de la petición.

El prompt de la sonda fue `"Responde solo: ok"` — 42 B. Todo lo demás es peaje.

| Bloque | Bytes | % | Dónde viaja |
|---|---:|---:|---|
| `CLAUDE.md` | 33.718 | 48% | `messages[0]`, como `<system-reminder>` con cabecera `# claudeMd` |
| Salida de hooks (`SessionStart`) | 19.918 | 29% | `messages[1]`, `role: "system"` |
| Listado de skills | 15.977 | 23% | `messages[1]`, al final del mismo bloque |
| **TOTAL** | **69.613** | | |

Para escala: el cuerpo completo de esa petición fue 183.861 B, de los cuales
103.136 B eran esquemas de `tools`.

---

## 2. Dónde cae cada cosa, y por qué eso decide el precio

`CLAUDE.md` viaja en `messages[0]`; la salida de hooks y las skills, en
`messages[1]`. Como `history = messages[:-1]`, **en cuanto la conversación
tiene tres mensajes los dos bloques están en `history`** — el prefijo estable,
que se cachea.

Consecuencia: el peaje se paga **a tarifa plena una vez por sesión** (más el
125% de escritura de caché), y **al 10% en cada turno siguiente**.

> **Aviso de método.** En la sonda `claude -p` solo hay dos mensajes, así que
> `messages[1]` sale en `last_turn` y parece no cacheado. Es un artefacto de
> medir un solo turno. La conclusión de arriba se deduce de la definición de
> los cubos más el hecho de que `SessionStart` dispara una vez;
> **no está verificada empíricamente en multi-turno**.

Lo que la caché **no** cambia: la ventana de contexto se ocupa igual, y los
bytes suben por el cable igual. Cacheado o no, esos 69,6 kB están ahí.

---

## 3. El listado de skills: 66 entradas, 242 B cada una

Se paga la línea `description` del frontmatter de **todas** las skills
instaladas, se invoquen o no, en cada sesión.

| Grupo | Skills | Bytes | % del listado |
|---|---:|---:|---:|
| plugin `vercel` | 33 | 8.852 | 59% |
| builtin / otros | 11 | 3.616 | 24% |
| propias (`~/.claude/skills`) | 17 | 2.057 | 14% |
| plugin `sdd-*` | 4 | 283 | 2% |
| plugin `engram` | 1 | 179 | 1% |

### La palanca NO es "escribe descripciones cortas"

Ese era el consejo de la medición anterior, y en esta máquina **ya está
aplicado**: las 17 skills propias promedian 121 B, justo en el objetivo.
Recortarlas todas a 120 B ahorraría **50 B** de 14.987. Nada.

**El 86% del listado es de plugins**, cuyas descripciones no controla quien
instala. La palanca real es otra: **decidir si cada plugin vale su peaje**.

El plugin de Vercel, por ejemplo, aporta 8.852 B de listado más 7.654 B de su
propio hook: **16.506 B, el 24% del peaje total**. Sin él, el peaje baja a
53.107 B. Eso es una decisión, no una optimización de redacción.

---

## 4. Cómo reproducirlo

Un servidor local que captura el cuerpo y devuelve `400`. **No necesita
credenciales**: el cliente manda la petición, se guarda, el cliente falla, y no
se consume cuota.

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8911 \
ANTHROPIC_API_KEY=dummy-no-se-usa \
claude -p "Responde solo: ok"
```

Luego se lee el cuerpo capturado y se buscan las fronteras: la cabecera
`# claudeMd`, las líneas `<hook>:<evento> hook success:`, y
`The following skills are available for use with the Skill tool:`.

**Leer los bytes, no restarlos.** Comparar dos capturas y atribuir la
diferencia es lo que hizo fallar dos veces esta medición:

- Un delta entre una captura con `settings.json` y otra sin él atribuyó
  +28.529 B a los hooks. Falso: a la segunda captura le faltaban también los
  plugins y el MCP. La cifra correcta (19.918 B) sale de leer `messages[1]`.
- Cortar una sección "hasta el siguiente `#`" midió `CONFLICT SURFACING` en
  17.002 B. Falso: no había siguiente `#` y el corte se tragó el listado de
  skills entero. Real: **1.025 B**.

### Precauciones

- **Registrar `sha256` de `~/.claude/CLAUDE.md`, `settings.json` y los
  `SKILL.md` antes y después.** La medición es de solo lectura y debe poder
  demostrarse.
- Lanzar desde un directorio vacío, o el `CLAUDE.md` del proyecto contamina.
- `claude -p` emite varias peticiones; hay que tomar la grande.
- Un `CLAUDE_CONFIG_DIR` de sandbox con `skills/` copiado **no carga ninguna
  skill**. Copiar el directorio no basta; para medir el listado hace falta la
  config real.

---

## 5. Qué NO dice esta medición

- **No es un número universal.** 69,6 kB es de esta máquina con estos plugins.
  Otra instalación dará otra cifra; lo transferible es el método y la forma del
  reparto.
- **No mide otros harnesses.** `opencode` inyecta su listado en formato
  `<available_skills>` XML: 23 skills a 686 B = 15.786 B. Cifra parecida, causa
  distinta, y no comparable entrada por entrada con la de aquí.
- **No dice que `CLAUDE.md` sobre.** Es el 48% del peaje y también lo que
  gobierna el comportamiento del agente. Que sea el bloque más grande no lo
  convierte en el mejor candidato a recortar.
- **No mide la calidad.** Quitar un plugin ahorra bytes y quita capacidades.
  Esta medición dice el precio, no si vale la pena.

---

## 6. Ver también

- [`telemetry-per-request.md`](telemetry-per-request.md) §4.11 — `cache_by_section`,
  qué cubo cayó dentro del prefijo cacheado
- [`optimizer-skills.md`](optimizer-skills.md) — la medición anterior de skills
  (11 skills a 138 B), cuyo consejo de descripciones cortas esta medición matiza
- [`context-tax.md`](context-tax.md) — el desglose completo de una petición real
- [`floor-across-tools.md`](floor-across-tools.md) — el mismo ejercicio comparando
  herramientas
