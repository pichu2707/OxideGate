//! Corredor del nivel 1 de [#29](https://github.com/pichu2707/OxideGate/issues/29):
//! el mismo modelo local para todos los harnesses, `n` alto y coste cero.
//!
//! Primera rodaja de [#121](https://github.com/pichu2707/OxideGate/issues/121):
//! **un solo harness**, Codex. La infraestructura es la definitiva; lo que falta
//! son las otras tres recetas de aislamiento.
//!
//! # Por qué una rodaja y no los cuatro de una
//!
//! Esta rodaja es a la vez el corredor y su **calibración go/no-go**, y hay un
//! motivo concreto para gastar la primera corrida en eso.
//!
//! [`modelo-del-nivel-1.md`](../docs/modelo-del-nivel-1.md) mide que apagar el
//! razonamiento de `qwen3:14b` **cuesta iniciativa**, y que el uso de
//! herramientas se vuelve binario según la redacción del encargo:
//!
//! | redacción | emite |
//! |---|---|
//! | «averigua cuál es el error» | 30/30 |
//! | «arréglalo» (con contexto) | **0/30** |
//! | «los tests de `tarifa.py` fallan» | **0/30** |
//!
//! Y el `TAREA.md` del banco está escrito, literalmente, en la familia de
//! abajo: *«`test_tarifa.py` falla. Haz que pase.»* Constatación más
//! imperativo — las dos formas que miden 0/30 con el modelo desnudo.
//!
//! El propio documento sostiene que eso **no** debería estorbar, porque «los
//! cuatro harnesses nombran sus herramientas en el system prompt» y eso pone al
//! modelo en la condición de techo. Es una hipótesis razonable y **no está
//! comprobada**: el mismo documento cierra con «esto abre la puerta; no la
//! cruza».
//!
//! Si la hipótesis falla, el corredor devuelve 0/n y el nivel 1 satura — como
//! `llama3.2:3b` a 0/10 quedó fuera del banco de tareas. Averiguarlo cuesta una
//! corrida; suponerlo costaría las otras tres recetas.
//!
//! Por eso [`ENCARGO`] es sobreescribible: el corredor lleva dentro su propio
//! discriminador. Ver §«El diagnóstico» más abajo.
//!
//! # Lo que este corredor NO hace
//!
//! - **No publica una tabla ni medianas.** Eso es
//!   [#122](https://github.com/pichu2707/OxideGate/issues/122).
//! - **No mide con cuota real.** Eso es el nivel 2,
//!   [#123](https://github.com/pichu2707/OxideGate/issues/123).
//! - **No inventa telemetría.** La escribe el propio proxy, igual que en
//!   producción; aquí solo se lee y se atribuye.
//!
//! # Las reglas del banco que este corredor hereda
//!
//! De [`banco-de-tareas.md`](../docs/banco-de-tareas.md) §6, y no son
//! negociables:
//!
//! 1. **El estado inicial tiene que fallar.** Se comprueba y se aborta si pasa.
//! 2. **Cada ejecución parte de una copia limpia.** Sin eso la segunda
//!    repetición hereda el arreglo de la primera y la tasa sale inflada.
//! 3. **El veredicto es el código de salida**, no lo que diga por pantalla.
//! 4. **Los fallos del banco se cuentan aparte de los del modelo.** Ver
//!    [`Veredicto`]: colapsarlos todos en «no resuelto» es exactamente el error
//!    que hizo publicar un 2/10 donde había un 4/10.
//! 5. **La tasa de éxito es un resultado, no un filtro.** Lo que no resuelve se
//!    cuenta y se publica.
//!
//! Y de [`banco-de-captura.md`](../docs/banco-de-captura.md) §2 y §6:
//!
//! 6. **La config del harness va aislada y sin credenciales.** El peor caso de
//!    un apuntado mal hecho tiene que ser un error de auth, nunca una factura.
//!    Es una propiedad estructural, no una precaución.
//! 7. **Se anota la versión exacta del harness.** Sin versión, la medición no
//!    se puede auditar.
//!
//! # El diagnóstico, si sale 0/n
//!
//! Un 0/n no dice por sí solo de quién es la culpa. Por eso el reporte NO
//! colapsa los fallos, y hay dos que apuntan fuera del modelo:
//!
//! - [`Veredicto::SinPeticiones`] — el harness no mandó ni una petición al
//!   proxy. Eso es **cableado**, no capacidad. Contarlo como «no resuelto»
//!   sería culpar al modelo de un fallo del banco.
//! - [`Veredicto::NoToco`] — mandó peticiones y no editó el fichero. Ese es el
//!   síntoma EXACTO que hundió el primer intento de #121: seis turnos
//!   inventándose respuestas de herramienta sin tocar la tarea.
//!
//! Y si el corredor sale 0/n con el encargo de fábrica, la contraprueba de una
//! línea separa «el harness no rescata la redacción» de «el cableado está roto»:
//!
//! ```sh
//! CORREDOR_ENCARGO="Averigua por que falla test_tarifa.py y corrigelo." \
//!   cargo run --example corredor-nivel-1
//! ```
//!
//! Si con «averigua» sube y con el de fábrica no, el hallazgo no es que el
//! nivel 1 no exista: es que **la redacción del encargo es un confundidor de
//! primer orden**, que es justo lo que #29 quiere medir en vez de sufrir.
//!
//! # Uso
//!
//! OxideGate tiene que estar arriba **y enrutando al modelo local**. No se
//! deduce: se comprueba y se aborta si no (ver [`guarda_proxy`]).
//!
//! ```sh
//! # 1. El proxy, apuntando a ollama
//! OPENAI_API_BASE=http://127.0.0.1:11434/v1 cargo run --release
//!
//! # 2. El corredor
//! cargo run --example corredor-nivel-1
//! CORREDOR_N=20 cargo run --example corredor-nivel-1
//! ```
//!
//! Variables: `CORREDOR_N` (3), `CORREDOR_MODELO` (`qwen3:14b-nothink`),
//! `CORREDOR_PUERTO` (8899), `CORREDOR_TAREA` (`tareas/reparar-tarifa`),
//! `CORREDOR_TIMEOUT` (300 s por repetición), `CORREDOR_ENCARGO`.

use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// El fichero que hay que reparar. Es el único que el harness debería tocar.
const FUENTE: &str = "tarifa.py";

/// El verificador. **No se copia como editable**: el `TAREA.md` dice que no se
/// toque, y si el harness lo reescribe para que pase, la medición es basura.
/// Se comprueba (ver [`Veredicto::TestsAlterados`]).
const TESTS: &str = "test_tarifa.py";

/// El fichero de instrucciones, normalizado.
///
/// **Tiene que ser el mismo contenido para los cuatro harnesses.** Con
/// contenido distinto se estaría midiendo la configuración del usuario, que es
/// justo lo que #29 dice que NO es comparar trabajo.
///
/// Gracias a [#66](https://github.com/pichu2707/OxideGate/issues/66) se sabe
/// cuánto ocupa este mismo fichero en cada dialecto (62/21/55 B + ruta en
/// Codex/opencode/`pi`, 70 B fijos en Qwen), así que la diferencia de envoltorio
/// es **un dato conocido y no una sorpresa**.
const AGENTS_MD: &str = "\
# Instrucciones

Trabaja en este directorio. Ejecuta `python3 test_tarifa.py` para comprobar tu
trabajo: sale 0 si pasa.

No modifiques `test_tarifa.py`.
";

/// El encargo que recibe el harness.
///
/// De fábrica es la redacción del `TAREA.md`, que es la honesta: es la tarea
/// tal y como está escrita. **Y está en la familia que mide 0/30** con el modelo
/// desnudo — ver el doc-comment del módulo. Sobreescribible con
/// `CORREDOR_ENCARGO` para correr la contraprueba.
const ENCARGO: &str = "test_tarifa.py falla. Haz que pase.";

/// El encargo del **modo peaje**: la ceremonia sin trabajo.
///
/// Mismo prompt que [`floor-across-tools.md`](../docs/floor-across-tools.md)
/// §1, para que las dos mediciones hablen del mismo estímulo.
const ENCARGO_PEAJE: &str = "Responde solo: ok";

/// Qué harness conduce esta corrida.
///
/// Empezó siendo una constante —Codex— y **tuvo que dejar de serlo**: Codex
/// 0.142.5 no puede operar herramientas contra el `/v1/responses` de ollama (su
/// router las rechaza con `unsupported call:` y el nombre vacío), y no hay
/// salida por el dialecto porque `wire_api = "chat"` está eliminado en esa
/// versión. Comprobado que **no es cosa de OxideGate**: apuntando Codex
/// directamente a ollama pasa lo mismo.
///
/// `pi` sí conduce: resolvió la tarea en 7 turnos el 2026-08-30. Es el defecto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Harness {
    /// El que cruza la puerta. `openai-completions`, 2.9 KB de herramientas.
    Pi,
    /// El segundo que cruza la puerta. Declara 19,5 KB de herramientas — 6,7
    /// veces las de `pi`— y aun así cabe de sobra.
    Opencode,
    /// Se conserva para poder REPRODUCIR el bloqueo, no para medir con él.
    Codex,
}

impl Harness {
    fn desde(nombre: &str) -> Option<Harness> {
        match nombre {
            "pi" => Some(Harness::Pi),
            "opencode" => Some(Harness::Opencode),
            "codex" => Some(Harness::Codex),
            _ => None,
        }
    }

    /// Dónde vive el fichero de credenciales de este harness, relativo a `HOME`.
    ///
    /// Se copia **una sola entrada** de ahí, nunca el fichero entero: el de
    /// opencode lleva `openai`, `google`, `anthropic` y `oxidegate`, y llevarse
    /// las cuatro a un directorio temporal por comodidad seria regalar tres
    /// credenciales que no hacen falta.
    fn ruta_credencial(self) -> Option<&'static str> {
        match self {
            Harness::Opencode => Some(".local/share/opencode/auth.json"),
            Harness::Pi => Some(".pi/agent/auth.json"),
            Harness::Codex => Some(".codex/auth.json"),
        }
    }

    /// El plugin que enruta el tráfico de este harness por OxideGate, si lo
    /// necesita.
    ///
    /// # Por qué hace falta copiarlo, y por qué duele
    ///
    /// El nivel 1 lanza opencode con `--pure` **a propósito**: sin plugins
    /// externos, para medir opencode y no lo que alguien le haya instalado
    /// encima. Pero `--pure` desactiva justo el plugin que redirige a
    /// OxideGate, y sin él el harness va **directo al upstream real, sin pasar
    /// por el proxy y sin telemetría**. Lo que hace reproducible la medida es
    /// lo que impide medirla.
    ///
    /// Y no hay salida por la config: el `CodexAuthPlugin` de opencode
    /// **hardcodea** el endpoint de Codex e **ignora**
    /// `provider.openai.options.baseURL` — verificado contra el binario. El
    /// parche de `fetch` del plugin es la única costura.
    ///
    /// Así que se copia **ese plugin y ninguno más**. Al ser el único del
    /// `HOME` aislado, quitar `--pure` carga ese y nada más.
    fn ruta_plugin(self) -> Option<&'static str> {
        match self {
            Harness::Opencode => Some(".config/opencode/plugins/oxidegate-codex.ts"),
            Harness::Pi | Harness::Codex => None,
        }
    }

    /// El binario que se invoca.
    fn binario(self) -> &'static str {
        match self {
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
            Harness::Codex => "codex",
        }
    }
}

/// Qué pasó en una repetición.
///
/// **No se colapsan.** La regla 4 del banco existe porque colapsarlos ya
/// escondió un fallo del instrumento del mismo tamaño que el efecto medido.
/// Cada variante de abajo señala a un culpable distinto, y tres de las seis NO
/// son del modelo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Veredicto {
    /// Los tests pasan. Único éxito.
    Resuelto,
    /// El harness editó la fuente y los tests siguen fallando. **Fallo legítimo
    /// del modelo**: lo intentó y no salió.
    NoResuelto,
    /// Mandó peticiones y no tocó la fuente. Es el síntoma exacto que hundió el
    /// primer intento de #121: turnos inventándose respuestas de herramienta
    /// sin llegar a la tarea.
    NoToco,
    /// Ni una petición llegó al proxy. **Fallo del banco, no del modelo**:
    /// cableado, aislamiento o el harness negándose a arrancar.
    SinPeticiones,
    /// Reescribió el verificador. La medición de esa repetición no vale: un
    /// test que se reescribe a sí mismo pasa siempre.
    TestsAlterados,
    /// Alguna petición llegó al modelo **con el prompt cortado**: el harness
    /// mandó más contexto del que cabe. **Fallo del banco**: lo que se midió no
    /// es el estímulo que se creía estar mandando.
    PromptTruncado,
    /// Se pasó del tiempo. Se cuenta aparte porque un timeout no dice si iba
    /// bien o mal encaminado.
    Timeout,
}

impl Veredicto {
    /// Etiqueta corta para el reporte.
    fn etiqueta(self) -> &'static str {
        match self {
            Veredicto::Resuelto => "resuelto",
            Veredicto::NoResuelto => "no resuelto",
            Veredicto::NoToco => "no toco el fichero",
            Veredicto::SinPeticiones => "sin peticiones (banco)",
            Veredicto::TestsAlterados => "tests alterados (nulo)",
            Veredicto::PromptTruncado => "prompt truncado (banco)",
            Veredicto::Timeout => "timeout",
        }
    }

    /// Si el fallo es del **banco** y no del modelo. Estas filas no pueden
    /// entrar en el denominador de una tasa de capacidad sin mentir.
    fn es_fallo_del_banco(self) -> bool {
        matches!(
            self,
            Veredicto::SinPeticiones | Veredicto::TestsAlterados | Veredicto::PromptTruncado
        )
    }
}

/// Decide el veredicto de una repetición a partir de hechos observados, no de
/// lo que el harness diga por pantalla (regla 3 del banco).
///
/// El orden importa y es deliberado:
/// 1. Los tests alterados anulan la repetición **antes** de mirar si pasan:
///    un verificador reescrito pasa siempre.
/// 2. Sin peticiones es un fallo del banco aunque además haya habido timeout;
///    lo que hay que arreglar es el cableado.
/// 3. Un prompt cortado invalida la repetición **antes** de mirar si resolvió:
///    aunque resolviera, no lo hizo con el estímulo que se creía mandar.
/// 4. El éxito se mide por el código de salida del verificador.
fn clasificar(
    tests_alterados: bool,
    peticiones: usize,
    truncado: bool,
    toco_fuente: bool,
    expiro: bool,
    tests_pasan: bool,
) -> Veredicto {
    if tests_alterados {
        return Veredicto::TestsAlterados;
    }
    if peticiones == 0 {
        return Veredicto::SinPeticiones;
    }
    // Antes que cualquier lectura de capacidad: si el estimulo llego cortado,
    // lo que venga despues no es sobre el modelo.
    if truncado {
        return Veredicto::PromptTruncado;
    }
    if tests_pasan {
        return Veredicto::Resuelto;
    }
    if expiro {
        return Veredicto::Timeout;
    }
    if !toco_fuente {
        return Veredicto::NoToco;
    }
    Veredicto::NoResuelto
}

/// La config aislada de Codex: declara OxideGate como proveedor y **no lleva
/// credenciales**.
///
/// El dialecto es una **variable del banco**, no una constante, y es la primera
/// cosa que hay que descartar cuando el harness llega al modelo y no hace nada.
///
/// Codex habla `responses` por defecto, y los tres eslabones existen —OxideGate
/// lo expone en `/v1/responses` y ollama también—. Pero «existe» no es
/// «interopera»: medido el 2026-08-30, por `responses` el modelo emite la
/// llamada, ollama la traduce, y el router de Codex la rechaza con
/// `error=unsupported call:` y el nombre VACÍO. El modelo contesta entonces «no
/// puedo ejecutar comandos» y no toca la tarea.
/// Copia al `HOME` aislado **una sola entrada** del fichero de credenciales
/// real, la del proveedor pedido.
///
/// # La garantía que esto rompe, y hasta dónde
///
/// El nivel 1 corre con el `HOME` aislado **vacío de credenciales**, y por eso
/// el peor caso de un apuntado mal hecho es un error de auth y **nunca una
/// factura**. Es una propiedad estructural, no una precaución
/// (`banco-de-captura.md` §2).
///
/// El nivel 2 **necesita** la credencial real: mide «cómo se usa de verdad».
/// Así que la garantía se degrada, y hay que decir exactamente a qué: de
/// «ninguna credencial» a **«solo la del proveedor nombrado»**. El fichero de
/// opencode lleva cuatro (`openai`, `google`, `anthropic`, `oxidegate`);
/// copiarlo entero por comodidad sería regalar tres que no hacen falta.
///
/// **Nunca se imprime el valor**, ni en un error: el mensaje dice qué
/// proveedor faltaba, no lo que había.
fn copiar_credencial(h: Harness, hogar: &Path, proveedor: &str) -> Result<(), String> {
    let rel = h
        .ruta_credencial()
        .ok_or_else(|| format!("no se donde guarda `{}` sus credenciales", h.binario()))?;
    let real = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rel);
    let bruto = std::fs::read_to_string(&real)
        .map_err(|e| format!("no puedo leer {}: {e}", real.display()))?;
    let v: Value =
        serde_json::from_str(&bruto).map_err(|_| format!("{} no es JSON", real.display()))?;

    // Algunos lo anidan bajo `providers`, otros lo ponen en la raiz.
    let raiz = v.get("providers").unwrap_or(&v);
    let entrada = raiz
        .get(proveedor)
        .ok_or_else(|| {
            let hay: Vec<&str> = raiz
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            format!("`{proveedor}` no esta en {}. Hay: {hay:?}", real.display())
        })?
        .clone();

    let solo_una = if v.get("providers").is_some() {
        serde_json::json!({ "providers": { proveedor: entrada } })
    } else {
        serde_json::json!({ proveedor: entrada })
    };

    let destino = hogar.join(rel);
    if let Some(padre) = destino.parent() {
        std::fs::create_dir_all(padre).map_err(|e| format!("no puedo crear {padre:?}: {e}"))?;
    }
    std::fs::write(&destino, solo_una.to_string())
        .map_err(|e| format!("no puedo escribir la credencial aislada: {e}"))
}

/// Copia al `HOME` aislado **el plugin de enrutado y ninguno más**.
///
/// Ver [`Harness::ruta_plugin`] para por qué hace falta. Si el harness no
/// necesita plugin, no hace nada y no es un error.
fn copiar_plugin(h: Harness, hogar: &Path) -> Result<(), String> {
    let Some(rel) = h.ruta_plugin() else {
        return Ok(());
    };
    let real = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rel);
    if !real.exists() {
        return Err(format!(
            "falta el plugin de enrutado en {}. Sin el, el harness va DIRECTO al \
             upstream real: gastaria cuota sin capturar telemetria, que es \
             exactamente el fallo que #123 avisa",
            real.display()
        ));
    }
    let destino = hogar.join(rel);
    if let Some(padre) = destino.parent() {
        std::fs::create_dir_all(padre).map_err(|e| format!("no puedo crear {padre:?}: {e}"))?;
    }
    std::fs::copy(&real, &destino)
        .map(|_| ())
        .map_err(|e| format!("no puedo copiar el plugin: {e}"))
}

/// Escribe la config aislada que le toca a cada harness, en el sitio donde ese
/// harness la busca. Un sitio equivocado no da error: el harness arranca con su
/// proveedor por defecto —la nube— y la corrida entera mide otra cosa.
fn escribir_config(
    h: Harness,
    hogar: &Path,
    puerto: &str,
    modelo: &str,
    wire: &str,
) -> std::io::Result<()> {
    match h {
        Harness::Pi => {
            let dir = hogar.join(".pi").join("agent");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("models.json"), config_pi(puerto, modelo))
        }
        Harness::Opencode => {
            let dir = hogar.join(".config").join("opencode");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("opencode.json"), config_opencode(puerto, modelo))
        }
        Harness::Codex => {
            std::fs::create_dir_all(hogar)?;
            std::fs::write(
                hogar.join("config.toml"),
                config_codex(puerto, modelo, wire),
            )
        }
    }
}

/// La config de opencode, aislada y **sin credenciales**.
///
/// Va en `$HOME/.config/opencode/opencode.json`. Declara OxideGate como
/// proveedor `openai-compatible` y **abre los permisos de edición y bash**: sin
/// eso opencode pide confirmación y en modo no interactivo se queda sin hacer
/// nada, que se contaría como «no tocó el fichero» culpando al modelo.
///
/// `webfetch` va a `deny` a propósito: el banco es de coste cero y sin red
/// hacia fuera. Una tarea que se resolviera buscando en internet no es la tarea.
fn config_opencode(puerto: &str, modelo: &str) -> String {
    format!(
        "{{\n\
         \x20 \"provider\": {{\n\
         \x20   \"oxidegate\": {{\n\
         \x20     \"npm\": \"@ai-sdk/openai-compatible\",\n\
         \x20     \"name\": \"OxideGate local\",\n\
         \x20     \"options\": {{ \"baseURL\": \"http://127.0.0.1:{puerto}/v1\", \"apiKey\": \"no-se-usa\" }},\n\
         \x20     \"models\": {{ \"{modelo}\": {{ \"name\": \"{modelo}\" }} }}\n\
         \x20   }}\n\
         \x20 }},\n\
         \x20 \"model\": \"oxidegate/{modelo}\",\n\
         \x20 \"permission\": {{ \"edit\": \"allow\", \"bash\": \"allow\", \"webfetch\": \"deny\" }}\n\
         }}\n"
    )
}

/// El registro de proveedores de `pi`, aislado y **sin credenciales**.
///
/// Va en `$HOME/.pi/agent/models.json`. **No sirve `models-store.json`**, que es
/// solo la caché del catálogo: escribir ahí da `Unknown provider`
/// (`banco-de-captura.md` §4).
fn config_pi(puerto: &str, modelo: &str) -> String {
    format!(
        "{{\n\
         \x20 \"providers\": {{\n\
         \x20   \"oxidegate\": {{\n\
         \x20     \"baseUrl\": \"http://127.0.0.1:{puerto}/v1\",\n\
         \x20     \"api\": \"openai-completions\",\n\
         \x20     \"apiKey\": \"no-se-usa\",\n\
         \x20     \"compat\": {{ \"supportsDeveloperRole\": false, \"supportsReasoningEffort\": false }},\n\
         \x20     \"models\": [{{ \"id\": \"{modelo}\" }}]\n\
         \x20   }}\n\
         \x20 }}\n\
         }}\n"
    )
}

fn config_codex(puerto: &str, modelo: &str, wire: &str) -> String {
    format!(
        "model = \"{modelo}\"\n\
         model_provider = \"oxidegate\"\n\
         approval_policy = \"never\"\n\
         sandbox_mode = \"danger-full-access\"\n\
         \n\
         [model_providers.oxidegate]\n\
         name = \"OxideGate -> modelo local\"\n\
         base_url = \"http://127.0.0.1:{puerto}/v1\"\n\
         wire_api = \"{wire}\"\n"
    )
}

fn var(nombre: &str, defecto: &str) -> String {
    std::env::var(nombre).unwrap_or_else(|_| defecto.to_string())
}

fn hash_fichero(p: &Path) -> Option<u64> {
    let contenido = std::fs::read(p).ok()?;
    let mut h = DefaultHasher::new();
    contenido.hash(&mut h);
    Some(h.finish())
}

/// Deja una copia limpia de la tarea en `destino` (regla 2 del banco).
fn preparar(origen: &Path, destino: &Path) -> std::io::Result<()> {
    if destino.exists() {
        std::fs::remove_dir_all(destino)?;
    }
    std::fs::create_dir_all(destino)?;
    for f in [FUENTE, TESTS] {
        std::fs::copy(origen.join(f), destino.join(f))?;
    }
    std::fs::write(destino.join("AGENTS.md"), AGENTS_MD)?;
    Ok(())
}

/// Deja el directorio del **modo peaje**: exactamente el mismo entorno que una
/// repetición normal **menos la tarea**.
///
/// Lleva el `AGENTS.md` normalizado y NO lleva los ficheros de la tarea. Esa
/// frontera es la que da sentido a la resta de #122: el fichero de
/// instrucciones es **ceremonia**, no trabajo, así que tiene que caer del lado
/// del peaje o el «trabajo real» se lo comería.
///
/// # En qué se diferencia del peaje de `floor-across-tools.md` §1
///
/// Aquel se mide «en un directorio vacío, con la configuración realmente
/// instalada en esta máquina» — 22 skills, MCP, todo. **Este se mide con el
/// `HOME` aislado del corredor**, que no tiene nada de eso.
///
/// No es un refinamiento: son cifras de instalaciones distintas y **no se
/// pueden restar entre sí**. El propio §4.2 lo dice: «los totales no son
/// comparables entre instalaciones». Medido el 2026-08-30, el peaje publicado
/// de opencode es **48 veces** su primer turno real dentro del corredor.
fn preparar_peaje(destino: &Path) -> std::io::Result<()> {
    if destino.exists() {
        std::fs::remove_dir_all(destino)?;
    }
    std::fs::create_dir_all(destino)?;
    std::fs::write(destino.join("AGENTS.md"), AGENTS_MD)
}

/// Corre el verificador. El veredicto es el **código de salida** (regla 3).
fn tests_pasan(dir: &Path) -> bool {
    std::process::Command::new("python3")
        .arg(TESTS)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ruta_telemetria() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("oxidegate")
        .join("telemetry.jsonl")
}

/// Cuenta las filas de telemetría que hay ahora. Se usa como marca de agua para
/// atribuir a esta repetición solo lo que se escribió durante ella.
fn filas_telemetria(p: &Path) -> usize {
    std::fs::read_to_string(p)
        .map(|c| c.lines().count())
        .unwrap_or(0)
}

/// Lee las filas nuevas desde `desde` **filtrando por modelo**.
///
/// El filtro no es cosmético: el fichero de telemetría es el mismo que usa el
/// proxy en producción, así que el tráfico normal de quien corre esto caería
/// dentro de la ventana y contaría como peticiones del harness.
fn filas_nuevas(p: &Path, desde: usize, modelo: &str) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .skip(desde)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["model"].as_str() == Some(modelo))
        .collect()
}

/// Cuánto por debajo del techo se considera ya «pegado al techo». El
/// tokenizador no aterriza exacto en el máximo.
const MARGEN_TECHO: usize = 64;

/// ¿Alguna petición llegó pegada al techo del contexto?
///
/// `input_tokens ≈ techo` es la **única** firma del truncamiento: ollama no
/// avisa, no cambia el código de estado y no toca el cuerpo. Lo que se lee es
/// lo que cupo.
///
/// Medido el 2026-08-30: Codex a 4095 contra un techo de 4096, y Qwen Code a
/// **32767 contra un techo de 32768** —sus 100 KB de declaraciones de
/// herramientas no caben ni en 32k—. Los dos casos habrían pasado por buenos.
fn alguna_truncada(filas: &[Value], techo: usize) -> bool {
    let umbral = techo.saturating_sub(MARGEN_TECHO);
    filas
        .iter()
        .filter_map(|f| f["input_tokens"].as_u64())
        .any(|t| t as usize >= umbral)
}

/// Las llamadas a herramientas de un lote de filas, o `None` si **el proveedor
/// de esa ruta no las extrae**.
///
/// La distinción no es un detalle: `src/telemetry/mcp.rs` lo deja escrito —
/// «`tool_calls: null` significa *este proveedor no mide invocaciones*», que es
/// otra cosa que «el modelo no invocó nada». Y los dos dialectos de OpenAI
/// (`/v1/chat/completions` y `/v1/responses`) declaran hoy
/// `captura_invocaciones() -> false`, así que para Codex esto es SIEMPRE `None`.
///
/// Leerlo como un cero y publicar un aviso encima es exactamente el error que
/// la fe de erratas de este proyecto lleva documentando desde la E-004: darle
/// un significado a la ausencia de dato. Aquí se publica «n/d» y no se concluye
/// nada.
///
/// El campo además **no es un número**: es `{"invoked": [...]}`.
fn tool_calls(filas: &[Value]) -> Option<usize> {
    let mut total = 0usize;
    let mut alguna = false;
    for f in filas {
        if let Some(invoked) = f["tool_calls"]["invoked"].as_array() {
            alguna = true;
            total += invoked.len();
        }
    }
    alguna.then_some(total)
}

/// Formatea el contador para el reporte, sin convertir un «no se mide» en un
/// cero.
fn fmt_calls(v: Option<usize>) -> String {
    v.map_or_else(|| "n/d".to_string(), |n| n.to_string())
}

/// Bytes de contexto que el harness MANDÓ en un lote de peticiones.
///
/// Es lo que `context_measured_bytes` mide en la entrada, antes de que el
/// modelo trunque nada. Distinto de `input_tokens`, que es lo que el proveedor
/// dijo haber leído: cuando hay truncamiento los dos divergen, y el que dice lo
/// que costó mandar es este.
fn bytes_mandados(filas: &[Value]) -> u64 {
    filas
        .iter()
        .filter_map(|f| f["context_measured_bytes"].as_u64())
        .sum()
}

/// La versión del harness, o `None` si no está instalado (regla 7 de captura:
/// sin versión la medición no se puede auditar).
fn version_harness(h: Harness) -> Option<String> {
    let salida = std::process::Command::new(h.binario())
        .arg("--version")
        .output()
        .ok()?;
    if !salida.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&salida.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
    )
}

/// Comprueba que el proxy está arriba **y enruta al modelo local**.
///
/// No basta con que responda: una instancia de OxideGate apuntando a
/// `api.openai.com` contesta igual de bien y el corredor mediría otro modelo
/// —o, peor, gastaría cuota— sin decir nada. Se manda una petición real por la
/// misma ruta que usará el harness.
async fn guarda_proxy(cliente: &reqwest::Client, puerto: &str, modelo: &str) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{puerto}/v1/responses");
    let cuerpo = serde_json::json!({
        "model": modelo,
        "input": "di ok",
        "max_output_tokens": 16,
    });
    let resp = cliente
        .post(&url)
        .json(&cuerpo)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("no contesta en {puerto}: {e}"))?;

    let estado = resp.status();
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("respuesta ilegible ({estado}): {e}"))?;

    if v["model"].as_str() != Some(modelo) {
        return Err(format!(
            "el proxy no enruta a `{modelo}` (contesto: {}). \
             Arrancalo con OPENAI_API_BASE=http://127.0.0.1:11434/v1",
            v["model"].as_str().unwrap_or("sin modelo")
        ));
    }
    Ok(())
}

/// Tokens que la guarda de contexto exige que quepan.
///
/// Codex manda ~6500 tokens de prompt real (system + 20 KB de declaraciones de
/// herramientas + el encargo), medido el 2026-08-30. El umbral va por encima
/// con margen: lo que hay que descartar es que el estímulo llegue cortado, no
/// afinar el mínimo.
const CONTEXTO_MINIMO: usize = 8_000;

/// Comprueba que el modelo **no trunca el prompt**, y aborta si lo hace.
///
/// Es la tercera ancla de este banco, y la más cara de aprender: ollama aplica
/// su `num_ctx` por defecto —4096 en 0.30.10— aunque el modelo declare 40960 de
/// contexto, y **corta el prompt en silencio**. Ni un error, ni un aviso: la
/// petición sale `200` y el modelo contesta a lo que le quedó.
///
/// La primera corrida de este corredor dio 0/3 con Codex y el prompt cortado a
/// 4095 tokens. Sin esta guarda, ese cero se habría leído como «el modelo no
/// sabe conducir un harness», que es una conclusión sobre el modelo sacada de
/// un fallo del banco — el error que la fe de erratas lleva documentando desde
/// la E-004.
///
/// Se manda un prompt de tamaño conocido por la misma ruta que usa el harness y
/// se compara lo que el proveedor dice haber leído contra lo que se envió.
async fn guarda_contexto(
    cliente: &reqwest::Client,
    puerto: &str,
    modelo: &str,
) -> Result<usize, String> {
    // Deliberadamente ENORME: mas grande que cualquier techo plausible. Lo que
    // el proveedor diga haber leido ES el techo efectivo, medido en vez de
    // supuesto. Un prompt que solo supere el minimo probaria que caben 8000 y
    // dejaria pasar el caso de qwen, que revienta un techo de 32768.
    let relleno = "lorem ipsum dolor sit amet consectetur ".repeat(12_000);
    let url = format!("http://127.0.0.1:{puerto}/v1/responses");
    let cuerpo = serde_json::json!({
        "model": modelo,
        "input": relleno,
        "max_output_tokens": 8,
    });
    let v: Value = cliente
        .post(&url)
        .json(&cuerpo)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| format!("la sonda de contexto no llego: {e}"))?
        .json()
        .await
        .map_err(|e| format!("la sonda de contexto devolvio algo ilegible: {e}"))?;

    let leidos = v["usage"]["input_tokens"]
        .as_u64()
        .ok_or("la sonda de contexto no devolvio usage.input_tokens")? as usize;

    if leidos < CONTEXTO_MINIMO {
        return Err(format!(
            "el techo de contexto efectivo son {leidos} tokens.\n\
             \x20 Con menos de {CONTEXTO_MINIMO} no cabe el prompt de un harness real y\n\
             \x20 cualquier tasa medida seria una tasa sobre un estimulo cortado.\n\
             \x20 Arreglo: derivar el modelo con `PARAMETER num_ctx 32768` (constante\n\
             \x20 para los cuatro harnesses, igual que el razonamiento apagado), o\n\
             \x20 arrancar ollama con OLLAMA_CONTEXT_LENGTH=32768."
        ));
    }
    Ok(leidos)
}

/// Lanza Codex aislado sobre `trabajo`. Devuelve `true` si expiró el plazo.
///
/// El aislamiento es la regla 6: `env_clear()` para no heredar nada de la
/// sesión, `HOME` y `CODEX_HOME` temporales para dejar fuera
/// `~/.codex/auth.json`. El peor caso de un apuntado mal hecho es un error de
/// auth, nunca una factura.
#[allow(clippy::too_many_arguments)]
async fn lanzar(
    h: Harness,
    trabajo: &Path,
    hogar: &Path,
    modelo: &str,
    encargo: &str,
    plazo: u64,
    nivel: u8,
    puerto: &str,
) -> (bool, String) {
    let mut cmd = Command::new(h.binario());

    // `env_clear()` PRIMERO, y no es estilo: en `std::process::Command` borra
    // tambien las variables ya puestas explicitamente. Comprobado con rustc
    // 1.96: una `.env("MARCA", ..)` antes de `.env_clear()` llega VACIA al
    // hijo. Ponerlo al final habria dejado a `pi` sin `PI_OFFLINE` y a Codex
    // sin `CODEX_HOME`, y el fallo no daria ningun error: el harness
    // simplemente se comportaria de otra forma.
    cmd.env_clear()
        .current_dir(trabajo)
        .env("HOME", hogar)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TERM", "dumb");

    if nivel == 2 {
        // El plugin de enrutado lee esto para saber donde escucha el proxy.
        // Sin ello apuntaria al puerto por defecto, que puede no ser el nuestro
        // — y como es fail-open, el fallo seria MUDO.
        cmd.env("OXIDEGATE_URL", format!("http://127.0.0.1:{puerto}"));
    }

    match h {
        Harness::Pi => {
            cmd.env("PI_OFFLINE", "1").args([
                "--provider",
                "oxidegate",
                "--model",
                modelo,
                "--api-key",
                "no-se-usa",
                "--print",
                "--no-session",
                "--offline",
                // `--approve` NO es opcional: sin el, `pi` puede no confiar en
                // los ficheros project-local, no inyectar el AGENTS.md y dejar
                // la corrida vacia SIN dar ningun error que lo delate
                // (`banco-de-captura.md` §4).
                "--approve",
            ]);
            cmd.arg(encargo);
        }
        Harness::Opencode => {
            cmd.args([
                "run",
                // Sin plugins externos: lo que se mide es opencode, no lo que
                // alguien le haya instalado encima.
                "--pure", "-m",
            ])
            .arg(format!("oxidegate/{modelo}"))
            .arg(encargo);
        }
        Harness::Codex => {
            // Su config vive en CODEX_HOME, no bajo HOME.
            cmd.env("CODEX_HOME", hogar)
                .arg("exec")
                // Sin esto Codex se niega a arrancar fuera de un repo git y no
                // manda ninguna peticion (`banco-de-captura.md` §2).
                .arg("--skip-git-repo-check")
                .arg(encargo);
        }
    }

    cmd
        // Al expirar el plazo se suelta el futuro, y con el el hijo. Sin esto
        // tokio NO lo mata: el harness seguiria vivo, mandando peticiones que
        // caerian en la ventana de telemetria de la repeticion SIGUIENTE y
        // pudiendo seguir editando su directorio. Corromperia las repeticiones
        // posteriores en silencio, que es la peor clase de fallo del banco.
        .kill_on_drop(true)
        // Los dos esperan stdin: sin cerrarlo se quedan colgados.
        .stdin(std::process::Stdio::null())
        // Se CAPTURA, no se tira. Cuando el harness llega al modelo y aun asi
        // no toca la tarea, su propia salida es el unico rastro de lo que hizo
        // — la telemetria no lo dice: los dialectos de OpenAI no extraen
        // invocaciones. Tirarla a `null` fue lo que dejo el primer diagnostico
        // sin nada sobre lo que apoyarse.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let Ok(hijo) = cmd.spawn() else {
        return (false, String::from("<no se pudo lanzar el harness>"));
    };
    match tokio::time::timeout(Duration::from_secs(plazo), hijo.wait_with_output()).await {
        Ok(Ok(o)) => {
            let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                t.push_str("\n--- stderr ---\n");
                t.push_str(&err);
            }
            (false, t)
        }
        Ok(Err(e)) => (false, format!("<fallo esperando al harness: {e}>")),
        // El hijo muere al soltarse el futuro, por `kill_on_drop` de arriba.
        Err(_) => (
            true,
            String::from("<expiro el plazo: sin salida capturada>"),
        ),
    }
}

/// Guarda la salida de una repeticion que no resolvio, para poder mirarla.
///
/// Solo las que NO resuelven: una corrida con `n` alto que resuelve casi todo
/// dejaria decenas de ficheros sin nada que contar.
fn guardar_rastro(dir: &Path, i: usize, v: Veredicto, salida: &str) -> Option<PathBuf> {
    if v == Veredicto::Resuelto {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    let ruta = dir.join(format!(
        "rep-{:02}-{}.txt",
        i + 1,
        v.etiqueta().replace(' ', "-")
    ));
    std::fs::write(&ruta, salida).ok()?;
    Some(ruta)
}

#[tokio::main]
async fn main() {
    let tarea = PathBuf::from(var("CORREDOR_TAREA", "tareas/reparar-tarifa"));
    let modelo = var("CORREDOR_MODELO", "qwen3:14b-nothink");
    let puerto = var("CORREDOR_PUERTO", "8899");
    let encargo = var("CORREDOR_ENCARGO", ENCARGO);
    let wire = var("CORREDOR_WIRE", "responses");
    let nombre_h = var("CORREDOR_HARNESS", "pi");
    let Some(harness) = Harness::desde(&nombre_h) else {
        eprintln!("ABORTA: harness `{nombre_h}` desconocido. Validos: pi, codex.");
        std::process::exit(1);
    };
    let modo_peaje = var("CORREDOR_MODO", "corrida") == "peaje";
    let nivel: u8 = var("CORREDOR_NIVEL", "1").parse().unwrap_or(1);
    let proveedor = var("CORREDOR_PROVEEDOR", "");
    // Topes de CUOTA, no de dolares: los dos harnesses hablan por OAuth de
    // suscripcion, asi que lo que se gasta no es una factura sino cuota.
    let tope_peticiones: usize = var("CORREDOR_TOPE_PETICIONES", "0").parse().unwrap_or(0);
    let tope_tokens: u64 = var("CORREDOR_TOPE_TOKENS", "0").parse().unwrap_or(0);
    let datos = var("CORREDOR_DATOS", "./datos-corredor.jsonl");
    let n: usize = var("CORREDOR_N", "3").parse().unwrap_or(3);
    let plazo: u64 = var("CORREDOR_TIMEOUT", "300").parse().unwrap_or(300);

    println!(
        "corredor del nivel 1 — {} contra {modelo}",
        harness.binario()
    );
    println!("  encargo: {encargo:?}");
    print!("  n={n}, timeout={plazo}s, proxy=127.0.0.1:{puerto}");
    if harness == Harness::Codex {
        print!(", wire_api={wire}");
    }
    println!("\n");

    // ---- Guardas. Ninguna se salta, y todas abortan sin publicar nada. ----

    // LAS GUARDAS DEL NIVEL 2 VAN LAS PRIMERAS, antes de cualquier cosa que
    // pueda gastar cuota. Son comprobaciones sobre la INTENCION de quien lanza
    // y no cuestan nada; ponerlas detras de una guarda de red las hacia
    // inalcanzables — comprobado: nunca se ejecutaban.
    if nivel == 2 {
        if proveedor.is_empty() {
            eprintln!("ABORTA: el nivel 2 necesita CORREDOR_PROVEEDOR (que credencial usar).");
            eprintln!("  No se elige por defecto a proposito: copiar una credencial es una");
            eprintln!("  decision, no un descuido.");
            std::process::exit(1);
        }
        if tope_peticiones == 0 && tope_tokens == 0 {
            eprintln!("ABORTA: el nivel 2 gasta CUOTA REAL y no lleva tope.");
            eprintln!("  Pon CORREDOR_TOPE_PETICIONES y/o CORREDOR_TOPE_TOKENS. #123 recuerda");
            eprintln!("  el precedente: 16.185 tokens quemados sin capturar nada.");
            std::process::exit(1);
        }
        println!(
            "  NIVEL 2 — gasta cuota real. Topes: {tope_peticiones} peticiones, {tope_tokens} tokens"
        );
        println!(
            "  aislamiento DEGRADADO: se copia la credencial de `{proveedor}` y el plugin de enrutado"
        );
    }

    let Some(version) = version_harness(harness) else {
        eprintln!(
            "ABORTA: `{}` no esta instalado o no responde a --version.",
            harness.binario()
        );
        eprintln!("  Sin version la medicion no se puede auditar (banco-de-captura §6.3).");
        std::process::exit(1);
    };
    println!("  harness: {version}");

    if modo_peaje {
        // El modo peaje NO toca la tarea, asi que sus guardas no aplican: no
        // hay estado inicial que tenga que fallar ni verificador que correr.
        // Las del proxy y el contexto SI, y siguen mas abajo.
        println!("  modo: PEAJE (la ceremonia sin trabajo)");
    } else if !tarea.join(FUENTE).exists() || !tarea.join(TESTS).exists() {
        eprintln!("ABORTA: no encuentro la tarea en {}.", tarea.display());
        std::process::exit(1);
    }

    // Regla 1: el estado inicial TIENE que fallar. Un banco que pasa de salida
    // invalida todo lo medido con el. En modo peaje no hay tarea, asi que no
    // hay nada que comprobar aqui.
    if !modo_peaje {
        let sonda = std::env::temp_dir().join(format!("corredor-guarda-{}", std::process::id()));
        if let Err(e) = preparar(&tarea, &sonda) {
            eprintln!("ABORTA: no puedo preparar la copia de guarda: {e}");
            std::process::exit(1);
        }
        let inicial_falla = !tests_pasan(&sonda);
        let _ = std::fs::remove_dir_all(&sonda);
        if !inicial_falla {
            eprintln!("ABORTA: el estado inicial de la tarea PASA.");
            eprintln!("  Una tarea que no falla no mide nada (banco-de-tareas §6.1).");
            std::process::exit(1);
        }
        println!("  estado inicial: falla (correcto)");
    }

    let cliente = reqwest::Client::new();
    if let Err(e) = guarda_proxy(&cliente, &puerto, &modelo).await {
        eprintln!("ABORTA: {e}");
        std::process::exit(1);
    }
    println!("  proxy: enruta a {modelo}");

    // La guarda de contexto manda un prompt ENORME a proposito, para que lo que
    // el proveedor diga haber leido sea el techo efectivo. Contra un modelo
    // local eso es gratis; contra uno DE PAGO seria quemar cuota antes de medir
    // nada — el fallo exacto que el nivel 2 existe para no repetir.
    //
    // Asi que en el nivel 2 no se corre, y el techo se declara desconocido:
    // `usize::MAX` desactiva la deteccion de truncamiento por repeticion. Se
    // pierde esa guarda, y se dice en voz alta.
    let techo = if nivel == 2 {
        println!(
            "  contexto: NO se sondea en el nivel 2 (costaria cuota).\n\
             \x20 Sin deteccion de truncamiento por repeticion.\n"
        );
        usize::MAX
    } else {
        match guarda_contexto(&cliente, &puerto, &modelo).await {
            Ok(t) => {
                println!("  contexto: techo efectivo de {t} tokens\n");
                t
            }
            Err(e) => {
                eprintln!("ABORTA: {e}");
                std::process::exit(1);
            }
        }
    };

    // ---- La corrida ----

    let telemetria = ruta_telemetria();
    let raiz = std::env::temp_dir().join(format!("corredor-{}", std::process::id()));
    // Los rastros sobreviven a la corrida a proposito: son lo que se mira
    // cuando el resultado no se explica solo.
    let rastros = PathBuf::from(var("CORREDOR_RASTROS", "./rastros-corredor"));
    let mut veredictos: Vec<Veredicto> = Vec::with_capacity(n);
    let mut peticiones_totales = 0usize;
    let mut tokens_totales = 0u64;
    // (bytes mandados, peticiones) por repeticion. Es lo que el informe de #122
    // convierte en medianas y rangos.
    let mut medidas: Vec<(u64, usize)> = Vec::with_capacity(n);
    // `None` mientras ningun lote traiga el dato: la ruta de Codex no lo
    // extrae, y un cero ahi seria una afirmacion que nadie ha medido.
    let mut llamadas_totales: Option<usize> = None;

    for i in 0..n {
        let trabajo = raiz.join(format!("rep-{i}"));
        let hogar = raiz.join(format!("home-{i}"));

        let preparado = if modo_peaje {
            preparar_peaje(&trabajo)
        } else {
            preparar(&tarea, &trabajo)
        };
        if let Err(e) = preparado {
            eprintln!("  rep {i}: no puedo preparar el directorio: {e}");
            std::process::exit(1);
        }
        if let Err(e) = escribir_config(harness, &hogar, &puerto, &modelo, &wire) {
            eprintln!("  rep {i}: no puedo escribir la config aislada: {e}");
            std::process::exit(1);
        }
        if nivel == 2 {
            if let Err(e) = copiar_credencial(harness, &hogar, &proveedor) {
                eprintln!("  rep {i}: {e}");
                std::process::exit(1);
            }
            if let Err(e) = copiar_plugin(harness, &hogar) {
                eprintln!("  rep {i}: {e}");
                std::process::exit(1);
            }
        }

        let hash_fuente = hash_fichero(&trabajo.join(FUENTE));
        let hash_tests = hash_fichero(&trabajo.join(TESTS));
        let marca = filas_telemetria(&telemetria);

        let encargo_rep = if modo_peaje {
            ENCARGO_PEAJE
        } else {
            encargo.as_str()
        };
        let (expiro, salida) = lanzar(
            harness,
            &trabajo,
            &hogar,
            &modelo,
            encargo_rep,
            plazo,
            nivel,
            &puerto,
        )
        .await;

        // La telemetria se escribe fuera del camino critico: se le da margen.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let filas = filas_nuevas(&telemetria, marca, &modelo);
        let llamadas = tool_calls(&filas);

        // En modo peaje no hay tarea que resolver: el unico fallo posible es
        // del banco, y `SinPeticiones` lo cubre. Clasificar por el verificador
        // ahi daria «no resuelto» en el 100% de los casos y ensuciaria la
        // lectura de una medida que no va de resolver nada.
        let v = if modo_peaje {
            if filas.is_empty() {
                Veredicto::SinPeticiones
            } else {
                Veredicto::Resuelto
            }
        } else {
            clasificar(
                hash_fichero(&trabajo.join(TESTS)) != hash_tests,
                filas.len(),
                alguna_truncada(&filas, techo),
                hash_fichero(&trabajo.join(FUENTE)) != hash_fuente,
                expiro,
                tests_pasan(&trabajo),
            )
        };

        let bytes = bytes_mandados(&filas);
        anotar_medida(
            &datos,
            harness,
            &version,
            &modelo,
            modo_peaje,
            i,
            v,
            filas.len(),
            bytes,
        );
        medidas.push((bytes, filas.len()));

        let rastro = guardar_rastro(&rastros, i, v, &salida);

        // CORTE DURO POR CUOTA, comprobado DESPUES de cada repeticion. Es lo
        // unico que impide que un harness desbocado se coma la cuota entera:
        // el plugin de enrutado es FAIL-OPEN, asi que si algo va mal el
        // trafico sigue saliendo — pero al menos se para aqui.
        peticiones_totales += filas.len();
        tokens_totales += filas
            .iter()
            .filter_map(|f| f["input_tokens"].as_u64())
            .sum::<u64>();
        if let Some(c) = llamadas {
            llamadas_totales = Some(llamadas_totales.unwrap_or(0) + c);
        }
        println!(
            "  rep {:>2}: {:<24} {:>3} peticiones, {:>9} B, {:>4} tool_calls{}",
            i + 1,
            v.etiqueta(),
            filas.len(),
            bytes,
            fmt_calls(llamadas),
            rastro.map_or(String::new(), |r| format!("  -> {}", r.display()))
        );
        veredictos.push(v);

        // LA GUARDA QUE EVITA EL PRECEDENTE DE #123. El plugin de enrutado es
        // FAIL-OPEN a proposito: si OxideGate no esta arriba, el trafico va
        // DIRECTO al backend real. Eso significa que un fallo de apuntado no
        // da error — gasta cuota y no captura nada, que es exactamente lo que
        // le paso a la captura de #62 (16.185 tokens quemados).
        //
        // Se comprueba tras la PRIMERA repeticion y se aborta: una repeticion
        // perdida es el precio minimo posible por descubrirlo.
        if nivel == 2 && i == 0 && filas.is_empty() {
            eprintln!("\nABORTA: la primera repeticion no dejo NI UNA fila de telemetria.");
            eprintln!("  El harness gasto cuota y el proxy no vio nada, asi que el enrutado");
            eprintln!("  NO esta funcionando. El plugin es fail-open: no da error, solo");
            eprintln!("  deja de medir.");
            eprintln!("  Comprueba que OxideGate escucha en {puerto} y que el plugin apunta");
            eprintln!("  ahi (OXIDEGATE_URL). Se para tras UNA repeticion, no tras {n}.");
            std::process::exit(1);
        }

        if nivel == 2 {
            let pasado = (tope_peticiones > 0 && peticiones_totales >= tope_peticiones)
                || (tope_tokens > 0 && tokens_totales >= tope_tokens);
            if pasado && i + 1 < n {
                println!(
                    "\n  TOPE DE CUOTA ALCANZADO tras {} repeticiones: {peticiones_totales} \
                     peticiones, {tokens_totales} tokens.\n  Se para aqui: lo medido vale, \
                     lo que falta no se gasta.",
                    i + 1
                );
                break;
            }
        }
    }

    let _ = std::fs::remove_dir_all(&raiz);
    reportar(
        harness,
        &medidas,
        &veredictos,
        &version,
        &modelo,
        &encargo,
        peticiones_totales,
        llamadas_totales,
    );
}

/// Mediana y rango de una muestra. `None` si está vacía.
///
/// **Nunca solo la media**: #29 lo exige —«n>1 por herramienta, con los rangos
/// publicados, no solo la media»— y con razón. Una media esconde exactamente lo
/// que hace falta ver: si dos harnesses con la misma media tienen rangos
/// distintos, no cuestan lo mismo.
///
/// Mediana y no media también a propósito: una sola repetición que se fue por
/// las ramas arrastra la media y deja de describir el caso típico.
fn mediana_y_rango(v: &[u64]) -> Option<(u64, u64, u64)> {
    if v.is_empty() {
        return None;
    }
    let mut o = v.to_vec();
    o.sort_unstable();
    Some((o[o.len() / 2], o[0], o[o.len() - 1]))
}

/// Anota una repetición en el fichero de datos, una línea JSON por repetición.
///
/// Existe porque el informe de
/// [#122](https://github.com/pichu2707/OxideGate/issues/122) necesita **la
/// distribución**, no el resumen: medianas y rangos, y eso no se puede
/// reconstruir de una media impresa por pantalla. Y porque la tabla cruza
/// harnesses distintos, que se miden en corridas distintas.
///
/// Se **añade**, no se sobrescribe: dos corridas del mismo harness son más
/// datos, no un reemplazo. Un fallo al escribir avisa y NO aborta — la medición
/// ya está hecha y perderla por un disco lleno sería peor.
#[allow(clippy::too_many_arguments)]
fn anotar_medida(
    ruta: &str,
    h: Harness,
    version: &str,
    modelo: &str,
    peaje: bool,
    rep: usize,
    v: Veredicto,
    peticiones: usize,
    bytes: u64,
) {
    let linea = serde_json::json!({
        "harness": h.binario(),
        "version": version,
        "modelo": modelo,
        "modo": if peaje { "peaje" } else { "corrida" },
        "rep": rep,
        "veredicto": v.etiqueta(),
        "resuelto": v == Veredicto::Resuelto,
        "fallo_del_banco": v.es_fallo_del_banco(),
        "peticiones": peticiones,
        "bytes": bytes,
    });
    let escrito = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ruta)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{linea}")
        });
    if let Err(e) = escrito {
        eprintln!("  AVISO: no pude anotar la medida en {ruta}: {e}");
    }
}

/// Publica el resultado **sin filtrar**: lo que no resuelve se cuenta y se
/// publica (regla 5 del banco).
#[allow(clippy::too_many_arguments)]
fn reportar(
    harness: Harness,
    medidas: &[(u64, usize)],
    veredictos: &[Veredicto],
    version: &str,
    modelo: &str,
    encargo: &str,
    peticiones: usize,
    llamadas: Option<usize>,
) {
    let n = veredictos.len();
    let cuenta = |v: Veredicto| veredictos.iter().filter(|x| **x == v).count();
    let resueltos = cuenta(Veredicto::Resuelto);
    let del_banco = veredictos.iter().filter(|v| v.es_fallo_del_banco()).count();

    println!("\n─── resultado ───");
    println!("  harness  : {} {version}", harness.binario());
    println!("  modelo   : {modelo}");
    println!("  encargo  : {encargo:?}");
    println!("  resuelto : {resueltos}/{n}");
    println!(
        "  trafico  : {peticiones} peticiones, {} tool_calls",
        fmt_calls(llamadas)
    );

    // La DISTRIBUCION, no el resumen. Ver `mediana_y_rango`.
    let bytes: Vec<u64> = medidas.iter().map(|(b, _)| *b).collect();
    let turnos: Vec<u64> = medidas.iter().map(|(_, t)| *t as u64).collect();
    if let (Some((mb, lob, hib)), Some((mt, lot, hit))) =
        (mediana_y_rango(&bytes), mediana_y_rango(&turnos))
    {
        println!("  bytes/rep: mediana {mb}  rango {lob}-{hib}");
        println!("  turnos   : mediana {mt}  rango {lot}-{hit}");
    }
    println!();

    for v in [
        Veredicto::Resuelto,
        Veredicto::NoResuelto,
        Veredicto::NoToco,
        Veredicto::Timeout,
        Veredicto::SinPeticiones,
        Veredicto::TestsAlterados,
        Veredicto::PromptTruncado,
    ] {
        let c = cuenta(v);
        if c > 0 {
            println!("    {:<24} {c}", v.etiqueta());
        }
    }

    if del_banco > 0 {
        println!(
            "\n  AVISO: {del_banco}/{n} son fallos del BANCO, no del modelo. La tasa de\n\
             \x20 arriba no es una tasa de capacidad hasta que eso este en cero."
        );
    }
    if llamadas.is_none() && peticiones > 0 {
        println!(
            "\n  NOTA: `tool_calls` sale n/d porque el proveedor de esta ruta no extrae\n\
             \x20 invocaciones (`captura_invocaciones() -> false` en los dos dialectos de\n\
             \x20 OpenAI). NO significa que el modelo no llamara a nada: significa que\n\
             \x20 este banco no lo mide. Para saberlo hace falta un extractor, no una\n\
             \x20 lectura optimista de un null."
        );
    }

    let no_toco = cuenta(Veredicto::NoToco);
    if resueltos == 0 && no_toco == n && n > 0 {
        println!(
            "\n  AVISO: el harness llego al modelo en las {n} repeticiones y no edito el\n\
             \x20 fichero en ninguna. Es el sintoma que hundio el primer intento de #121.\n\
             \x20 Antes de concluir nada del modelo, descarta el banco:\n\
             \x20 1) la guarda de contexto paso, asi que el prompt NO llego cortado;\n\
             \x20 2) contraprueba de redaccion:\n\
             \x20    CORREDOR_ENCARGO=\"Averigua por que falla test_tarifa.py y corrigelo.\"\n\
             \x20 3) y sobre todo: LEE LOS RASTROS. La salida del harness esta guardada."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- clasificar: cada culpable, el suyo ----

    #[test]
    fn los_tests_pasan_y_no_se_tocaron_es_resuelto() {
        assert_eq!(
            clasificar(false, 4, false, true, false, true),
            Veredicto::Resuelto
        );
    }

    #[test]
    fn reescribir_el_verificador_anula_la_repeticion_aunque_pase() {
        // El caso peligroso: los tests PASAN, pero pasan porque los reescribio.
        // Si esta guarda se mira despues de `tests_pasan`, cuenta como exito.
        assert_eq!(
            clasificar(true, 4, false, true, false, true),
            Veredicto::TestsAlterados
        );
    }

    #[test]
    fn sin_peticiones_es_fallo_del_banco_y_no_del_modelo() {
        assert_eq!(
            clasificar(false, 0, false, false, false, false),
            Veredicto::SinPeticiones
        );
        assert!(Veredicto::SinPeticiones.es_fallo_del_banco());
    }

    #[test]
    fn sin_peticiones_gana_al_timeout_porque_lo_roto_es_el_cableado() {
        assert_eq!(
            clasificar(false, 0, false, false, true, false),
            Veredicto::SinPeticiones
        );
    }

    #[test]
    fn mando_peticiones_y_no_toco_el_fichero_se_cuenta_aparte() {
        // El sintoma exacto que hundio el primer intento de #121.
        assert_eq!(
            clasificar(false, 6, false, false, false, false),
            Veredicto::NoToco
        );
        assert!(!Veredicto::NoToco.es_fallo_del_banco());
    }

    #[test]
    fn edito_y_siguen_fallando_es_un_fallo_legitimo_del_modelo() {
        assert_eq!(
            clasificar(false, 6, false, true, false, false),
            Veredicto::NoResuelto
        );
        assert!(!Veredicto::NoResuelto.es_fallo_del_banco());
    }

    #[test]
    fn un_timeout_que_alcanzo_a_resolver_cuenta_como_resuelto() {
        // El plazo es del corredor, no de la tarea: si los tests pasan, paso.
        assert_eq!(
            clasificar(false, 3, false, true, true, true),
            Veredicto::Resuelto
        );
    }

    #[test]
    fn un_timeout_sin_resolver_no_se_confunde_con_no_saber() {
        assert_eq!(
            clasificar(false, 3, false, true, true, false),
            Veredicto::Timeout
        );
    }

    // ---- la config aislada ----

    #[test]
    fn la_config_de_codex_apunta_al_proxy_y_habla_responses() {
        let c = config_codex("8899", "qwen3:14b-nothink", "responses");
        assert!(c.contains("base_url = \"http://127.0.0.1:8899/v1\""));
        assert!(c.contains("wire_api = \"responses\""));
        assert!(c.contains("model = \"qwen3:14b-nothink\""));
    }

    #[test]
    fn el_dialecto_es_una_variable_y_llega_a_la_config() {
        // Si el wire_api se quedara fijo, la contraprueba que separa «el modelo
        // no llama» de «el harness no sabe enrutar la llamada» no se podria
        // correr — y es la que resolvio el 0/3.
        assert!(config_codex("8899", "m", "chat").contains("wire_api = \"chat\""));
        assert!(config_codex("8899", "m", "responses").contains("wire_api = \"responses\""));
    }

    #[test]
    fn la_config_de_pi_apunta_al_proxy_y_declara_el_dialecto_validado() {
        let c = config_pi("8901", "qwen3:14b-nothink");
        assert!(c.contains("\"baseUrl\": \"http://127.0.0.1:8901/v1\""));
        // `openai-completions` es el dialecto que la sonda valido a 30/30, y el
        // que Codex NO puede usar (wire_api = "chat" esta eliminado en 0.142.5).
        assert!(c.contains("\"api\": \"openai-completions\""));
        assert!(c.contains("\"id\": \"qwen3:14b-nothink\""));
    }

    #[test]
    fn la_config_de_pi_es_json_valido() {
        // Se construye con format!, asi que una coma de mas no la caza el
        // compilador: la caza esto. Un JSON roto deja a `pi` con su proveedor
        // por defecto —la nube— sin dar error.
        let c = config_pi("8901", "m");
        let v: Value = serde_json::from_str(&c).expect("config_pi no es JSON valido");
        assert_eq!(
            v["providers"]["oxidegate"]["baseUrl"],
            "http://127.0.0.1:8901/v1"
        );
    }

    #[test]
    fn la_config_de_pi_no_lleva_credenciales_reales() {
        let c = config_pi("8901", "m").to_lowercase();
        for veneno in ["sk-", "bearer", "token"] {
            assert!(!c.contains(veneno), "la config de pi filtra `{veneno}`");
        }
    }

    #[test]
    fn la_config_de_opencode_es_json_valido_y_apunta_al_proxy() {
        let c = config_opencode("8901", "qwen3:14b-nothink");
        let v: Value = serde_json::from_str(&c).expect("config_opencode no es JSON valido");
        assert_eq!(
            v["provider"]["oxidegate"]["options"]["baseURL"],
            "http://127.0.0.1:8901/v1"
        );
        assert_eq!(v["model"], "oxidegate/qwen3:14b-nothink");
    }

    #[test]
    fn opencode_arranca_con_los_permisos_abiertos() {
        // Sin esto pide confirmacion, en modo no interactivo no hace nada, y la
        // repeticion se contaria como «no toco el fichero» CULPANDO AL MODELO de
        // una config del banco.
        let v: Value = serde_json::from_str(&config_opencode("8901", "m")).unwrap();
        assert_eq!(v["permission"]["edit"], "allow");
        assert_eq!(v["permission"]["bash"], "allow");
        // Y la red hacia fuera cerrada: el banco es de coste cero.
        assert_eq!(v["permission"]["webfetch"], "deny");
    }

    #[test]
    fn la_config_de_opencode_no_lleva_credenciales_reales() {
        let c = config_opencode("8901", "m").to_lowercase();
        for veneno in ["sk-", "bearer", "token"] {
            assert!(
                !c.contains(veneno),
                "la config de opencode filtra `{veneno}`"
            );
        }
    }

    /// La garantia del nivel 2 no es «ninguna credencial» sino «SOLO la
    /// nombrada». Este test la fija: el auth real de opencode lleva cuatro
    /// proveedores, y llevarse las cuatro a un temporal por comodidad seria
    /// regalar tres que no hacen falta.
    #[test]
    fn se_copia_UNA_credencial_y_no_el_fichero_entero() {
        let raiz = std::env::temp_dir().join(format!("cred-{}", std::process::id()));
        let falso_home = raiz.join("home-real");
        let aislado = raiz.join("aislado");
        let rel = Harness::Opencode.ruta_credencial().unwrap();
        std::fs::create_dir_all(falso_home.join(rel).parent().unwrap()).unwrap();
        std::fs::write(
            falso_home.join(rel),
            r#"{"openai":{"type":"oauth","access":"AAA"},
                "google":{"type":"oauth","access":"BBB"},
                "anthropic":{"type":"oauth","access":"CCC"}}"#,
        )
        .unwrap();

        // `copiar_credencial` lee de $HOME; se apunta al falso.
        let previo = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &falso_home) };
        let r = copiar_credencial(Harness::Opencode, &aislado, "openai");
        if let Some(h) = previo {
            unsafe { std::env::set_var("HOME", h) };
        }
        r.expect("deberia copiar");

        let copiado = std::fs::read_to_string(aislado.join(rel)).unwrap();
        assert!(copiado.contains("openai"), "falta la que se pidio");
        assert!(!copiado.contains("google"), "se colo `google`");
        assert!(!copiado.contains("anthropic"), "se colo `anthropic`");
        assert!(!copiado.contains("BBB") && !copiado.contains("CCC"));
        let _ = std::fs::remove_dir_all(&raiz);
    }

    /// Un proveedor que no esta se reporta SIN imprimir lo que si habia: el
    /// mensaje de error de un fichero de credenciales no puede ser un volcado.
    #[test]
    fn un_proveedor_ausente_no_filtra_los_valores_de_los_demas() {
        let raiz = std::env::temp_dir().join(format!("cred-no-{}", std::process::id()));
        let falso_home = raiz.join("home-real");
        let rel = Harness::Opencode.ruta_credencial().unwrap();
        std::fs::create_dir_all(falso_home.join(rel).parent().unwrap()).unwrap();
        std::fs::write(
            falso_home.join(rel),
            r#"{"google":{"type":"oauth","access":"SECRETO-QUE-NO-DEBE-SALIR"}}"#,
        )
        .unwrap();

        let previo = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &falso_home) };
        let e = copiar_credencial(Harness::Opencode, &raiz.join("aislado"), "openai")
            .expect_err("no esta, tiene que fallar");
        if let Some(h) = previo {
            unsafe { std::env::set_var("HOME", h) };
        }
        assert!(e.contains("openai") && e.contains("google"), "{e}");
        assert!(
            !e.contains("SECRETO-QUE-NO-DEBE-SALIR"),
            "el error filtra el valor de la credencial: {e}"
        );
        let _ = std::fs::remove_dir_all(&raiz);
    }

    /// El plugin es FAIL-OPEN: si falta, el harness va directo al upstream y
    /// gasta cuota sin capturar nada. Faltar tiene que ser un ERROR, no un
    /// silencio.
    #[test]
    fn un_plugin_de_enrutado_ausente_es_un_error_y_no_un_silencio() {
        let raiz = std::env::temp_dir().join(format!("plug-{}", std::process::id()));
        let previo = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", raiz.join("home-vacio")) };
        let e = copiar_plugin(Harness::Opencode, &raiz.join("aislado"))
            .expect_err("sin plugin tiene que fallar");
        if let Some(h) = previo {
            unsafe { std::env::set_var("HOME", h) };
        }
        assert!(
            e.contains("cuota"),
            "el error tiene que decir QUE se arriesga: {e}"
        );
        // Y un harness que no necesita plugin no falla por no tenerlo.
        assert!(copiar_plugin(Harness::Pi, &raiz.join("aislado")).is_ok());
        let _ = std::fs::remove_dir_all(&raiz);
    }

    #[test]
    fn cada_harness_declara_donde_vive_su_credencial() {
        for h in [Harness::Pi, Harness::Opencode, Harness::Codex] {
            let r = h.ruta_credencial().expect("todos la declaran");
            assert!(!r.starts_with('/'), "tiene que ser relativa a HOME: {r}");
        }
        // Solo opencode necesita plugin de enrutado.
        assert!(Harness::Opencode.ruta_plugin().is_some());
        assert!(Harness::Pi.ruta_plugin().is_none());
    }

    #[test]
    fn cada_harness_tiene_su_binario_y_se_resuelve_por_nombre() {
        assert_eq!(Harness::desde("pi"), Some(Harness::Pi));
        assert_eq!(Harness::desde("opencode"), Some(Harness::Opencode));
        assert_eq!(Harness::desde("codex"), Some(Harness::Codex));
        assert_eq!(Harness::desde("claude"), None);
        assert_eq!(Harness::Pi.binario(), "pi");
        assert_eq!(Harness::Opencode.binario(), "opencode");
        assert_eq!(Harness::Codex.binario(), "codex");
    }

    /// `env_clear()` en `std::process::Command` **borra tambien las variables ya
    /// puestas explicitamente** — comprobado con rustc 1.96: una `.env(..)`
    /// antes de `.env_clear()` llega VACIA al hijo.
    ///
    /// Si alguien reordena `lanzar()` y pone el `env_clear` al final, `pi` se
    /// queda sin `PI_OFFLINE` y Codex sin `CODEX_HOME` — y **no da ningun
    /// error**: el harness simplemente busca su config donde no esta y se va con
    /// su proveedor por defecto. Este test fija la semantica que el codigo
    /// asume, para que el dia que cambie se entere alguien.
    #[test]
    fn env_clear_borra_lo_puesto_antes_de_el() {
        use std::process::Command as Std;
        let antes = Std::new("sh")
            .arg("-c")
            .arg("printf %s \"$MARCA\"")
            .env("MARCA", "se-pierde")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .expect("sh no arranco");
        assert_eq!(
            String::from_utf8_lossy(&antes.stdout),
            "",
            "env_clear ya NO borra lo puesto antes: revisar el orden en lanzar()"
        );

        let despues = Std::new("sh")
            .arg("-c")
            .arg("printf %s \"$MARCA\"")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("MARCA", "sobrevive")
            .output()
            .expect("sh no arranco");
        assert_eq!(String::from_utf8_lossy(&despues.stdout), "sobrevive");
    }

    #[test]
    fn la_config_aislada_no_lleva_credenciales() {
        // Regla 6: el peor caso de un apuntado mal hecho tiene que ser un error
        // de auth, nunca una factura.
        let c = config_codex("8899", "qwen3:14b-nothink", "responses").to_lowercase();
        for veneno in ["api_key", "apikey", "auth", "token", "bearer", "sk-"] {
            assert!(!c.contains(veneno), "la config filtra `{veneno}`: {c}");
        }
    }

    #[test]
    fn el_proveedor_declarado_es_el_que_usa_el_modelo() {
        // Un `model_provider` que no case con la seccion declarada deja a Codex
        // hablando con su default, que es la nube: fuga silenciosa.
        let c = config_codex("8899", "m", "responses");
        assert!(c.contains("model_provider = \"oxidegate\""));
        assert!(c.contains("[model_providers.oxidegate]"));
    }

    // ---- el fichero de instrucciones ----

    #[test]
    fn el_agents_md_es_ascii_puro() {
        // Misma condicion que el fixture de la tarea: con acentos, la
        // codificacion entraria como variable en un experimento que mide otra
        // cosa.
        assert!(
            AGENTS_MD.is_ascii(),
            "el AGENTS.md normalizado tiene no-ASCII"
        );
    }

    #[test]
    fn el_agents_md_nombra_el_verificador_y_lo_protege() {
        assert!(AGENTS_MD.contains("python3 test_tarifa.py"));
        assert!(AGENTS_MD.contains("No modifiques `test_tarifa.py`"));
    }

    #[test]
    fn el_encargo_de_fabrica_es_el_del_tarea_md() {
        // Si alguien lo "mejora" a una redaccion de la familia `averigua`, la
        // medicion deja de ser la de la tarea escrita y nadie se entera.
        assert_eq!(ENCARGO, "test_tarifa.py falla. Haz que pase.");
        assert!(ENCARGO.is_ascii());
    }

    // ---- telemetria ----

    #[test]
    fn un_tool_calls_nulo_no_se_lee_como_cero() {
        // `null` = "este proveedor no mide invocaciones", NO "no invoco nada".
        // Los dos dialectos de OpenAI declaran captura_invocaciones() -> false,
        // asi que para Codex esto es siempre el caso.
        let filas = vec![
            serde_json::json!({"model": "m", "tool_calls": null}),
            serde_json::json!({"model": "m"}),
        ];
        assert_eq!(tool_calls(&filas), None);
        assert_eq!(fmt_calls(None), "n/d");
    }

    #[test]
    fn un_tool_calls_medido_y_vacio_si_es_un_cero() {
        // Un extractor que mira y no ve nada SI afirma cero. La diferencia con
        // el test de arriba es la que separa medir de suponer.
        let filas = vec![serde_json::json!({"tool_calls": {"invoked": []}})];
        assert_eq!(tool_calls(&filas), Some(0));
        assert_eq!(fmt_calls(Some(0)), "0");
    }

    #[test]
    fn tool_calls_cuenta_las_invocaciones_no_el_numero_de_filas() {
        // El campo es un objeto `{"invoked": [...]}`, no un entero: leerlo con
        // as_u64() devuelve siempre 0 y el contador miente en silencio.
        let filas = vec![
            serde_json::json!({"tool_calls": {"invoked": [{"name": "a"}, {"name": "b"}]}}),
            serde_json::json!({"tool_calls": {"invoked": [{"name": "c"}]}}),
        ];
        assert_eq!(tool_calls(&filas), Some(3));
    }

    #[test]
    fn un_prompt_truncado_invalida_la_repeticion_aunque_resuelva() {
        // Resolver con el estimulo cortado no es resolver la tarea que se creia
        // estar midiendo.
        assert_eq!(
            clasificar(false, 4, true, true, false, true),
            Veredicto::PromptTruncado
        );
        assert!(Veredicto::PromptTruncado.es_fallo_del_banco());
    }

    #[test]
    fn detecta_la_peticion_pegada_al_techo() {
        // La firma de qwen: 32767 contra un techo de 32768.
        let filas = vec![
            serde_json::json!({"input_tokens": 6485}),
            serde_json::json!({"input_tokens": 32767}),
        ];
        assert!(alguna_truncada(&filas, 32_768));
    }

    #[test]
    fn una_peticion_holgada_no_se_marca_como_truncada() {
        let filas = vec![serde_json::json!({"input_tokens": 6485})];
        assert!(!alguna_truncada(&filas, 32_768));
        // Y sin el dato tampoco se inventa un truncamiento.
        assert!(!alguna_truncada(&[serde_json::json!({})], 32_768));
    }

    #[test]
    fn el_umbral_de_contexto_cubre_el_prompt_real_de_un_harness() {
        // Codex manda ~6500 tokens. Un umbral por debajo dejaria pasar
        // exactamente el truncamiento que esta guarda existe para cazar.
        assert!(
            CONTEXTO_MINIMO > 6_500,
            "el umbral no cubre el prompt medido de Codex"
        );
        // Y por encima del num_ctx por defecto de ollama, que es el culpable.
        assert!(CONTEXTO_MINIMO > 4_096);
    }

    #[test]
    fn las_filas_de_otro_modelo_no_cuentan_como_peticiones_del_harness() {
        let dir = std::env::temp_dir().join(format!("corredor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(
            &p,
            "{\"model\":\"viejo\"}\n\
             {\"model\":\"mio\",\"tool_calls\":2}\n\
             {\"model\":\"otro\",\"tool_calls\":9}\n\
             {\"model\":\"mio\",\"tool_calls\":3}\n",
        )
        .unwrap();

        let filas = filas_nuevas(&p, 1, "mio");
        assert_eq!(filas.len(), 2, "se colo trafico de otro modelo");
        assert_eq!(tool_calls(&filas), None); // ninguna fila trae el objeto
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn una_linea_corrupta_no_tumba_la_lectura() {
        let dir = std::env::temp_dir().join(format!("corredor-corr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(&p, "no soy json\n{\"model\":\"mio\",\"tool_calls\":1}\n").unwrap();
        assert_eq!(filas_nuevas(&p, 0, "mio").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sin_fichero_de_telemetria_no_revienta() {
        let p = std::env::temp_dir().join("corredor-no-existe-jamas.jsonl");
        assert_eq!(filas_telemetria(&p), 0);
        assert!(filas_nuevas(&p, 0, "mio").is_empty());
    }

    // ---- las reglas del banco, como test ----

    #[test]
    fn la_mediana_y_el_rango_describen_la_muestra() {
        assert_eq!(mediana_y_rango(&[5, 1, 9, 3, 7]), Some((5, 1, 9)));
        assert_eq!(mediana_y_rango(&[4]), Some((4, 4, 4)));
        assert_eq!(
            mediana_y_rango(&[]),
            None,
            "una muestra vacia no tiene mediana"
        );
    }

    /// El caso que justifica publicar el rango y no solo la media: dos muestras
    /// con la MISMA media que no dicen lo mismo.
    #[test]
    fn dos_muestras_con_la_misma_media_no_tienen_el_mismo_rango() {
        let estable = [50u64, 50, 50];
        let volatil = [10u64, 50, 90];
        let media = |v: &[u64]| v.iter().sum::<u64>() / v.len() as u64;
        assert_eq!(media(&estable), media(&volatil));
        assert_ne!(mediana_y_rango(&estable), mediana_y_rango(&volatil));
    }

    /// El peaje se mide con el MISMO entorno que las corridas menos la tarea:
    /// lleva el AGENTS.md -que es ceremonia- y no lleva los ficheros.
    #[test]
    fn el_directorio_del_peaje_lleva_las_instrucciones_y_no_la_tarea() {
        let dir = std::env::temp_dir().join(format!("corredor-peaje-{}", std::process::id()));
        preparar_peaje(&dir).unwrap();
        assert!(dir.join("AGENTS.md").exists(), "el AGENTS.md es ceremonia");
        assert!(!dir.join(FUENTE).exists(), "el peaje no lleva la tarea");
        assert!(!dir.join(TESTS).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El encargo del peaje es el MISMO que el de `floor-across-tools.md` §1,
    /// para que las dos mediciones hablen del mismo estimulo.
    #[test]
    fn el_encargo_del_peaje_es_el_del_banco_del_suelo() {
        assert_eq!(ENCARGO_PEAJE, "Responde solo: ok");
        assert!(ENCARGO_PEAJE.is_ascii());
        assert_ne!(ENCARGO_PEAJE, ENCARGO, "el peaje NO puede llevar la tarea");
    }

    #[test]
    fn los_bytes_se_leen_de_lo_MANDADO_no_de_lo_leido() {
        // `input_tokens` es lo que el proveedor dijo haber leido; cuando trunca,
        // los dos divergen. El que dice lo que costo mandar es el otro.
        let filas = vec![
            serde_json::json!({"context_measured_bytes": 1000, "input_tokens": 4095}),
            serde_json::json!({"context_measured_bytes": 2500, "input_tokens": 4095}),
            serde_json::json!({"input_tokens": 900}),
        ];
        assert_eq!(bytes_mandados(&filas), 3500);
    }

    #[test]
    fn los_fallos_del_banco_son_exactamente_tres() {
        // Si alguien anade una variante nueva, tiene que decidir a que lado
        // cae. Este test le obliga a mirarlo.
        let todos = [
            Veredicto::Resuelto,
            Veredicto::NoResuelto,
            Veredicto::NoToco,
            Veredicto::SinPeticiones,
            Veredicto::TestsAlterados,
            Veredicto::PromptTruncado,
            Veredicto::Timeout,
        ];
        assert_eq!(todos.iter().filter(|v| v.es_fallo_del_banco()).count(), 3);
    }

    #[test]
    fn cada_veredicto_tiene_su_etiqueta_y_ninguna_se_repite() {
        let todos = [
            Veredicto::Resuelto,
            Veredicto::NoResuelto,
            Veredicto::NoToco,
            Veredicto::SinPeticiones,
            Veredicto::TestsAlterados,
            Veredicto::PromptTruncado,
            Veredicto::Timeout,
        ];
        let mut etiquetas: Vec<&str> = todos.iter().map(|v| v.etiqueta()).collect();
        etiquetas.sort_unstable();
        let antes = etiquetas.len();
        etiquetas.dedup();
        assert_eq!(etiquetas.len(), antes, "dos veredictos comparten etiqueta");
    }
}
