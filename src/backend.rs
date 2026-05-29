use crate::config::AppConfig;

#[derive(Debug, Clone)]
pub struct BackendOutput {
    pub text: String,
}

pub trait Backend {
    fn name(&self) -> &'static str;
    fn respond(&self, prompt: &str) -> BackendOutput;
}

pub struct EchoBackend;
pub struct StubBackend;

impl Backend for EchoBackend {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn respond(&self, prompt: &str) -> BackendOutput {
        BackendOutput {
            text: format!("echo: {prompt}"),
        }
    }
}

impl Backend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn respond(&self, prompt: &str) -> BackendOutput {
        BackendOutput {
            text: format!("provider stub is not wired yet: {prompt}"),
        }
    }
}

pub fn select_backend(config: &AppConfig) -> Box<dyn Backend> {
    match config.provider.as_str() {
        "stub" => Box::new(StubBackend),
        "echo" => Box::new(EchoBackend),
        _ => Box::new(EchoBackend),
    }
}
