use clap::{Args, Subcommand};
use nh_core::{
  args::CommonRebuildArgs,
  checks::{
    FeatureRequirements,
    FlakeFeatures,
    LegacyFeatures,
    SystemReplFeatures,
  },
};
use nh_installable::{CommandContext, InstallableArgs};
use nh_remote::RemoteHost;

#[derive(Args, Debug)]
#[clap(verbatim_doc_comment)]
/// System Manager functionality
///
/// Activate services/config on non-NixOS systems via System Manager
pub struct SystemArgs {
  #[command(subcommand)]
  pub subcommand: SystemSubcommand,
}

impl SystemArgs {
  #[must_use]
  pub fn get_feature_requirements(&self) -> Box<dyn FeatureRequirements> {
    match &self.subcommand {
      SystemSubcommand::Repl(args) => {
        let is_flake = args.uses_flakes();
        Box::new(SystemReplFeatures { is_flake })
      },
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

  /// Load a system-manager configuration in a Nix REPL
  Repl(SystemReplArgs),
}

#[derive(Debug, Args)]
pub struct SystemRebuildArgs {
  #[command(flatten)]
  pub common: CommonRebuildArgs,

  #[command(flatten)]
  pub update_args: nh_core::update::UpdateArgs,

  /// When using a flake installable, select this name from systemConfigs
  ///
  /// When unspecified, NH tries the local hostname for local deployments, or
  /// the hostname of the target machine for remote deployments (see
  /// --target-host), then `default`.
  #[arg(long, short)]
  pub configuration: Option<String>,

  /// Deploy the built configuration to a different host over SSH
  #[arg(long)]
  pub target_host: Option<RemoteHost>,

  /// Build the configuration on a different host over SSH
  #[arg(long)]
  pub build_host: Option<RemoteHost>,

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
  #[arg(long, env = "NH_SHOW_ACTIVATION_LOGS", value_parser = clap::builder::BoolishValueParser::new())]
  pub show_activation_logs: bool,
}

impl SystemRebuildArgs {
  #[must_use]
  pub fn uses_flakes(&self) -> bool {
    self.common.installable.uses_flakes(CommandContext::System)
  }
}

#[derive(Debug, Args)]
pub struct SystemReplArgs {
  #[command(flatten)]
  pub installable: InstallableArgs,

  /// When using a flake installable, select this name from systemConfigs
  ///
  /// When unspecified, NH tries the local hostname, then `default`.
  #[arg(long, short)]
  pub configuration: Option<String>,

  /// Extra arguments passed to nix repl
  #[arg(last = true)]
  pub extra_args: Vec<String>,
}

impl SystemReplArgs {
  #[must_use]
  pub fn uses_flakes(&self) -> bool {
    self.installable.uses_flakes(CommandContext::System)
  }
}
