//! Energía por petición: lo que la máquina consumió mientras el request
//! estuvo abierto.
//!
//! Cierra la simetría que `pricing::estimate_cost_usd` deja abierta. Contra un
//! modelo de nube el proxy sabe exactamente lo que cuesta una petición; contra
//! `ollama` en tu propia máquina no sabía **nada**, y sin embargo se paga: se
//! paga en electricidad.
//!
//! # Lo que este módulo NO afirma
//!
//! **No dice «esta petición gastó tanto».** Dice «la máquina gastó tanto
//! MIENTRAS esta petición estaba abierta», que no es lo mismo y no se puede
//! convertir en lo mismo con los datos que hay.
//!
//! Si dos peticiones se solapan, **las dos integran los mismos vatios y las
//! dos los reclaman**. Sumar `energy_wh` sobre varias filas daría más energía
//! de la que la máquina consumió de verdad. La suma sobre filas solapadas es
//! **inválida**, y hay un test que la fija (`dos_ventanas_solapadas_reclaman_
//! la_misma_energia`) para que la propiedad quede escrita y no descubierta.
//!
//! Es la misma trampa que `docs/fixed-toll-claude-code.md` §4 documenta como
//! «leer los bytes, no restarlos»: mezclar dos medidas tomadas en puntos
//! distintos y presentar el resultado como si fuera una sola.
//!
//! Por eso se publica el **reposo al lado** de la energía bruta en vez de un
//! único número ya restado. La resta la hace quien lee, viendo lo que resta.
//!
//! # Y tampoco convierte a dinero
//!
//! Se publica la energía. El precio del kWh lo pone quien lee: cambia por
//! país, por contrato y por hora del día. Un euro impreso aquí sería falso en
//! cuanto cambiara la tarifa, y nadie volvería a mirarlo.
//!
//! # Por qué el muestreador vive en el proxy
//!
//! Porque el proxy es el único que sabe **cuándo empieza y acaba** cada
//! petición. El monitor puede enseñar la aguja en vivo —y la enseña, panel
//! `g`— pero no puede atribuir.
//!
//! El coste no lo prohíbe. Medido en esta máquina: arrancar `nvidia-smi`
//! cuesta **23,81 ms**, seis veces todo el overhead del proxy. Pero eso es el
//! coste de ARRANCAR, no el de leer. Un proceso **persistente** con `-lms 200`
//! paga ese arranque UNA vez y luego escupe una muestra cada 200 ms por
//! **0,1% de un core** (50 muestras en 10 s, medido). Leer el anillo dentro de
//! `emit` cuesta microsegundos.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Milisegundos entre muestras del proceso persistente.
///
/// 200 ms es un compromiso medido: a esa cadencia una petición corta de 1 s ya
/// cae sobre cinco muestras —suficiente para que la integración sea una curva
/// y no un rectángulo— y el proceso sigue costando 0,1% de un core.
pub const CADENCIA_MS: u64 = 200;

/// Cuánto se tolera que el muestreador vaya por detrás del cierre de la
/// petición, en milisegundos.
///
/// La ventana acaba en `Instant::now()` y la muestra más nueva es SIEMPRE del
/// pasado: como mucho una cadencia atrás. Exigir cobertura estricta hasta el
/// final dejaría el campo en `None` en casi todas las peticiones — se
/// comprobó midiendo, no razonando.
///
/// Así que la cola se mantiene al último valor leído (`zero-order hold`), y
/// eso es una EXTRAPOLACIÓN: pequeña, acotada y declarada. Dos cadencias de
/// margen absorben el jitter del proceso; más allá, el muestreador se quedó
/// atrás de verdad y el campo vuelve a ser `None`.
pub const TOLERANCIA_COLA_MS: u64 = CADENCIA_MS * 2;

/// Muestras que se guardan: 6 minutos a [`CADENCIA_MS`].
///
/// El anillo tiene que sobrevivir a la petición más larga que se quiera medir,
/// porque una ventana que empieza antes de la muestra más vieja **no se
/// publica** (ver [`PowerRing::ventana`]). Seis minutos cubre con holgura
/// cualquier respuesta de un modelo local.
pub const CAPACIDAD_ANILLO: usize = 1_800;

/// Lo que la tarjeta estaba consumiendo en un instante concreto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerSample {
    pub at: Instant,
    pub vatios: f64,
}

/// Resultado de integrar el consumo sobre la ventana de una petición.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyWindow {
    /// Energía BRUTA: el área bajo la curva de potencia, reposo incluido.
    pub wh: f64,
    /// Lo que la máquina habría gastado en reposo durante esa misma ventana.
    /// Restarlo de [`Self::wh`] da lo atribuible al trabajo — y se publica
    /// aparte justamente para que la resta sea visible.
    pub idle_wh: f64,
    /// Pico de potencia dentro de la ventana. Es la cifra que se compara con
    /// el límite de la tarjeta y con el ruido del ventilador; la energía es la
    /// que se compara con la factura.
    pub peak_w: f64,
    /// Muestras REALES que cayeron dentro de la ventana.
    ///
    /// Sin este número no se distingue una curva de un rectángulo: con `0`, la
    /// energía sale de interpolar entre dos muestras de fuera, que es una
    /// estimación honesta pero mucho más basta. Se publica para que quien lee
    /// pueda decidir si se fía.
    pub samples: u32,
}

/// Lee una línea de `nvidia-smi --query-gpu=power.draw --format=...,nounits`.
///
/// Función PURA: el CI no tiene GPU, igual que no tiene ollama delante.
///
/// Devuelve `None` ante cualquier cosa que no sea un número de vatios
/// plausible. **Nunca rellena con cero**: `nvidia-smi` escupe `[N/A]` o
/// `[Not Supported]` en tarjetas que no exponen el sensor, y tomar eso por
/// `0 W` publicaría «esta petición fue gratis» donde lo cierto es «no lo sé».
pub fn parse_vatios(linea: &str) -> Option<f64> {
    let v: f64 = linea.trim().parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some(v)
}

/// Extrae el host de una URL sin traer una dependencia solo para esto.
///
/// Devuelve `None` si no hay autoridad que leer. Maneja userinfo (`user@host`),
/// puerto, y la forma IPv6 entre corchetes — que sin tratar aparte convertiría
/// `[::1]:11434` en `[` al cortar por el primer `:`.
pub fn host_de_url(url: &str) -> Option<&str> {
    let sin_esquema = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let autoridad = sin_esquema.split(['/', '?', '#']).next()?;
    let autoridad = autoridad
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(autoridad);
    if autoridad.is_empty() {
        return None;
    }
    if let Some(resto) = autoridad.strip_prefix('[') {
        return resto.split_once(']').map(|(h, _)| h);
    }
    autoridad.split(':').next()
}

/// `true` si la URL destino apunta a esta misma máquina.
///
/// Es la guarda del contrato: muestrear tu GPU mientras responde Anthropic
/// mide **tu escritorio**, no la inferencia. Con upstream remoto los campos de
/// energía valen `None`, que es la respuesta correcta, en vez de un número que
/// parece significar algo.
///
/// Se decide por el HOST parseado, no por `contains`: `localhost.ejemplo.com`
/// es un dominio remoto perfectamente válido y contiene la palabra.
pub fn es_upstream_local(url: &str) -> bool {
    let Some(host) = host_de_url(url) else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => false,
    }
}

/// Historial reciente de potencia, en orden cronológico.
#[derive(Debug)]
pub struct PowerRing {
    muestras: VecDeque<PowerSample>,
    cap: usize,
}

impl PowerRing {
    pub fn new() -> Self {
        Self::con_capacidad(CAPACIDAD_ANILLO)
    }

    pub fn con_capacidad(cap: usize) -> Self {
        Self {
            muestras: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, m: PowerSample) {
        self.muestras.push_back(m);
        while self.muestras.len() > self.cap {
            self.muestras.pop_front();
        }
    }

    /// Solo para los tests: en produccion nadie pregunta el tamano del
    /// anillo, se pregunta por una ventana. Exponerlo fuera de `cfg(test)`
    /// seria API muerta.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.muestras.len()
    }

    /// El reposo es el **mínimo observado en el anillo**, no una constante.
    ///
    /// No es «el suelo de la tarjeta»: es «lo más bajo que le hemos visto en
    /// los últimos minutos». Si la GPU nunca estuvo ociosa en esa ventana, el
    /// mínimo será alto y la resta dará de menos — cosa que quien lee puede
    /// ver, porque el reposo se publica junto a la energía en vez de aplicarse
    /// por dentro.
    pub fn reposo_w(&self) -> Option<f64> {
        self.muestras
            .iter()
            .map(|m| m.vatios)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            })
    }

    /// Potencia en un instante, interpolando linealmente entre las dos
    /// muestras que lo rodean.
    fn interpolar(&self, t: Instant) -> Option<f64> {
        let mut antes: Option<&PowerSample> = None;
        for m in self.muestras.iter() {
            if m.at <= t {
                antes = Some(m);
                continue;
            }
            let a = antes?;
            let tramo = (m.at - a.at).as_secs_f64();
            if tramo <= 0.0 {
                return Some(m.vatios);
            }
            let f = (t - a.at).as_secs_f64() / tramo;
            return Some(a.vatios + (m.vatios - a.vatios) * f);
        }
        antes.map(|a| a.vatios)
    }

    /// Integra la potencia sobre `[t0, t1]`.
    ///
    /// Devuelve `None` —nunca un cero— cuando el anillo **no cubre la
    /// ventana**: si la petición empezó antes de la muestra más vieja, no hay
    /// forma de saber qué pasó antes y cualquier número sería una
    /// extrapolación disfrazada de medición. Es el caso normal justo al
    /// arrancar el proxy.
    ///
    /// Por el lado del CIERRE la exigencia se relaja hasta
    /// [`TOLERANCIA_COLA_MS`], porque la ventana acaba en `Instant::now()` y
    /// la muestra más nueva es siempre del pasado: exigir cobertura estricta
    /// dejaba el campo vacío en casi todas las peticiones. Ese último tramo se
    /// mantiene al valor leído. Es extrapolación, está acotada, y si el
    /// muestreador se queda más atrás que eso el campo vuelve a ser `None`.
    pub fn ventana(&self, t0: Instant, t1: Instant) -> Option<EnergyWindow> {
        if t1 < t0 {
            return None;
        }
        let primera = self.muestras.front()?;
        let ultima = self.muestras.back()?;
        if primera.at > t0 {
            return None;
        }
        if t1 > ultima.at && (t1 - ultima.at) > std::time::Duration::from_millis(TOLERANCIA_COLA_MS)
        {
            return None;
        }

        let mut puntos: Vec<(f64, f64)> = Vec::with_capacity(self.muestras.len() + 2);
        puntos.push((0.0, self.interpolar(t0)?));
        for m in self.muestras.iter() {
            if m.at > t0 && m.at < t1 {
                puntos.push(((m.at - t0).as_secs_f64(), m.vatios));
            }
        }
        let dur_s = (t1 - t0).as_secs_f64();
        puntos.push((dur_s, self.interpolar(t1)?));

        let vatios_por_segundo: f64 = puntos
            .windows(2)
            .map(|par| (par[0].1 + par[1].1) / 2.0 * (par[1].0 - par[0].0))
            .sum();

        Some(EnergyWindow {
            wh: vatios_por_segundo / 3600.0,
            idle_wh: self.reposo_w().unwrap_or(0.0) * dur_s / 3600.0,
            peak_w: puntos.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
            samples: self
                .muestras
                .iter()
                .filter(|m| m.at >= t0 && m.at <= t1)
                .count() as u32,
        })
    }
}

impl Default for PowerRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Variable que apaga el muestreador.
///
/// Existe porque el proxy lanza un proceso hijo, y eso es una decisión que
/// alguien puede querer revertir sin recompilar — en una máquina compartida,
/// en un contenedor sin `/dev/nvidia*`, o simplemente por no querer.
pub const ENV_APAGADO: &str = "OXIDEGATE_POWER_SAMPLING";

/// Campos que se le piden a `nvidia-smi`. Uno solo: la potencia. El resto de
/// la tarjeta ya lo enseña el panel `g` del monitor, y pedir menos hace la
/// línea imposible de malinterpretar.
const QUERY: &str = "power.draw";

/// El muestreador: un `nvidia-smi` persistente y el anillo que llena.
pub struct PowerMeter {
    anillo: Arc<Mutex<PowerRing>>,
    hijo: Mutex<Option<std::process::Child>>,
}

impl PowerMeter {
    /// Arranca el proceso y el hilo lector.
    ///
    /// `None` —y el proxy sigue funcionando igual— si el muestreo está apagado
    /// por entorno o si `nvidia-smi` no existe. Que no haya GPU no es un
    /// error: es un campo `None`.
    pub fn arrancar() -> Option<Arc<Self>> {
        if std::env::var(ENV_APAGADO).is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
            return None;
        }
        let mut hijo = Command::new("nvidia-smi")
            .args([
                &format!("--query-gpu={QUERY}"),
                "--format=csv,noheader,nounits",
                "-lms",
                &CADENCIA_MS.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let salida = hijo.stdout.take()?;

        let anillo = Arc::new(Mutex::new(PowerRing::new()));
        let destino = Arc::clone(&anillo);
        std::thread::spawn(move || {
            for linea in BufReader::new(salida).lines().map_while(Result::ok) {
                let Some(vatios) = parse_vatios(&linea) else {
                    continue;
                };
                if let Ok(mut r) = destino.lock() {
                    r.push(PowerSample {
                        at: Instant::now(),
                        vatios,
                    });
                }
            }
        });

        Some(Arc::new(Self {
            anillo,
            hijo: Mutex::new(Some(hijo)),
        }))
    }

    /// Un medidor SIN proceso hijo, sobre un anillo ya lleno.
    ///
    /// Separa «quién llena el anillo» de «quién lo integra», que es lo que
    /// permite ejercitar la guarda de upstream local en un CI sin GPU.
    ///
    /// Va bajo `cfg(test)` porque hoy **solo lo usan los tests**, y una API
    /// pública sin llamadas es API muerta: se pudre sin que nadie se entere.
    /// Si algún día entra otra fuente de muestras —RAPL para la CPU— la
    /// separación ya está hecha y quitar el atributo es una línea.
    #[cfg(test)]
    pub fn con_anillo(anillo: PowerRing) -> Arc<Self> {
        Arc::new(Self {
            anillo: Arc::new(Mutex::new(anillo)),
            hijo: Mutex::new(None),
        })
    }

    pub fn ventana(&self, t0: Instant, t1: Instant) -> Option<EnergyWindow> {
        self.anillo.lock().ok()?.ventana(t0, t1)
    }
}

impl Drop for PowerMeter {
    /// Mata al hijo. Sin esto, un `nvidia-smi` quedaría escupiendo muestras
    /// para siempre después de que el proxy pare.
    fn drop(&mut self) {
        if let Ok(mut h) = self.hijo.lock() {
            if let Some(mut c) = h.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl std::fmt::Debug for PowerMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerMeter").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn anillo(base: Instant, puntos: &[(u64, f64)]) -> PowerRing {
        let mut r = PowerRing::new();
        for (ms, w) in puntos {
            r.push(PowerSample {
                at: base + Duration::from_millis(*ms),
                vatios: *w,
            });
        }
        r
    }

    /// La línea normal de `nvidia-smi` con `nounits` es un número pelado.
    #[test]
    fn una_linea_de_vatios_se_lee() {
        assert_eq!(parse_vatios(" 47.88 "), Some(47.88));
    }

    /// `[N/A]` es lo que escupe una tarjeta sin sensor de potencia. Tomarlo
    /// por cero publicaría «esta petición fue gratis» donde lo cierto es «no
    /// se pudo leer». Ausencia honesta, igual que en el resto del proyecto.
    #[test]
    fn una_lectura_no_disponible_no_se_convierte_en_cero() {
        assert_eq!(parse_vatios("[N/A]"), None);
        assert_eq!(parse_vatios("[Not Supported]"), None);
        assert_eq!(parse_vatios(""), None);
    }

    /// Vatios negativos no existen. Si el driver los escupe, es basura, y
    /// meterla en el anillo bajaría el reposo y con él toda la resta.
    #[test]
    fn una_lectura_imposible_se_descarta() {
        assert_eq!(parse_vatios("-3"), None);
        assert_eq!(parse_vatios("NaN"), None);
    }

    /// La afirmación central: energía = área bajo la curva. 100 W constantes
    /// durante una hora son exactamente 100 Wh, y si esta cuenta se tuerce
    /// todo lo demás publica un número bonito y falso.
    #[test]
    fn cien_vatios_durante_una_hora_son_cien_vatios_hora() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (3_600_000, 100.0)]);
        let v = r.ventana(base, base + Duration::from_secs(3600)).unwrap();
        assert!((v.wh - 100.0).abs() < 1e-9, "{}", v.wh);
    }

    /// Una rampa se integra como trapecio, no como rectángulo: de 0 a 200 W en
    /// una hora son 100 Wh, no 200. Sin esto, cualquier petición que arranque
    /// la GPU desde el reposo publicaría el doble de lo que gastó.
    #[test]
    fn una_rampa_se_integra_como_trapecio_no_como_rectangulo() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 0.0), (3_600_000, 200.0)]);
        let v = r.ventana(base, base + Duration::from_secs(3600)).unwrap();
        assert!((v.wh - 100.0).abs() < 1e-9, "{}", v.wh);
    }

    /// Si la petición empezó ANTES de la muestra más vieja, el anillo no cubre
    /// la ventana y cualquier cifra sería una extrapolación disfrazada.
    #[test]
    fn una_ventana_que_empieza_antes_del_anillo_no_se_publica() {
        let base = Instant::now();
        let r = anillo(base + Duration::from_secs(10), &[(0, 100.0), (1000, 100.0)]);
        assert_eq!(r.ventana(base, base + Duration::from_secs(11)), None);
    }

    /// **El caso que se me escapó y que solo salió midiendo de verdad.**
    ///
    /// La ventana acaba en `Instant::now()` y la muestra más nueva SIEMPRE es
    /// del pasado. Con cobertura estricta hasta el final, el campo salía
    /// `None` en cada petición real — y mi primer test lo tapó porque metía
    /// una muestra en el FUTURO, cosa que el muestreador no puede producir.
    ///
    /// Ese último tramo se mantiene al valor leído: 100 W sostenidos una hora
    /// siguen siendo 100 Wh aunque la última muestra sea de 150 ms antes.
    #[test]
    fn la_cola_hasta_la_ultima_muestra_se_mantiene_al_valor_leido() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (3_599_850, 100.0)]);
        let v = r.ventana(base, base + Duration::from_secs(3600)).unwrap();
        assert!((v.wh - 100.0).abs() < 1e-9, "{}", v.wh);
    }

    /// Pero la extrapolación está ACOTADA. Si el muestreador se quedó atrás
    /// más de la tolerancia —murió, se colgó el driver, la máquina se
    /// suspendió— no se sabe qué pasó en ese hueco y el campo vuelve a ser
    /// `None`. Sin este corte, un proceso muerto publicaría el último valor
    /// leído multiplicado por horas.
    #[test]
    fn si_el_muestreador_se_quedo_atras_no_se_publica() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (1000, 100.0)]);
        assert_eq!(r.ventana(base, base + Duration::from_secs(5)), None);
    }

    /// Sin ninguna muestra no hay nada que integrar. Cero sería mentira.
    #[test]
    fn un_anillo_vacio_no_publica_energia() {
        let base = Instant::now();
        assert_eq!(PowerRing::new().ventana(base, base), None);
    }

    /// El reposo es el mínimo OBSERVADO, no una constante compilada. Una
    /// constante sería falsa en cuanto alguien cambiara de tarjeta.
    #[test]
    fn el_reposo_es_el_minimo_observado() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 250.0), (200, 44.0), (400, 180.0)]);
        assert_eq!(r.reposo_w(), Some(44.0));
    }

    /// El reposo se publica APARTE, sin restarlo. Quien lee ve la resta; el
    /// medidor no la esconde dentro de un número único que finge precisión.
    #[test]
    fn el_reposo_se_publica_al_lado_y_no_va_restado() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (3_600_000, 100.0)]);
        let v = r.ventana(base, base + Duration::from_secs(3600)).unwrap();
        assert!((v.wh - 100.0).abs() < 1e-9, "bruta: {}", v.wh);
        assert!((v.idle_wh - 100.0).abs() < 1e-9, "reposo: {}", v.idle_wh);
    }

    /// El pico es el de la VENTANA, no el del anillo entero: si la tarjeta
    /// llegó a 300 W diez minutos antes, eso no es de esta petición.
    #[test]
    fn el_pico_es_el_de_la_ventana_no_el_del_anillo() {
        let base = Instant::now();
        let r = anillo(
            base,
            &[(0, 300.0), (1000, 50.0), (2000, 120.0), (3000, 60.0)],
        );
        let v = r
            .ventana(base + Duration::from_secs(1), base + Duration::from_secs(3))
            .unwrap();
        assert!((v.peak_w - 120.0).abs() < 1e-9, "{}", v.peak_w);
    }

    /// Cuántas muestras REALES cayeron dentro. Es lo que distingue una curva
    /// de una interpolación entre dos puntos de fuera.
    #[test]
    fn se_publica_cuantas_muestras_reales_hubo() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (500, 110.0), (1000, 120.0)]);
        let v = r
            .ventana(
                base + Duration::from_millis(400),
                base + Duration::from_millis(600),
            )
            .unwrap();
        assert_eq!(v.samples, 1);
    }

    /// Una ventana más corta que la cadencia del muestreador sigue publicando
    /// energía —interpolada entre las dos muestras que la rodean— pero lo
    /// declara con `samples: 0` para que quien lee sepa lo basta que es.
    #[test]
    fn una_ventana_sin_muestras_dentro_lo_declara_en_vez_de_callarlo() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (1000, 100.0)]);
        let v = r
            .ventana(
                base + Duration::from_millis(300),
                base + Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(v.samples, 0);
        assert!(v.wh > 0.0);
    }

    /// **La propiedad incómoda, fijada a propósito.**
    ///
    /// Dos peticiones solapadas integran los MISMOS vatios y las dos los
    /// reclaman. Sumar `energy_wh` sobre filas solapadas da más energía de la
    /// que la máquina gastó. No es un bug que se pueda arreglar con estos
    /// datos: es lo que significa el campo, y por eso está escrito aquí y en
    /// los docs en vez de descubrirse sumando una columna.
    #[test]
    fn dos_ventanas_solapadas_reclaman_la_misma_energia() {
        let base = Instant::now();
        let r = anillo(base, &[(0, 100.0), (3_600_000, 100.0)]);
        let a = r.ventana(base, base + Duration::from_secs(3600)).unwrap();
        let b = r.ventana(base, base + Duration::from_secs(3600)).unwrap();
        assert_eq!(a.wh, b.wh);
        // La máquina gastó 100 Wh. La suma de las dos filas dice 200.
        assert!((a.wh + b.wh - 200.0).abs() < 1e-9);
    }

    /// El anillo no puede crecer sin límite: el proxy corre durante días.
    #[test]
    fn el_anillo_tira_las_muestras_viejas() {
        let base = Instant::now();
        let mut r = PowerRing::con_capacidad(3);
        for i in 0..10 {
            r.push(PowerSample {
                at: base + Duration::from_millis(i * 200),
                vatios: i as f64,
            });
        }
        assert_eq!(r.len(), 3);
        assert_eq!(r.reposo_w(), Some(7.0));
    }

    /// La guarda del contrato: con upstream remoto no hay campo de energía.
    /// Muestrear tu GPU mientras responde Anthropic mide tu escritorio.
    #[test]
    fn un_upstream_local_se_reconoce() {
        for url in [
            "http://127.0.0.1:11434/api/chat",
            "http://localhost:8899/v1/messages",
            "http://[::1]:11434/api/generate",
            "http://0.0.0.0:11434/",
            "http://127.0.0.5:11434/",
        ] {
            assert!(es_upstream_local(url), "{url}");
        }
    }

    #[test]
    fn un_upstream_remoto_no_publica_energia() {
        for url in [
            "https://api.anthropic.com/v1/messages",
            "https://generativelanguage.googleapis.com/v1beta/models",
            "https://api.openai.com/v1/chat/completions",
        ] {
            assert!(!es_upstream_local(url), "{url}");
        }
    }

    /// **El caso que un `contains` habría fallado.** `localhost.ejemplo.com`
    /// es un dominio remoto perfectamente registrable y contiene la palabra:
    /// publicaría los vatios de tu escritorio como si fueran de su inferencia.
    #[test]
    fn un_dominio_remoto_que_contiene_localhost_no_es_local() {
        assert!(!es_upstream_local("https://localhost.ejemplo.com/v1/chat"));
        assert!(!es_upstream_local("https://127.0.0.1.ejemplo.com/v1/chat"));
        assert!(!es_upstream_local("https://ejemplo.com/proxy/localhost"));
    }

    /// El userinfo de una URL no es el host. `http://localhost@remoto.com/`
    /// apunta a `remoto.com`.
    #[test]
    fn el_userinfo_no_se_confunde_con_el_host() {
        assert_eq!(
            host_de_url("http://usuario@remoto.com/x"),
            Some("remoto.com")
        );
        assert!(!es_upstream_local("http://localhost@remoto.com/x"));
    }
}
