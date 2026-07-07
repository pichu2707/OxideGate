//! Lee el entorno
use std::env;
use std::path::PathBuf;

pub struct AppConfig {
    pub local_port: u16,
    pub target_openai_url: String,
    pub target_anthropic_url: String,
    /// Host raíz de Gemini (sin path). El path `/v1beta/models/...` lo preserva
    /// el proxy tal cual llega del cliente, así que aquí va solo el origen.
    pub target_gemini_url: String,
    pub storage_dir: PathBuf,
}

impl AppConfig {
    pub fn load() -> Self {
        // Buscamos la carpeta HOME del usuario para guardar nuestros propios datos de forma limpia
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let mut storage_dir = PathBuf::from(home);
        storage_dir.push(".config");
        storage_dir.push("oxidegate"); // Nuestra propia carpeta independiente

        Self {
            local_port: env::var("OXIDEGATE_PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .unwrap_or(8080),
            target_openai_url: env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            target_anthropic_url: env::var("ANTHROPIC_API_BASE")
                .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
            // Solo el host: el cliente Gemini pega a `/v1beta/models/{model}:...`
            // y ese path se reenvía sin tocar.
            target_gemini_url: env::var("GEMINI_API_BASE")
                .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string()),
            storage_dir,
        }
    }

    // Método útil para que el optimizador sepa si existe el entorno de OpenCode en la máquina
    pub fn has_opencode_env(&self) -> bool {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let mut path = PathBuf::from(home);
        path.push(".config");
        path.push("opencode");
        path.exists()
    }
}
