use std::env;

use clap::{Args, Subcommand};
use nh_core::{
  args::CommonRebuildArgs,
  checks::{FeatureRequirements, FlakeFeatures, LegacyFeatures},
  installable::Installable,
};

#[derive(Args, Debug)]
#[clap(verbatim_doc_comment)]
/// System-manager functionality
///
/// Activate services/config on non-NixOS systems via system-manager
pub struct SystemArgs {
  #[command(subcommand)]
  pub subcommand: SystemSubcommand,
}

impl SystemArgs {
  #[must_use]
  pub fn get_feature_requirements(&self) -> Box<dyn FeatureRequirements> {
    match &self.subcommand {
      SystemSubcommand::Switch(args) | SystemSubcommand::Build(args) => {
        if args.uses_flakes() {
          Box::new(FlakeFeatures)
        } else {
          Box::new(LegacyFeatures)
        }
      },
    }
  }
}

#[derive(Debug, Subcommand)]
pub enum SystemSubcommand {
  /// Build and activate a system-manager configuration
  Switch(SystemRebuildArgs),

  /// Build a system-manager configuration
  Build(SystemRebuildArgs),
}

#[derive(Debug, Args)]
pub struct SystemRebuildArgs {
  #[command(flatten)]
  pub common: CommonRebuildArgs,

  #[command(flatten)]
  pub update_args: nh_core::update::UpdateArgs,

  /// Name of the flake systemConfigs attribute
  ///
  /// If unspecified, NH will try <hostname> and then default.
  #[arg(long, short)]
  pub configuration: Option<String>,

  /// Extra arguments passed to nix build
  #[arg(last = true)]
  pub extra_args: Vec<String>,

  /// Don't panic if calling nh as root
  #[arg(short = 'R', long, env = "NH_BYPASS_ROOT_CHECK")]
  pub bypass_root_check: bool,

  /// If true, write under /run/etc instead of /etc during activation
  #[arg(long)]
  pub ephemeral: bool,

  /// Show activation logs
  #[arg(long, env = "NH_SHOW_ACTIVATION_LOGS")]
  pub show_activation_logs: bool,
}

impl SystemRebuildArgs {
  #[must_use]
  pub fn uses_flakes(&self) -> bool {
    if env::var("NH_SYSTEM_FLAKE").is_ok_and(|v| !v.is_empty()) {
      return true;
    }

    matches!(self.common.installable, Installable::Flake { .. })
  }
}
