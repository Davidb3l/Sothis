//! Command-line glue for the mutating commands: flag parsing, real services,
//! output. Everything of substance lives in [`crate::install::engine`].

use crate::install::engine::{self, Context, Operation, Options, PathMode, RealProbe, Services};
use crate::install::layout::Layout;
use crate::install::plugins::RealClaude;
use crate::install::source;
use crate::install::verify::CosignVerifier;
use crate::manifest::Manifest;
use crate::platform::{HostOs, HostPlatform};
use std::path::PathBuf;

pub const MUTATION_ENV_MIRROR: &str = "SOTHIS_RELEASE_MIRROR";

type CliResult = Result<(), (i32, String)>;

/// Parsed, validated flags of a mutating command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub options: Options,
    pub mirror: Option<PathBuf>,
}

fn flag(args: &[String], name: &str) -> Result<bool, (i32, String)> {
    match args.iter().filter(|arg| arg.as_str() == name).count() {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err((2, format!("{name} may be supplied only once"))),
    }
}

/// Parse the arguments of `install`, `update`, `uninstall` or `rollback`
/// (without `--plan`). `resolve_profile` is injected so tests need no cwd.
pub fn parse(
    operation: Operation,
    args: &[String],
    resolve_profile: &dyn Fn(Option<String>) -> Result<String, (i32, String)>,
) -> Result<Parsed, (i32, String)> {
    let (allowed, value_options): (&[&str], &[&str]) = match operation {
        Operation::Install | Operation::Update => (
            &[
                "--profile",
                "--mirror",
                "--allow-missing-cosign",
                "--allow-downgrade",
                "--no-modify-path",
                "--no-plugins",
            ],
            &["--profile", "--mirror"],
        ),
        Operation::Uninstall => (
            &["--profile", "--plugins", "--no-modify-path"],
            &["--profile"],
        ),
        Operation::Rollback => (&[], &[]),
    };

    // Split positionals from options.
    let mut positionals = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg.starts_with('-') {
            if !allowed.contains(&arg.as_str()) {
                return Err((2, format!("unknown argument: {arg}")));
            }
            if value_options.contains(&arg.as_str()) {
                index += 1;
                if index >= args.len() {
                    return Err((2, format!("{arg} requires a value")));
                }
            }
        } else {
            positionals.push(arg.clone());
        }
        index += 1;
    }

    let rollback_tool = if operation == Operation::Rollback {
        match positionals.as_slice() {
            [tool] => Some(tool.clone()),
            _ => return Err((2, "usage: sothis rollback <tool> [--json]".to_string())),
        }
    } else {
        if let Some(extra) = positionals.first() {
            return Err((2, format!("unexpected argument: {extra}")));
        }
        None
    };

    let profile_flag = crate::option_value(args, "--profile")?;
    let mirror = crate::option_value(args, "--mirror")?.map(PathBuf::from);
    let profile = if operation == Operation::Rollback {
        "n/a".to_string()
    } else {
        resolve_profile(profile_flag)?
    };
    let no_plugins = flag(args, "--no-plugins")?;
    let options = Options {
        operation,
        profile,
        allow_missing_cosign: flag(args, "--allow-missing-cosign")?,
        allow_downgrade: flag(args, "--allow-downgrade")?,
        modify_path: !flag(args, "--no-modify-path")?,
        plugins: match operation {
            Operation::Uninstall => flag(args, "--plugins")?,
            _ => !no_plugins,
        },
        rollback_tool,
    };
    Ok(Parsed { options, mirror })
}

pub fn run_mutation(
    manifest: &Manifest,
    operation: Operation,
    args: &[String],
    json_output: bool,
) -> CliResult {
    let cwd = std::env::current_dir().map_err(|e| (1, format!("could not read cwd: {e}")))?;
    let parsed = parse(operation, args, &|requested| {
        crate::resolve_profile(manifest, requested, &cwd)
    })?;
    if let Some(tool) = &parsed.options.rollback_tool {
        if !manifest.tools.iter().any(|candidate| &candidate.id == tool) {
            return Err((2, format!("unknown tool {tool:?}")));
        }
    }
    let errors = crate::manifest::validate(manifest);
    if !errors.is_empty() {
        return Err((1, format!("manifest is invalid: {}", errors.join("; "))));
    }

    let host = HostPlatform::detect().map_err(|e| (1, e))?;
    let layout = Layout::resolve(host).map_err(|e| (1, e))?;
    let env_mirror = std::env::var_os(MUTATION_ENV_MIRROR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let fetcher = source::select_fetcher(parsed.mirror.as_deref(), env_mirror.as_deref(), &cwd)
        .map_err(|e| (1, e))?;
    let claude = RealClaude::detect();
    let expand_env = |name: &str| std::env::var(name).ok();
    let find_command = |name: &str| crate::doctor::find_command(name);
    let path_dirs = std::env::var_os("PATH")
        .map(|value| crate::install::pathenv::split_path_var(&value))
        .unwrap_or_default();

    #[cfg(windows)]
    let mut registry = crate::install::pathenv::RegistryPathStore;
    #[cfg(windows)]
    let path_mode = PathMode::Store(&mut registry);
    #[cfg(not(windows))]
    let path_mode = PathMode::Advisory;

    let ctx = Context {
        manifest,
        layout: &layout,
        platform: host.key(),
        exe_suffix: if host.os == HostOs::Windows {
            ".exe"
        } else {
            ""
        },
    };
    let services = Services {
        fetcher: fetcher.as_ref(),
        verifier: &CosignVerifier,
        probe: &RealProbe,
        claude: &claude,
        path: path_mode,
        plugin_state_dir: crate::doctor::claude_plugins_dir(),
        path_dirs,
        expand_env: &expand_env,
        find_command: &find_command,
    };
    if !json_output && operation != Operation::Rollback {
        eprintln!(
            "sothis {}: profile={}, platform={}, root={}, source={}",
            operation.name(),
            parsed.options.profile,
            host.key(),
            layout.root.display(),
            fetcher.describe()
        );
    }
    let summary = engine::run(&ctx, &parsed.options, services).map_err(|e| (1, e))?;
    if json_output {
        crate::print_json(&summary)?;
    } else {
        print!("{}", summary.render_human());
    }
    if summary.ok {
        Ok(())
    } else {
        Err((
            1,
            if json_output {
                String::new()
            } else {
                format!(
                    "{} did not complete; see the failed steps above",
                    operation.name()
                )
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn resolve(requested: Option<String>) -> Result<String, (i32, String)> {
        Ok(requested.unwrap_or_else(|| "core".to_string()))
    }

    #[test]
    fn install_flags_map_to_options() {
        let parsed = parse(
            Operation::Install,
            &args(&[
                "--profile",
                "full",
                "--mirror",
                "m",
                "--allow-missing-cosign",
                "--no-modify-path",
                "--no-plugins",
            ]),
            &resolve,
        )
        .unwrap();
        assert_eq!(parsed.options.profile, "full");
        assert!(parsed.options.allow_missing_cosign);
        assert!(!parsed.options.modify_path);
        assert!(!parsed.options.plugins);
        assert!(!parsed.options.allow_downgrade);
        assert_eq!(parsed.mirror, Some(PathBuf::from("m")));

        let defaults = parse(Operation::Update, &[], &resolve).unwrap();
        assert_eq!(defaults.options.profile, "core");
        assert!(defaults.options.modify_path);
        assert!(defaults.options.plugins);
        assert!(!defaults.options.allow_missing_cosign);
    }

    #[test]
    fn uninstall_plugins_flag_is_opt_in() {
        let default = parse(Operation::Uninstall, &[], &resolve).unwrap();
        assert!(!default.options.plugins);
        let with = parse(Operation::Uninstall, &args(&["--plugins"]), &resolve).unwrap();
        assert!(with.options.plugins);
        assert!(parse(Operation::Uninstall, &args(&["--no-plugins"]), &resolve).is_err());
        assert!(parse(Operation::Uninstall, &args(&["--mirror", "x"]), &resolve).is_err());
    }

    #[test]
    fn usage_errors_exit_two() {
        for (operation, list) in [
            (Operation::Install, vec!["--bogus"]),
            (Operation::Install, vec!["--profile"]),
            (Operation::Install, vec!["--mirror"]),
            (Operation::Install, vec!["stray"]),
            (Operation::Install, vec!["--no-plugins", "--no-plugins"]),
            (Operation::Install, vec!["--platform", "linux-x64"]),
            (Operation::Install, vec!["--profile", "a", "--profile", "b"]),
            (Operation::Update, vec!["--plugins"]),
            (Operation::Rollback, vec![]),
            (Operation::Rollback, vec!["a", "b"]),
            (Operation::Rollback, vec!["a", "--profile", "core"]),
        ] {
            let error = parse(operation, &args(&list), &resolve).unwrap_err();
            assert_eq!(error.0, 2, "{operation:?} {list:?}: {}", error.1);
        }
    }

    #[test]
    fn rollback_takes_exactly_one_tool() {
        let parsed = parse(Operation::Rollback, &args(&["hayvenhurst"]), &resolve).unwrap();
        assert_eq!(parsed.options.rollback_tool.as_deref(), Some("hayvenhurst"));
    }

    #[test]
    fn a_mirror_path_is_not_mistaken_for_a_positional() {
        let parsed = parse(
            Operation::Install,
            &args(&["--mirror", "some/dir"]),
            &resolve,
        )
        .unwrap();
        assert_eq!(parsed.mirror, Some(PathBuf::from("some/dir")));
    }
}
