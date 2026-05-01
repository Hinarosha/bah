mod alpm;
mod legacy_pacman;

use crate::args::Args;
use crate::config::Config;
use crate::exec::Status;

use anyhow::Result;

pub trait PackageBackend {
    fn pacman(&self, config: &Config, args: &Args<&str>) -> Result<Status>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    LegacyPacman,
    Alpm,
}

fn selected_backend() -> BackendKind {
    match std::env::var("BAH_BACKEND")
        .unwrap_or_else(|_| "alpm".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "legacy" | "pacman" => BackendKind::LegacyPacman,
        _ => BackendKind::Alpm,
    }
}

fn allow_fallback() -> bool {
    !matches!(
        std::env::var("BAH_BACKEND_STRICT")
            .ok()
            .as_deref()
            .map(|v| v.to_ascii_lowercase()),
        Some(v) if v == "1" || v == "true" || v == "yes"
    )
}

pub fn pacman<S: AsRef<str>>(config: &Config, args: &Args<S>) -> Result<Status> {
    let args = args.as_str();
    match selected_backend() {
        BackendKind::LegacyPacman => legacy_pacman::LegacyPacmanBackend.pacman(config, &args),
        BackendKind::Alpm => {
            let backend = alpm::AlpmBackend;
            match backend.pacman(config, &args) {
                Ok(status) => Ok(status),
                Err(err) => {
                    eprintln!(
                        "{} {}: {} (op={} args={:?} targets={:?})",
                        config.color.warning.paint("::"),
                        "ALPM backend failed",
                        err,
                        args.op,
                        args.args
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>(),
                        args.targets
                    );
                    if allow_fallback() {
                        eprintln!(
                            "{} {}",
                            config.color.warning.paint("::"),
                            "falling back to legacy pacman backend"
                        );
                        legacy_pacman::LegacyPacmanBackend.pacman(config, &args)
                    } else {
                        Err(err)
                    }
                }
            }
        }
    }
}


