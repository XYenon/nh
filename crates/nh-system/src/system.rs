use std::{ffi::OsString, path::PathBuf};

use color_eyre::{
  Result,
  eyre::{Context, bail},
};
use nh_core::{
  args::DiffType,
  command::{self as commands, Command, ElevationStrategy},
  installable::{CommandContext, Installable, parse_attribute},
  update::update,
  util::{get_hostname, print_dix_diff},
};
use tracing::{debug, info, warn};

use crate::args::{SystemArgs, SystemRebuildArgs, SystemSubcommand};

const PROFILE_PATH: &str =
  "/nix/var/nix/profiles/system-manager-profiles/system-manager";
const ENGINE_BIN: &str = "bin/system-manager-engine";
const MAX_SYSTEM_ATTR_DEPTH: usize = 3;

impl SystemArgs {
  /// Run the `system` subcommand.
  ///
  /// # Parameters
  ///
  /// * `self` - The System Manager operation arguments
  /// * `elevation` - The privilege elevation strategy (sudo/doas/none)
  ///
  /// # Returns
  ///
  /// Returns `Ok(())` if the operation succeeds.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  ///
  /// - Build or activation operations fail
  /// - Nix evaluation or building fails
  /// - File system operations fail
  pub fn run(self, elevation: ElevationStrategy) -> Result<()> {
    use SystemRebuildVariant::{Build, Switch};
    match self.subcommand {
      SystemSubcommand::Switch(args) => args.rebuild(&Switch, elevation),
      SystemSubcommand::Build(args) => {
        if args.common.ask || args.common.dry {
          warn!("`--ask` and `--dry` have no effect for `nh system build`");
        }
        args.rebuild(&Build, elevation)
      },
    }
  }
}

enum SystemRebuildVariant {
  Switch,
  Build,
}

impl SystemRebuildArgs {
  fn rebuild(
    self,
    variant: &SystemRebuildVariant,
    elevation: ElevationStrategy,
  ) -> Result<()> {
    use SystemRebuildVariant::Build;

    if nix::unistd::Uid::effective().is_root() && !self.bypass_root_check {
      bail!(
        "Don't run nh system as root. I will call sudo internally as needed"
      );
    }

    let (out_path, _tempdir_guard): (PathBuf, Option<tempfile::TempDir>) =
      if let Some(ref p) = self.common.out_link {
        (p.clone(), None)
      } else {
        let dir = tempfile::Builder::new().prefix("nh-system").tempdir()?;
        (dir.as_ref().join("result"), Some(dir))
      };

    debug!("Output path: {out_path:?}");

    let installable = self
      .common
      .installable
      .clone()
      .resolve(CommandContext::System)?;

    let installable = match installable {
      Installable::Unspecified => Installable::try_find_default_for_system()?,
      other => other,
    };

    if self.update_args.update_all || self.update_args.update_input.is_some() {
      update(
        &installable,
        self.update_args.update_input,
        self.common.passthrough.commit_lock_file,
      )?;
    }

    let toplevel =
      toplevel_for(installable, &self.extra_args, self.configuration.clone())?;

    commands::Build::new(toplevel)
      .extra_arg("--out-link")
      .extra_arg(&out_path)
      .extra_args(&self.extra_args)
      .passthrough(&self.common.passthrough)
      .message("Building System Manager configuration")
      .nom(!self.common.no_nom)
      .run()
      .wrap_err("Failed to build System Manager configuration")?;

    if matches!(self.common.diff, DiffType::Never) {
      debug!("Not running dix as the --diff flag is set to never.");
    } else {
      let profile_path = PathBuf::from(PROFILE_PATH);
      if profile_path.exists() {
        let _ = print_dix_diff(&profile_path, &out_path);
      } else {
        debug!("Skipping diff as no system-manager profile was found.");
      }
    }

    if self.common.dry || matches!(variant, Build) {
      if self.common.ask {
        warn!("--ask has no effect as dry run was requested");
      }
      return Ok(());
    }

    if self.common.ask {
      let confirmation = inquire::Confirm::new("Apply the config?")
        .with_default(false)
        .prompt()?;

      if !confirmation {
        bail!("User rejected the new config");
      }
    }

    let store_path = out_path
      .canonicalize()
      .context("Failed to resolve output path to actual store path")?;
    let engine_path = store_path.join(ENGINE_BIN);

    if !engine_path
      .try_exists()
      .context("Failed to check if system-manager-engine exists")?
    {
      bail!(
        "Built output does not contain system-manager-engine at {}",
        engine_path.display()
      );
    }

    Command::new(&engine_path)
      .arg("register")
      .arg("--store-path")
      .arg(&store_path)
      .elevate(Some(elevation.clone()))
      .with_required_env()
      .show_output(self.show_activation_logs)
      .message("Registering System Manager profile")
      .run()
      .wrap_err("Failed to register System Manager profile")?;

    let mut activate_cmd = Command::new(&engine_path)
      .arg("activate")
      .arg("--store-path")
      .arg(&store_path)
      .elevate(Some(elevation))
      .with_required_env()
      .show_output(self.show_activation_logs)
      .message("Activating System Manager profile");

    if self.ephemeral {
      activate_cmd = activate_cmd.arg("--ephemeral");
    }

    activate_cmd
      .run()
      .wrap_err("System Manager activation failed")?;

    debug!("Completed operation with output path: {out_path:?}");

    Ok(())
  }
}

pub fn toplevel_for<I, S>(
  installable: Installable,
  extra_args: I,
  configuration_name: Option<String>,
) -> Result<Installable>
where
  I: IntoIterator<Item = S>,
  S: AsRef<std::ffi::OsStr>,
{
  let mut res = installable;
  let extra_args: Vec<OsString> = {
    let mut vec = Vec::new();
    for elem in extra_args {
      vec.push(elem.as_ref().to_owned());
    }
    vec
  };

  let mut parsed_configuration = configuration_name
    .map(|name| {
      let parsed = parse_attribute(&name);
      if parsed.is_empty() {
        bail!("--configuration cannot be empty");
      }
      Ok(parsed)
    })
    .transpose()?;

  match res {
    Installable::Flake {
      ref reference,
      ref mut attribute,
    } => {
      if !attribute.is_empty() {
        if parsed_configuration.is_some() {
          bail!(
            "Cannot use --configuration together with an installable \
             attribute path"
          );
        }

        if attribute[0] != "systemConfigs" {
          attribute.insert(0, String::from("systemConfigs"));
        }
      } else {
        attribute.push(String::from("systemConfigs"));
      }

      if attribute.len() > MAX_SYSTEM_ATTR_DEPTH {
        bail!(
          "Attribute path is too specific: {}. Please specify only the \
           configuration name (e.g., '.#default')",
          attribute.join(".")
        );
      }

      if attribute.len() == 1 {
        if let Some(config_attribute) = parsed_configuration.take() {
          attribute.extend(config_attribute);
        } else {
          attribute.extend(discover_system_config(reference, &extra_args)?);
        }
      }
    },
    Installable::File {
      ref mut attribute, ..
    }
    | Installable::Expression {
      ref mut attribute, ..
    } => {
      if !attribute.is_empty() {
        if parsed_configuration.is_some() {
          bail!(
            "Cannot use --configuration together with an installable \
             attribute path"
          );
        }

        if attribute[0] != "systemConfigs" {
          attribute.insert(0, String::from("systemConfigs"));
        }
      } else {
        attribute.push(String::from("systemConfigs"));
      }

      if attribute.len() > MAX_SYSTEM_ATTR_DEPTH {
        bail!(
          "Attribute path is too specific: {}. Please specify only the \
           configuration name (e.g., '.#default')",
          attribute.join(".")
        );
      }

      if attribute.len() == 1 {
        if let Some(config_attribute) = parsed_configuration.take() {
          attribute.extend(config_attribute);
        } else {
          info!(
            "No configuration was specified for this installable, defaulting \
             to systemConfigs.default"
          );
          attribute.push(String::from("default"));
        }
      }
    },
    Installable::Store { .. } => {
      if parsed_configuration.is_some() {
        warn!(
          "Ignoring --configuration because store path installables already \
           point to an exact build output"
        );
      }
    },
    Installable::Unspecified => {
      unreachable!(
        "Unspecified installable should have been resolved before calling \
         toplevel_for"
      )
    },
  }

  Ok(res)
}

fn discover_system_config(
  flake_reference: &str,
  extra_args: &[OsString],
) -> Result<Vec<String>> {
  let hostname = get_hostname(None)?;
  let current_system = get_current_system();

  let mut candidates = Vec::new();
  if let Some(system) = current_system {
    candidates.push(vec![system.clone(), hostname.clone()]);
    candidates.push(vec![system, String::from("default")]);
  }
  candidates.push(vec![hostname]);
  candidates.push(vec![String::from("default")]);

  for candidate in &candidates {
    if flake_attr_exists(flake_reference, candidate, extra_args)? {
      debug!(
        "Using inferred system-manager configuration: systemConfigs.{}",
        candidate.join(".")
      );
      return Ok(candidate.clone());
    }
  }

  let tried = candidates
    .iter()
    .map(|candidate| format!("systemConfigs.{}", candidate.join(".")))
    .collect::<Vec<_>>()
    .join(", ");

  bail!(
    "Couldn't find a system-manager configuration automatically, tried: \
     {tried}. Use --configuration or pass an explicit flake attribute."
  );
}

fn flake_attr_exists(
  flake_reference: &str,
  candidate: &[String],
  extra_args: &[OsString],
) -> Result<bool> {
  let attr_path_expr = candidate
    .iter()
    .map(|segment| {
      format!("\"{}\"", segment.replace('\\', "\\\\").replace('"', "\\\""))
    })
    .collect::<Vec<_>>()
    .join(".");

  let check_res = Command::new("nix")
    .with_required_env()
    .arg("eval")
    .args(extra_args)
    .arg("--apply")
    .arg(format!("x: x ? {attr_path_expr}"))
    .args(
      (Installable::Flake {
        reference: flake_reference.to_owned(),
        attribute: vec![String::from("systemConfigs")],
      })
      .to_args(),
    )
    .run_capture()
    .wrap_err(format!(
      "Failed running nix eval to check for system-manager configuration \
       systemConfigs.{}",
      candidate.join(".")
    ))?;

  Ok(check_res.map(|s| s.trim().to_owned()).as_deref() == Some("true"))
}

fn get_current_system() -> Option<String> {
  let result = Command::new("nix")
    .with_required_env()
    .arg("config")
    .arg("show")
    .arg("system")
    .run_capture();

  match result {
    Ok(Some(system)) => {
      let trimmed = system.trim();
      if trimmed.is_empty() {
        None
      } else {
        Some(trimmed.to_owned())
      }
    },
    Ok(None) => None,
    Err(err) => {
      debug!(
        "Failed to determine current Nix system for system-manager \
         auto-discovery: {err}"
      );
      None
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_toplevel_for_flake_with_explicit_configuration() {
    let installable = Installable::Flake {
      reference: String::from("."),
      attribute: vec![],
    };

    let result = toplevel_for(
      installable,
      Vec::<String>::new(),
      Some(String::from("default")),
    )
    .expect("toplevel_for should succeed");

    assert_eq!(result.to_args(), vec![String::from(
      ".#systemConfigs.default"
    )]);
  }

  #[test]
  fn test_toplevel_for_flake_prepends_system_configs() {
    let installable = Installable::Flake {
      reference: String::from("."),
      attribute: vec![String::from("x86_64-linux"), String::from("default")],
    };

    let result = toplevel_for(installable, Vec::<String>::new(), None)
      .expect("toplevel_for should succeed");

    assert_eq!(result.to_args(), vec![String::from(
      ".#systemConfigs.x86_64-linux.default"
    )]);
  }

  #[test]
  fn test_toplevel_for_file_defaults_to_default_configuration() {
    let installable = Installable::File {
      path:      PathBuf::from("./flake.nix"),
      attribute: vec![],
    };

    let result = toplevel_for(installable, Vec::<String>::new(), None)
      .expect("toplevel_for should succeed");

    assert_eq!(result.to_args(), vec![
      String::from("--file"),
      String::from("./flake.nix"),
      String::from("systemConfigs.default"),
    ]);
  }

  #[test]
  fn test_toplevel_for_rejects_configuration_with_explicit_attribute_path() {
    let installable = Installable::Flake {
      reference: String::from("."),
      attribute: vec![String::from("systemConfigs"), String::from("default")],
    };

    assert!(
      toplevel_for(
        installable,
        Vec::<String>::new(),
        Some(String::from("other")),
      )
      .is_err()
    );
  }
}
