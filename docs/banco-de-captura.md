# El banco de captura — medir lo que inyecta un harness sin gastar un token

> Validado el **2026-08-09** con Codex 0.142.5 y Qwen Code 0.21.7 contra
> `ollama` local. Coste: **cero**. El método es reproducible en cualquier
> máquina con ollama; los números son de esta.

---

## 1. El problema que resuelve

Medir el peaje fijo exige leer el **cuerpo real** de la petición. No se puede
deducir de la documentación de la herramienta ni restar dos capturas: las dos
cosas ya fallaron y están documentadas en
[`fixed-toll-claude-code.md`](fixed-toll-claude-code.md) §4.

El método anterior —una sonda que guarda el body y devuelve `400`— **solo
estaba verificado para Claude Code**. Con Codex falló, y caro:

> `codex exec` ignoró `OPENAI_BASE_URL`, se fue a su auth de suscripción
> guardada, respondió de verdad y gastó **16.185 tokens de cuota real**. No se
> capturó nada: el cuerpo nunca pasó por el puerto.

Ese incidente tiene dos causas, y el banco arregla las dos.

---

## 2. Aislar la configuración: el harness no puede gastar lo que no tiene

**Apuntar bien no basta.** Si el apuntado falla y la herramienta encuentra
credenciales, se va con ellas. La garantía no puede depender de acertar.

Por eso cada captura se lanza con el directorio de configuración **aislado y
vacío de credenciales**:

| Herramienta | Cómo se aísla | Qué queda fuera |
|---|---|---|
| Codex | `CODEX_HOME=<dir temporal>` | `~/.codex/auth.json` |
| Qwen Code, Gemini CLI | `HOME=<dir temporal>` | `~/.qwen`, `~/.gemini` |
| opencode, `pi` | `HOME=<dir temporal>` | sus credenciales bajo `$HOME` |

Con eso, **el peor caso de un apuntado mal hecho es un error de auth, nunca
una factura.** Es una propiedad estructural, no una precaución.

Se comprobó en el primer intento con Codex: se negó a arrancar por no estar en
un repo git (`--skip-git-repo-check` faltaba) y **no envió ninguna petición**.

---

## 3. Reenviar a un modelo local en vez de devolver un error

Un harness que recibe un `400` puede reintentar, cambiar de ruta o caer a otro
backend. Lo que capturas entonces no es su petición normal, sino la de después
del fallo.

El banco **reenvía a `ollama`** y devuelve la respuesta real. El harness
termina contento y lo que queda en disco es exactamente lo que manda a diario.

Y el modelo local no es una limitación, es lo que hace el método transferible:

> **El bloque de instrucciones lo inyecta el HARNESS, no el modelo.** Su tamaño
> y su marca no dependen de a quién se mande la petición.

Así que una captura contra `llama3.2:3b` mide lo mismo que una contra un modelo
de pago — sin cuenta, sin cuota, sin red. Es lo que
[`fixed-toll-claude-code.md`](fixed-toll-claude-code.md) §5 declara como lo
único transferible: **el método, no el precio**.

`ollama` expone `/v1/chat/completions` **y** `/v1/responses`, así que cubre los
dos dialectos que usan las herramientas medidas.

---

## 4. Cómo se usa

```sh
# 1. El banco (deja los cuerpos crudos en ./capturas)
cargo run --example captura

# 2. Un proyecto sonda con un AGENTS.md de tamaño CONOCIDO
mkdir -p /tmp/sonda && cd /tmp/sonda
printf '# Instrucciones\n\nResponde en una linea.\n' > AGENTS.md
wc -c AGENTS.md      # el dato de referencia

# 3. El harness, aislado y apuntando al banco
```

**Codex** — config aislada declarando el banco como proveedor:

```toml
# $CODEX_HOME/config.toml
model = "llama3.2:3b"
model_provider = "banco"
approval_policy = "never"
sandbox_mode = "danger-full-access"

[model_providers.banco]
name = "banco de captura local"
base_url = "http://127.0.0.1:8912/v1"
wire_api = "responses"
```

```sh
CODEX_HOME=/tmp/banco-codex codex exec --skip-git-repo-check "Responde solo: ok" < /dev/null
```

> `codex exec` **espera stdin**: sin `< /dev/null` se queda colgado.

**Qwen Code** — variables OpenAI-compatibles, con `HOME` aislado:

```sh
HOME=/tmp/home-aislado \
OPENAI_BASE_URL=http://127.0.0.1:8912/v1 \
OPENAI_API_KEY=sonda-no-se-usa \
OPENAI_MODEL=llama3.2:3b \
  qwen -p "Responde solo: ok" < /dev/null
```

Variables del banco: `CAPTURA_PORT` (8912), `CAPTURA_DIR` (`./capturas`),
`CAPTURA_MODELO` (`qwen2.5:7b`), `CAPTURA_OLLAMA` (11434).

---

## 5. Lo que se midió el 2026-08-09

Mismo `AGENTS.md` de **202 B** en los dos casos.

| Herramienta | Bloque | Envoltorio | Ruta | Cierre |
|---|---:|---:|---|---|
| **Codex 0.142.5** | 380 B | **178 B** | absoluta (116 B) | `</INSTRUCTIONS>` |
| **Qwen Code 0.21.7** | 272 B | **70 B** | relativa | `--- End of Context… ---` |
| **opencode 1.18.15** | 349 B | **147 B** | absoluta (126 B) | **ninguno** |

Dos de los tres tienen el envoltorio **dominado por la ruta absoluta del
proyecto** — 65% en Codex, 86% en opencode. Ninguna de sus cifras publicadas es
una constante, y no se pueden comparar entre máquinas sin decir la ruta.

### Codex: el envoltorio depende de DÓNDE tengas el proyecto

```
# AGENTS.md instructions for <RUTA ABSOLUTA>

<INSTRUCTIONS>
…contenido…
</INSTRUCTIONS>
```

De los 178 B de envoltorio, **116 son la ruta absoluta**. Solo 62 B son fijos.

Eso significa que el «+159 B» que circulaba **no es una constante**: es
`62 B + longitud de la ruta absoluta del proyecto`. Un proyecto en `/home/u/p`
paga bastante menos que uno en un directorio profundo, y los 159 salen de una
ruta de ~97 caracteres. **Ese número no se puede comparar entre máquinas sin
decir la ruta.**

> La marca `--- project-doc ---` que se había documentado para Codex **no
> existe en 0.142.5**. `grep project-doc` sobre la captura devuelve cero. Es
> exactamente la deriva contra la que avisa el issue #66: una marca es una
> cadena literal, y las cadenas cambian.

### Qwen: envoltorio fijo y ruta relativa

```
--- Context from: AGENTS.md ---
…contenido…
--- End of Context from: AGENTS.md ---
```

70 B fijos, con **apertura y cierre reales** —encaja directo en
[`block_scan`](../src/provider/block_scan.rs)— y ruta **relativa**, así que no
varía con la profundidad del directorio.

Qwen inyecta además otros bloques `Context from:` (por ejemplo
`.qwen/output-language.md`), así que el detector tendrá que quedarse con el que
corresponde y no con el primero que aparezca.

---

## 6. Reglas que no se saltan

1. **Leer los bytes, no restarlos.** Comparar dos capturas y atribuir la
   diferencia hizo fallar esta medición dos veces
   ([`fixed-toll-claude-code.md`](fixed-toll-claude-code.md) §4).
2. **Las fronteras las pone el envoltorio del harness, nunca el contenido.**
   El contenido es texto del usuario y puede tener cualquier forma.
3. **Anotar la versión exacta** de la herramienta con la que se capturó. Una
   marca es una cadena literal: sin versión, la medición no se puede auditar.
4. **El upstream del banco es siempre `127.0.0.1`.** No se lee de una variable
   y no se reenvía ninguna cabecera del harness — podrían llevar credenciales.
   Si alguien lo cambia, el coste deja de ser cero.

---

## 7. Qué falta

- **opencode: capturado.** Su marca documentada (`Instructions from: <ruta>`)
  **sí sobrevivió** a las diez versiones de deriva, al revés que la de Codex.
  Detector en `provider::instructions`.
- **`pi`**: sin capturar.
- **`pi` manda el cuerpo comprimido con zstd**, único de los cuatro. Sus cifras
  serán **lógicas**; el coste en cable es ~1/3. Publicarlo sin decirlo lo
  penalizaría por partida doble.
- **Los detectores en sí**: este documento es la condición de entrada de #66,
  no su implementación. Un dialecto por PR, cada uno con su captura.

---

## Ver también

- [`fixed-toll-claude-code.md`](fixed-toll-claude-code.md) — el peaje fijo y
  las dos mediciones que fallaron
- [`telemetry-per-request.md`](telemetry-per-request.md) §4.13 y §4.17 — los
  campos `instructions` y `hooks`
- [`skills-across-tools.md`](skills-across-tools.md) §6 — la tabla de marcas
  por herramienta, ahora con dos entradas verificadas
