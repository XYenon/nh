pub mod args;

use std::{
  convert::Into,
  ffi::OsString,
  path::{Path, PathBuf},
};

use args::{SystemRebuildArgs, SystemReplArgs, SystemSubcommand};
use color_eyre::{
  Result,
  eyre::{Context, bail},
};
use nh_core::{
  args::DiffType,
  command::{self, Command, CommandKind, ElevationStrategy, NixCommand},
  update::update_with_args,
  util::{ensure_ssh_key_login, get_hostname},
};
use nh_diff::print_dix_diff;
use nh_installable::{CommandContext, Installable, parse_attribute};
use nh_remote::{self, RemoteBuildConfig, RemoteHost};
use tracing::{debug, info, warn};

/// Run a [`NixCommand`] and capture its stdout, propagating stderr context
/// on failure.
///
/// # Errors
///
/// Returns an error if the command cannot be started, returns a non-zero exit
/// status, or emits non-UTF-8 stdout.
fn capture_nix_stdout(command: &NixCommand) -> Result<String> {
  let output = command.output().wrap_err("Failed to run nix command")?;
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
      bail!("nix command failed (exit status {:?})", output.status);
    }
    bail!(
      "nix command failed (exit status {:?})\nstderr:\n{stderr}",
      output.status
    );
  }

  String::from_utf8(output.stdout)
    .wrap_err("nix command emitted non-UTF-8 stdout")
}

const SYSTEM_MANAGER_PROFILE: &str =
  "/nix/var/nix/profiles/system-manager-profiles/system-manager";
const ENGINE_BIN: &str = "bin/system-manager-engine";
const ESSENTIAL_FILES: &[(&str, &str)] =
  &[(ENGINE_BIN, "system-manager engine")];
const MAX_SYSTEM_ATTR_DEPTH: usize = 3;

impl args::SystemArgs {
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
  /// - Remote operations encounter network or SSH issues
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
      SystemSubcommand::Repl(args) => args.run(),
    }
  }
}

#[derive(Debug)]
enum SystemRebuildVariant {
  Switch,
  Build,
}

impl SystemRebuildArgs {
  /// Build or switch a system-manager configuration.
  ///
  /// # Errors
  ///
  /// Returns an error if installable resolution, Nix build, remote deploy, or
  /// system-manager register/activate fails.
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

    if self.build_host.is_some() || self.target_host.is_some() {
      ensure_ssh_key_login()?;
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
      .resolve_or_default(CommandContext::System)?;

    if self.update_args.update_all || self.update_args.update_input.is_some() {
      update_with_args(
        &installable,
        self.update_args.update_input.clone(),
        &self.common.passthrough,
      )?;
    }

    let discovery_hostname = resolve_discovery_hostname(
      self.configuration.as_deref(),
      self.target_host.as_ref(),
    )?;

    let _ssh_guard = if self.build_host.is_some() || self.target_host.is_some()
    {
      let guard = nh_remote::init_ssh_control();

      if let Some(build_host) = &self.build_host {
        nh_remote::open_ssh_control_master(build_host)
          .context("Failed to establish SSH connection to build host")?;
      }

      if let Some(target_host) = &self.target_host {
        nh_remote::open_ssh_control_master(target_host)
          .context("Failed to establish SSH connection to target host")?;
      }

      Some(guard)
    } else {
      None
    };

    let (store_path, already_on_target) = match installable {
      Installable::Store { path } => {
        if self.configuration.is_some() {
          warn!(
            "Ignoring --configuration because store path installables already \
             point to an exact build output"
          );
        }
        (
          path.canonicalize().context(
            "Failed to resolve store path installable to canonical store path",
          )?,
          false,
        )
      },
      installable => {
        let toplevel = toplevel_for(
          installable,
          &self.extra_args,
          self.configuration.clone(),
          discovery_hostname,
        )?;

        if let Some(build_host) = self.build_host.clone() {
          info!("Building System Manager configuration");

          let already_on_target = self
            .target_host
            .as_ref()
            .is_some_and(|target| target.hostname() == build_host.hostname());

          let config = RemoteBuildConfig {
            build_host,
            target_host: self.target_host.clone(),
            use_nom: !self.common.no_nom,
            use_substitutes: self.common.passthrough.use_substitutes
              && !self.common.passthrough.network_restricted(),
            execution_args: self
              .extra_args
              .iter()
              .map(Into::into)
              .chain(
                self
                  .common
                  .passthrough
                  .generate_remote_build_args()
                  .into_iter()
                  .map(Into::into),
              )
              .collect(),
          };

          let store_path = nh_remote::build_remote_with_args(
            &toplevel,
            &config,
            Some(&out_path),
            &self.common.passthrough.generate_evaluation_args(),
          )
          .wrap_err("Failed to build System Manager configuration")?;

          // When build and target are the same remote host, no local out-link
          // is created and the returned store path is the only authoritative
          // result path. Different hosts still copy through localhost below so
          // the fallback path is completed when direct remote copying fails.
          (store_path, already_on_target)
        } else {
          command::Build::new(toplevel)
            .extra_arg("--out-link")
            .extra_arg(&out_path)
            .extra_args(&self.extra_args)
            .passthrough(&self.common.passthrough)
            .message("Building System Manager configuration")
            .nom(!self.common.no_nom)
            .run()
            .wrap_err("Failed to build System Manager configuration")?;

          (
            out_path
              .canonicalize()
              .context("Failed to resolve output path to actual store path")?,
            false,
          )
        }
      },
    };

    if matches!(self.common.diff, DiffType::Never) {
      debug!("Not running dix as the --diff flag is set to never.");
    } else {
      let profile_path = PathBuf::from(SYSTEM_MANAGER_PROFILE);
      if profile_path.exists() {
        let _ = print_dix_diff(&profile_path, &store_path);
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

    // Elevation is activation-only. Avoid probing a remote uid for build-only
    // and dry-run operations.
    let remote_elevate = if self.target_host.is_some() {
      self.determine_remote_elevation(&elevation)?
    } else {
      false
    };

    if self.common.ask {
      let confirmation = inquire::Confirm::new("Apply the config?")
        .with_default(false)
        .prompt()?;

      if !confirmation {
        bail!("User rejected the new config");
      }
    }

    let engine_path = store_path.join(ENGINE_BIN);

    if let Some(target_host) = &self.target_host {
      if !already_on_target {
        nh_remote::copy_to_remote_with_args(
          target_host,
          &store_path,
          self.common.passthrough.use_substitutes
            && !self.common.passthrough.network_restricted(),
          &self.common.passthrough.generate_evaluation_args(),
        )
        .context("Failed to copy system-manager closure to target host")?;
      }

      nh_remote::validate_closure_remote(
        target_host,
        &store_path,
        ESSENTIAL_FILES,
        self
          .build_host
          .as_ref()
          .map(|host| format!("built on '{host}'"))
          .as_deref(),
      )
      .context("Failed to validate system-manager closure on target host")?;
    } else if !engine_path
      .try_exists()
      .context("Failed to check if system-manager-engine exists")?
    {
      bail!(
        "Built output does not contain system-manager-engine at {}",
        engine_path.display()
      );
    }

    self.register_and_activate(
      &store_path,
      &engine_path,
      self.target_host.as_ref(),
      remote_elevate,
      elevation,
    )?;

    debug!("Completed operation with store path: {store_path:?}");

    Ok(())
  }

  /// Determine whether privilege elevation is needed on the remote target
  /// host.
  ///
  /// Returns `Ok(false)` when there is no target host or elevation is
  /// disabled.
  ///
  /// # Errors
  ///
  /// Returns an error if the remote user's uid cannot be probed.
  fn determine_remote_elevation(
    &self,
    elevation: &ElevationStrategy,
  ) -> Result<bool> {
    let Some(target_host) = &self.target_host else {
      return Ok(false);
    };
    if matches!(elevation, ElevationStrategy::None) {
      return Ok(false);
    }
    let uid = nh_remote::probe_remote_uid(target_host)?;
    Ok(uid != 0)
  }

  /// Register and activate a system-manager configuration.
  ///
  /// For remote targets this delegates to [`nh_remote::activate_remote`]; for
  /// local activation it runs `system-manager-engine register` followed by
  /// `system-manager-engine activate`, optionally with privilege elevation.
  ///
  /// # Errors
  ///
  /// Returns an error if the register or activate commands fail, or if the
  /// remote SSH activation fails.
  fn register_and_activate(
    &self,
    store_path: &Path,
    engine_path: &Path,
    target_host: Option<&RemoteHost>,
    remote_elevate: bool,
    elevation: ElevationStrategy,
  ) -> Result<()> {
    if let Some(host) = target_host {
      return nh_remote::activate_remote(
        host,
        store_path,
        &nh_remote::ActivateRemoteConfig {
          platform:           nh_remote::Platform::SystemManager,
          activation_type:    nh_remote::ActivationType::Switch,
          install_bootloader: false,
          ephemeral:          self.ephemeral,
          show_logs:          self.show_activation_logs,
          elevation:          remote_elevate.then_some(elevation),
        },
      );
    }

    let store_path_arg = store_path.to_string_lossy().into_owned();
    let register_args = [
      String::from("register"),
      String::from("--store-path"),
      store_path_arg.clone(),
    ];
    let mut activate_args = vec![
      String::from("activate"),
      String::from("--store-path"),
      store_path_arg,
    ];
    if self.ephemeral {
      activate_args.push(String::from("--ephemeral"));
    }

    Command::new(engine_path)
      .args(&register_args)
      .elevate(Some(elevation.clone()))
      .with_required_env()
      .show_output(self.show_activation_logs)
      .message("Registering System Manager profile")
      .run()
      .wrap_err("Failed to register System Manager profile")?;

    Command::new(engine_path)
      .args(&activate_args)
      .elevate(Some(elevation))
      .with_required_env()
      .show_output(self.show_activation_logs)
      .message("Activating System Manager profile")
      .run()
      .wrap_err("System Manager activation failed")?;

    Ok(())
  }
}

impl SystemReplArgs {
  /// Load a system-manager configuration in a Nix REPL.
  ///
  /// # Errors
  ///
  /// Returns an error if the installable cannot be resolved, the configuration
  /// attribute cannot be discovered, or `nix repl` fails.
  fn run(self) -> Result<()> {
    let installable = self
      .installable
      .resolve_or_default(CommandContext::System)?;

    if matches!(installable, Installable::Store { .. }) {
      bail!("Nix doesn't support nix store installables in repl mode.");
    }

    let discovery_hostname =
      resolve_discovery_hostname(self.configuration.as_deref(), None)?;

    let toplevel = toplevel_for(
      installable,
      &self.extra_args,
      self.configuration.clone(),
      discovery_hostname,
    )?;

    let status = NixCommand::new(CommandKind::Repl)
      .args(toplevel.to_args())
      .with_required_env()
      .run_with_logs()?;
    if !status.success() {
      bail!("nix repl failed (exit status {status:?})");
    }

    Ok(())
  }
}

/// Resolve the hostname used for automatic system-manager configuration
/// discovery.
///
/// Returns `Ok(None)` when an explicit `--configuration` is provided.
///
/// # Errors
///
/// Returns an error if the hostname cannot be determined.
fn resolve_discovery_hostname(
  configuration: Option<&str>,
  target_host: Option<&RemoteHost>,
) -> Result<Option<String>> {
  if configuration.is_some() {
    return Ok(None);
  }

  let hostname =
    get_hostname(target_host.map(RemoteHost::hostname).map(ToOwned::to_owned))?;
  Ok(Some(hostname))
}

/// Resolve the Nix installable for a system-manager configuration.
///
/// # Errors
///
/// Returns an error if attribute paths are invalid, `nix eval` checks fail, or
/// no matching `systemConfigs` entry is found.
pub fn toplevel_for<I, S>(
  installable: Installable,
  extra_args: I,
  configuration_name: Option<String>,
  discovery_hostname: Option<String>,
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
      let parsed = parse_attribute(&name)
        .map_err(|err| color_eyre::eyre::eyre!("--configuration {err}"))?;
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
      if attribute.is_empty() {
        attribute.push(String::from("systemConfigs"));
      } else {
        if parsed_configuration.is_some() {
          bail!(
            "Cannot use --configuration together with an installable \
             attribute path"
          );
        }

        if attribute[0] != "systemConfigs" {
          attribute.insert(0, String::from("systemConfigs"));
        }
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
          attribute.extend(resolve_configuration_attr(
            reference,
            &config_attribute,
            &extra_args,
          )?);
        } else {
          let hostname = discovery_hostname.ok_or_else(|| {
            color_eyre::eyre::eyre!(
              "Missing hostname for system-manager configuration discovery"
            )
          })?;
          attribute.extend(discover_system_config(
            reference,
            &extra_args,
            &hostname,
          )?);
        }
      }
    },
    Installable::File {
      ref path,
      ref mut attribute,
      ..
    } => {
      resolve_file_or_expression_attrs(
        Some(path),
        None,
        attribute,
        &mut parsed_configuration,
        &extra_args,
        discovery_hostname,
      )?;
    },
    Installable::Expression {
      ref expression,
      ref mut attribute,
    } => {
      resolve_file_or_expression_attrs(
        None,
        Some(expression.as_str()),
        attribute,
        &mut parsed_configuration,
        &extra_args,
        discovery_hostname,
      )?;
    },
    Installable::Store { .. } => {},
  }

  Ok(res)
}

/// Resolve the attribute path for `--file`/`--expr` installables, mirroring
/// the flake-based discovery logic.
///
/// # Errors
///
/// Returns an error if the attribute path is too specific, a configuration
/// name is combined with an explicit attribute path, or `nix eval` checks
/// fail.
fn resolve_file_or_expression_attrs(
  file_path: Option<&Path>,
  expression: Option<&str>,
  attribute: &mut Vec<String>,
  parsed_configuration: &mut Option<Vec<String>>,
  extra_args: &[OsString],
  discovery_hostname: Option<String>,
) -> Result<()> {
  if attribute.is_empty() {
    attribute.push(String::from("systemConfigs"));
  } else {
    if parsed_configuration.is_some() {
      bail!(
        "Cannot use --configuration together with an installable attribute \
         path"
      );
    }

    if attribute[0] != "systemConfigs" {
      attribute.insert(0, String::from("systemConfigs"));
    }
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
      if !file_or_expression_attr_exists(
        file_path,
        expression,
        &config_attribute,
        extra_args,
      )? {
        bail!(
          "Explicitly provided system-manager configuration not found: \
           systemConfigs.{}",
          config_attribute.join(".")
        );
      }
      attribute.extend(config_attribute);
    } else {
      let hostname = discovery_hostname.ok_or_else(|| {
        color_eyre::eyre::eyre!(
          "Missing hostname for system-manager configuration discovery"
        )
      })?;
      attribute.extend(discover_system_config_in_file(
        file_path, expression, &hostname, extra_args,
      )?);
    }
  }

  Ok(())
}

fn configuration_candidates(hostname: &str) -> [Vec<String>; 2] {
  [vec![hostname.to_owned()], vec![String::from("default")]]
}

/// Discover a matching system-manager configuration in a flake by trying the
/// hostname attribute first, then `default`.
///
/// # Errors
///
/// Returns an error if no suitable configuration is found, or if the
/// underlying `nix eval` checks fail.
fn discover_system_config(
  flake_reference: &str,
  extra_args: &[OsString],
  hostname: &str,
) -> Result<Vec<String>> {
  let candidates = configuration_candidates(hostname);

  for candidate in candidates {
    if let Some(resolved) =
      try_system_config_attr(flake_reference, &candidate, extra_args)?
    {
      debug!(
        "Using inferred system-manager configuration: systemConfigs.{}",
        resolved.join(".")
      );
      return Ok(resolved);
    }
  }

  bail!(
    "No suitable system-manager configuration found automatically. Use \
     --configuration or pass an explicit flake attribute (e.g. '.#default')."
  );
}

/// Discover a matching system-manager configuration in a `--file`/`--expr`
/// installable by trying the hostname attribute first, then `default`.
///
/// # Errors
///
/// Returns an error if no suitable configuration is found, or if the
/// underlying `nix eval` checks fail.
fn discover_system_config_in_file(
  file_path: Option<&Path>,
  expression: Option<&str>,
  hostname: &str,
  extra_args: &[OsString],
) -> Result<Vec<String>> {
  let candidates = configuration_candidates(hostname);

  for candidate in candidates {
    if file_or_expression_attr_exists(
      file_path, expression, &candidate, extra_args,
    )? {
      debug!(
        "Using inferred system-manager configuration: systemConfigs.{}",
        candidate.join(".")
      );
      return Ok(candidate);
    }
  }

  bail!(
    "No suitable system-manager configuration found automatically. Use \
     --configuration or pass an explicit attribute path."
  );
}

/// Resolve an explicitly-provided configuration attribute in a flake.
///
/// # Errors
///
/// Returns an error if the configuration is not found, or if the underlying
/// `nix eval` checks fail.
fn resolve_configuration_attr(
  flake_reference: &str,
  candidate: &[String],
  extra_args: &[OsString],
) -> Result<Vec<String>> {
  if let Some(resolved) =
    try_system_config_attr(flake_reference, candidate, extra_args)?
  {
    return Ok(resolved);
  }

  bail!(
    "Explicitly provided system-manager configuration not found: \
     systemConfigs.{}",
    candidate.join(".")
  );
}

fn render_attr_path(candidate: &[String]) -> String {
  candidate
    .iter()
    .map(|segment| {
      format!("\"{}\"", segment.replace('\\', "\\\\").replace('"', "\\\""))
    })
    .collect::<Vec<_>>()
    .join(".")
}

/// Check whether a configuration attribute exists in a `--file`/`--expr`
/// installable via `nix eval`.
///
/// # Errors
///
/// Returns an error if the `nix eval` command fails to execute.
fn file_or_expression_attr_exists(
  file_path: Option<&Path>,
  expression: Option<&str>,
  candidate: &[String],
  extra_args: &[OsString],
) -> Result<bool> {
  let attr_path_expr = render_attr_path(candidate);

  let mut cmd = NixCommand::new(CommandKind::Eval)
    .with_required_env()
    .args(extra_args)
    .arg("--apply")
    .arg(format!("x: x ? {attr_path_expr}"));

  match (file_path, expression) {
    (Some(path), None) => {
      cmd = cmd.arg("--file").arg(path).arg("systemConfigs");
    },
    (None, Some(expr)) => {
      cmd = cmd.arg("--expr").arg(expr).arg("systemConfigs");
    },
    _ => {
      bail!("Invalid file/expression installable for configuration lookup");
    },
  }

  let check_res = capture_nix_stdout(&cmd).wrap_err(format!(
    "Failed running nix eval to check for system-manager configuration \
     systemConfigs.{}",
    candidate.join(".")
  ))?;

  Ok(check_res.trim() == "true")
}

/// Try to resolve a system-manager configuration attribute in a flake,
/// attempting the current system prefix first, then the bare candidate.
///
/// Returns `Ok(None)` when no attempt matches.
///
/// # Errors
///
/// Returns an error if the underlying `nix eval` checks fail.
fn try_system_config_attr(
  flake_reference: &str,
  candidate: &[String],
  extra_args: &[OsString],
) -> Result<Option<Vec<String>>> {
  let current_system = get_current_system();

  let mut attempts = Vec::new();
  if let Some(system) = current_system {
    let mut scoped = vec![system];
    scoped.extend(candidate.iter().cloned());
    attempts.push(scoped);
  }
  attempts.push(candidate.to_vec());

  for attempt in attempts {
    if flake_attr_exists(flake_reference, &attempt, extra_args)? {
      return Ok(Some(attempt));
    }
  }

  Ok(None)
}

/// Check whether a configuration attribute exists in a flake via `nix eval`.
///
/// # Errors
///
/// Returns an error if the `nix eval` command fails to execute.
fn flake_attr_exists(
  flake_reference: &str,
  candidate: &[String],
  extra_args: &[OsString],
) -> Result<bool> {
  let attr_path_expr = render_attr_path(candidate);

  let check_res = capture_nix_stdout(
    &NixCommand::new(CommandKind::Eval)
      .with_required_env()
      .args(extra_args)
      .arg("--apply")
      .arg(format!("x: x ? {attr_path_expr}"))
      .args(
        (Installable::Flake {
          reference: flake_reference.to_owned(),
          attribute: vec![String::from("systemConfigs")],
        })
        .to_args(),
      ),
  )
  .wrap_err(format!(
    "Failed running nix eval to check for system-manager configuration \
     systemConfigs.{}",
    candidate.join(".")
  ))?;

  Ok(check_res.trim() == "true")
}

fn get_current_system() -> Option<String> {
  let result = capture_nix_stdout(
    &NixCommand::new(CommandKind::Config)
      .with_required_env()
      .args(["show", "system"]),
  );

  match result {
    Ok(system) => {
      let trimmed = system.trim();
      if trimmed.is_empty() {
        None
      } else {
        Some(trimmed.to_owned())
      }
    },
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
#[expect(clippy::expect_used, reason = "Fine in tests")]
mod tests {
  use super::*;

  /// Configuration discovery uses `nix eval`; skip when Nix is not installed.
  fn nix_on_path() -> bool {
    std::process::Command::new("nix")
      .arg("--version")
      .output()
      .is_ok_and(|output| output.status.success())
  }

  #[test]
  fn test_configuration_candidates_preserve_literal_hostname() {
    assert_eq!(configuration_candidates("host.example.com"), [
      vec![String::from("host.example.com")],
      vec![String::from("default")],
    ]);
  }

  #[test]
  fn test_render_attr_path_quotes_and_escapes_segments() {
    assert_eq!(
      render_attr_path(&[
        String::from("host.example.com"),
        String::from("quoted\"name"),
      ]),
      "\"host.example.com\".\"quoted\\\"name\""
    );
  }

  #[test]
  fn test_toplevel_for_file_with_explicit_configuration() {
    if !nix_on_path() {
      return;
    }
    let path = PathBuf::from(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../test/system.nix"
    ));
    let installable = Installable::File {
      path:      path.clone(),
      attribute: vec![],
    };

    let result = toplevel_for(
      installable,
      Vec::<String>::new(),
      Some(String::from("default")),
      None,
    )
    .expect("toplevel_for should succeed");

    assert_eq!(result.to_args(), vec![
      String::from("--file"),
      path.to_string_lossy().into_owned(),
      String::from("systemConfigs.default"),
    ]);
  }

  #[test]
  fn test_toplevel_for_flake_short_configuration_name() {
    let installable = Installable::Flake {
      reference: String::from("."),
      attribute: vec![String::from("default")],
    };

    let result = toplevel_for(
      installable,
      Vec::<String>::new(),
      None,
      Some(String::from("unused")),
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

    let result = toplevel_for(
      installable,
      Vec::<String>::new(),
      None,
      Some(String::from("unused")),
    )
    .expect("toplevel_for should succeed");

    assert_eq!(result.to_args(), vec![String::from(
      ".#systemConfigs.x86_64-linux.default"
    )]);
  }

  #[test]
  fn test_toplevel_for_file_defaults_to_default_configuration() {
    if !nix_on_path() {
      return;
    }

    let path = PathBuf::from(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../test/system.nix"
    ));
    let installable = Installable::File {
      path:      path.clone(),
      attribute: vec![],
    };

    let result = toplevel_for(
      installable,
      Vec::<String>::new(),
      None,
      Some(String::from("missing-host")),
    )
    .expect("toplevel_for should succeed");

    assert_eq!(result.to_args(), vec![
      String::from("--file"),
      path.to_string_lossy().into_owned(),
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
        None,
      )
      .is_err()
    );
  }
}
