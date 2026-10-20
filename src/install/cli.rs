//! Command-line glue for the mutating commands: flag parsing, real services,
//! output. Everything of substance lives in [`crate::install::engine`].

use crate::install::engine::{self, Context, Operation, Options, PathMode, RealProbe, Services};
use crate::install::layout::{EnvVars, Layout};
use crate::install::legacy;
use crate::install::plugins::RealClaude;
use crate::install::source;
use crate::install::verify::CosignVerifier;
use crate::install::virtualized::{self, RealVirtualizationProbe, VirtualizationProbe};
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
    /// `--allow-virtualized-root`: install even where the probe shows the root is virtualized.
    pub allow_virtualized_root: bool,
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
                "--allow-virtualized-root",
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
    Ok(Parsed {
        options,
        mirror,
        allow_virtualized_root: flag(args, "--allow-virtualized-root")?,
    })
}

/// The directories whose virtualization matters: the root, and the bin
/// directory when it is not inside the root (macOS/Linux).
pub fn probe_targets(layout: &Layout) -> Vec<&std::path::Path> {
    let mut dirs = vec![layout.root.as_path()];
    if !virtualized::is_under(&layout.bin, &layout.root) {
        dirs.push(layout.bin.as_path());
    }
    dirs
}

/// Refuse to install or update into a root that Windows would silently
/// virtualize (see [`crate::install::virtualized`]), unless explicitly
/// allowed (`--allow-virtualized-root` or `SOTHIS_ALLOW_VIRTUALIZED_ROOT`).
/// The probe runs before anything else is written (it creates and removes one
/// uniquely named file). Uninstall and rollback stay possible so a v0.1.0
/// install can still be removed.
pub fn guard_virtualized_root(
    operation: Operation,
    allow: bool,
    layout: &Layout,
    probe: &dyn VirtualizationProbe,
) -> CliResult {
    if allow || !matches!(operation, Operation::Install | Operation::Update) {
        return Ok(());
    }
    match virtualized::virtualized_problem(&probe_targets(layout), probe) {
        Some(reason) => Err((
            1,
            format!(
                "refusing to {}: {reason}. Nothing was installed. (Pass --allow-virtualized-root only if you know what you are doing.)",
                operation.name()
            ),
        )),
        None => Ok(()),
    }
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
    let process_env = EnvVars::from_process();
    let layout = Layout::resolve_from(host, &process_env).map_err(|e| (1, e))?;
    let allow_virtualized = parsed.allow_virtualized_root
        || std::env::var_os(virtualized::ALLOW_ENV).is_some_and(|value| !value.is_empty());
    guard_virtualized_root(
        operation,
        allow_virtualized,
        &layout,
        &RealVirtualizationProbe,
    )?;
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
        legacy_root: legacy::legacy_root(host, &process_env),
        legacy_path_entries: legacy::legacy_path_entries(host, &process_env),
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
    fn allow_virtualized_root_is_an_install_and_update_flag_only() {
        let parsed = parse(
            Operation::Install,
            &args(&["--allow-virtualized-root"]),
            &resolve,
        )
        .unwrap();
        assert!(parsed.allow_virtualized_root);
        assert!(
            !parse(Operation::Update, &[], &resolve)
                .unwrap()
                .allow_virtualized_root
        );
        assert!(parse(
            Operation::Uninstall,
            &args(&["--allow-virtualized-root"]),
            &resolve
        )
        .is_err());
    }

    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", path.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{path}"))
        }
    }

    fn layout_at(root: PathBuf) -> Layout {
        Layout {
            bin: root.join("bin"),
            root,
        }
    }

    #[test]
    fn a_virtualized_install_into_appdata_is_refused_unless_allowed() {
        let probe = virtualized::FakeProbe::virtualizing(
            abs("Users/t/AppData/Local"),
            "Claude_pzs8sxrjxfjjc",
        );
        let layout = layout_at(abs("Users/t/AppData/Local/Sothis"));
        for operation in [Operation::Install, Operation::Update] {
            let (code, message) =
                guard_virtualized_root(operation, false, &layout, &probe).unwrap_err();
            assert_eq!(code, 1);
            assert!(message.contains("Nothing was installed"), "{message}");
            assert!(message.contains("Claude_pzs8sxrjxfjjc"), "{message}");
            assert!(message.contains("--allow-virtualized-root"), "{message}");
        }
        let allowed = virtualized::FakeProbe::virtualizing(abs("Users/t/AppData/Local"), "P");
        assert!(guard_virtualized_root(Operation::Install, true, &layout, &allowed).is_ok());
        assert!(
            allowed.probed.borrow().is_empty(),
            "an allowed install does not even probe"
        );
        // Removing an old install stays possible, and needs no probe.
        for operation in [Operation::Uninstall, Operation::Rollback] {
            assert!(guard_virtualized_root(operation, false, &layout, &probe).is_ok());
        }
    }

    #[test]
    fn the_guard_passes_for_a_direct_root_and_when_the_probe_is_unknown() {
        let probe = virtualized::FakeProbe::virtualizing(abs("Users/t/AppData"), "P");
        let safe = layout_at(abs("Users/t/.sothis"));
        assert!(guard_virtualized_root(Operation::Install, false, &safe, &probe).is_ok());
        assert_eq!(
            *probe.probed.borrow(),
            [abs("Users/t/.sothis")],
            "root only"
        );

        let unknown = virtualized::FakeProbe {
            unknown: true,
            ..virtualized::FakeProbe::direct()
        };
        let appdata = layout_at(abs("Users/t/AppData/Roaming/x"));
        assert!(guard_virtualized_root(Operation::Install, false, &appdata, &unknown).is_ok());
    }

    #[test]
    fn a_bin_directory_outside_the_root_is_probed_too() {
        let probe = virtualized::FakeProbe::virtualizing(abs("Users/t/AppData/Local/bin"), "P");
        let layout = Layout {
            root: abs("Users/t/.sothis"),
            bin: abs("Users/t/AppData/Local/bin"),
        };
        assert!(guard_virtualized_root(Operation::Install, false, &layout, &probe).is_err());
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
