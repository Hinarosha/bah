use crate::args::Args;
use crate::backend::PackageBackend;
use crate::config::Config;
use crate::exec::{self, Status};

use anyhow::Result;

#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyPacmanBackend;

impl PackageBackend for LegacyPacmanBackend {
    fn pacman(&self, config: &Config, args: &Args<&str>) -> Result<Status> {
        exec::pacman(config, args)
    }
}
