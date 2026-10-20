mod doctor;
mod install;
mod manifest;
mod plan;
mod platform;
mod version;
mod workspace;

use install::engine::Operation;
use serde_json::json;
use std::path::PathBuf;

fn main() {
    let json_requested = std::env::args().any(|arg| arg == "--json");
    if let Err((code, message)) = run() {
        if !message.is_empty() {
            if json_requested {
                let payload = json!({ "ok": false, "error": message, "exitCode": code });
                println!("{payload}");
            } else {
                eprintln!("sothis: {message}");
            }
        }
        std::process::exit(code);
    }
}

fn run() -> Result<(), (i32, String)> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let json_output = take_flag(&mut args, "--json");
    if args.is_empty() || take_flag(&mut args, "--help") || take_flag(&mut args, "-h") {
        print_help();
        return Ok(());
    }
    if take_flag(&mut args, "--version") || take_flag(&mut args, "-V") {
        println!("sothis {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let command = args.remove(0);
    let manifest = manifest::embedded().map_err(|e| (1, e))?;
    match command.as_str() {
        "manifest" => command_manifest(&manifest, &args, json_output),
        "doctor" => command_doctor(&manifest, &args, json_output),
        "install" => command_mutating(&manifest, Operation::Install, &args, json_output),
        "update" => command_mutating(&manifest, Operation::Update, &args, json_output),
        "uninstall" => command_mutating(&manifest, Operation::Uninstall, &args, json_output),
        "rollback" => {
            install::cli::run_mutation(&manifest, Operation::Rollback, &args, json_output)
        }
        "env" => command_env(&args, json_output),
        "init" => command_init(&args, json_output),
        other => Err((2, format!("unknown command {other:?}; run sothis --help"))),
    }
}

fn command_manifest(
    manifest: &manifest::Manifest,
    args: &[String],
    json_output: bool,
) -> Result<(), (i32, String)> {
    let action = args.first().map(String::as_str).unwrap_or("validate");
    if args.len() > 1 {
        return Err((2, format!("unexpected manifest argument: {}", args[1])));
    }
    match action {
        "validate" => {
            let errors = manifest::validate(manifest);
            if json_output {
                print_json(&json!({
                    "ok": errors.is_empty(),
                    "schemaVersion": manifest.schema_version,
                    "suiteVersion": manifest.suite_version,
                    "errors": errors
                }))?;
            } else if errors.is_empty() {
                println!(
                    "manifest valid: schema v{}, suite {} ({} tools, {} profiles)",
                    manifest.schema_version,
                    manifest.suite_version,
                    manifest.tools.len(),
                    manifest.profiles.len()
                );
            } else {
                for error in &errors {
                    eprintln!("- {error}");
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err((
                    1,
                    if json_output {
                        String::new()
                    } else {
                        "manifest validation failed".to_string()
                    },
                ))
            }
        }
        "show" => {
            print_json(manifest)?;
            Ok(())
        }
        other => Err((2, format!("unknown manifest action {other:?}"))),
    }
}

fn command_doctor(
    manifest: &manifest::Manifest,
    args: &[String],
    json_output: bool,
) -> Result<(), (i32, String)> {
    let requested_profile = option_value(args, "--profile")?;
    reject_unknown_args(args, &["--profile"])?;
    let host = platform::HostPlatform::detect().map_err(|e| (1, e))?;
    let cwd = std::env::current_dir().map_err(|e| (1, format!("could not read cwd: {e}")))?;
    let profile = resolve_profile(manifest, requested_profile, &cwd)?;
    let checks = doctor::run(manifest, &profile, host.key()).map_err(|e| (1, e))?;
    let ok = checks.iter().all(doctor::Check::ok);
    if json_output {
        print_json(&json!({
            "ok": ok,
            "tool": "sothis",
            "version": env!("CARGO_PKG_VERSION"),
            "platform": host.key(),
            "profile": profile,
            "checks": checks
        }))?;
    } else {
        for check in &checks {
            println!(
                "[{}] {} — {}",
                check.status.label(),
                check.name,
                check.detail
            );
        }
    }
    if ok {
        Ok(())
    } else {
        Err((
            1,
            if json_output {
                String::new()
            } else {
                format!("doctor found an incomplete {profile} installation")
            },
        ))
    }
}

/// `install`/`update`/`uninstall`: `--plan` is the dry run; without it the
/// staged, verified install engine runs.
fn command_mutating(
    manifest: &manifest::Manifest,
    operation: Operation,
    args: &[String],
    json_output: bool,
) -> Result<(), (i32, String)> {
    if args.iter().any(|arg| arg == "--plan") {
        command_plan(manifest, operation.name(), args, json_output)
    } else {
        install::cli::run_mutation(manifest, operation, args, json_output)
    }
}

fn command_plan(
    manifest: &manifest::Manifest,
    operation: &str,
    args: &[String],
    json_output: bool,
) -> Result<(), (i32, String)> {
    let plan_count = args.iter().filter(|arg| arg.as_str() == "--plan").count();
    if plan_count > 1 {
        return Err((2, "--plan may be supplied only once".to_string()));
    }
    let requested_profile = option_value(args, "--profile")?;
    let platform = match option_value(args, "--platform")? {
        Some(platform) => platform,
        None => platform::HostPlatform::detect()
            .map_err(|e| (1, e))?
            .key()
            .to_string(),
    };
    reject_unknown_args(args, &["--plan", "--profile", "--platform", "--no-plugins"])?;
    let cwd = std::env::current_dir().map_err(|e| (1, format!("could not read cwd: {e}")))?;
    let profile = resolve_profile(manifest, requested_profile, &cwd)?;
    let mut items = plan::build(manifest, &profile, &platform).map_err(|e| (2, e))?;
    plan::apply_operation(&mut items, operation).map_err(|e| (2, e))?;
    if operation == "uninstall" {
        items.reverse();
    }
    // `update` also refreshes installed suite plugins (read-only lookup here).
    let plugin_steps = if operation == "update" && !args.iter().any(|arg| arg == "--no-plugins") {
        let state = install::plugins::load_state(doctor::claude_plugins_dir().as_deref())
            .unwrap_or_default();
        let targets = install::plugins::update_targets(manifest, &state);
        install::plugins::update_commands(&manifest.marketplace.canonical.name, &targets)
    } else {
        Vec::new()
    };
    if json_output {
        print_json(&json!({
            "ok": true,
            "operation": operation,
            "profile": profile,
            "platform": platform,
            "dryRun": true,
            "items": items,
            "pluginSteps": plugin_steps
        }))?;
    } else {
        println!("{operation} plan: profile={profile}, platform={platform}");
        for item in items {
            println!("- {:?}: {} [{}]", item.action, item.name, item.support);
            if let Some(note) = item.note {
                println!("  {note}");
            }
        }
        if !plugin_steps.is_empty() {
            println!("plugin steps:");
            for step in &plugin_steps {
                println!("- {step}");
            }
        }
        println!("dry run only; no machine state changed");
    }
    Ok(())
}

fn command_env(args: &[String], json_output: bool) -> Result<(), (i32, String)> {
    reject_unknown_args(args, &[])?;
    let host = platform::HostPlatform::detect().map_err(|e| (1, e))?;
    let layout = install::layout::Layout::resolve(host).map_err(|e| (1, e))?;
    if json_output {
        print_json(&json!({
            "platform": host.key(),
            "root": layout.root,
            "installDir": layout.bin,
            "pathChangeRequiresNewProcess": true
        }))?;
    } else {
        println!("platform: {}", host.key());
        println!("sothis root: {}", layout.root.display());
        println!("install directory: {}", layout.bin.display());
        println!("PATH changes are visible only to new processes.");
    }
    Ok(())
}

fn command_init(args: &[String], json_output: bool) -> Result<(), (i32, String)> {
    let root = match args {
        [] => std::env::current_dir().map_err(|e| (1, format!("could not read cwd: {e}")))?,
        [path] => PathBuf::from(path),
        _ => return Err((2, "usage: sothis init [path] [--json]".to_string())),
    };
    let result = workspace::init(&root).map_err(|e| (1, e))?;
    if json_output {
        print_json(&json!({ "ok": true, "result": result }))?;
    } else {
        println!("Sothis workspace: {}", result.root.display());
        println!(
            "config: {} ({})",
            result.config.display(),
            if result.created_config {
                "created"
            } else {
                "already present"
            }
        );
    }
    Ok(())
}

/// Resolve the effective profile the same way for doctor and the plan
/// commands: an explicit --profile value wins, then the .suite/config.json
/// found from `cwd`, then "core". An unknown profile is a usage error
/// (exit 2) when it came from the flag and an environment error (exit 1)
/// when it came from the workspace config.
fn resolve_profile(
    manifest: &manifest::Manifest,
    requested: Option<String>,
    cwd: &std::path::Path,
) -> Result<String, (i32, String)> {
    let explicit = requested.is_some();
    let profile = match requested {
        Some(profile) => profile,
        None => workspace::configured_profile(cwd)
            .map_err(|e| (1, e))?
            .unwrap_or_else(|| "core".to_string()),
    };
    if !manifest.profiles.contains_key(&profile) {
        let source = if explicit {
            "--profile"
        } else {
            ".suite/config.json profile"
        };
        return Err((
            if explicit { 2 } else { 1 },
            format!("{source} selects unknown profile {profile:?}"),
        ));
    }
    Ok(profile)
}

fn print_json(value: &impl serde::Serialize) -> Result<(), (i32, String)> {
    let output = serde_json::to_string_pretty(value)
        .map_err(|e| (1, format!("could not serialize output: {e}")))?;
    println!("{output}");
    Ok(())
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        args.remove(index);
        true
    } else {
        false
    }
}

fn option_value(args: &[String], name: &str) -> Result<Option<String>, (i32, String)> {
    let positions = args
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| (arg == name).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        return Err((2, format!("{name} may be supplied only once")));
    }
    let Some(index) = positions.first().copied() else {
        return Ok(None);
    };
    let value = args
        .get(index + 1)
        .ok_or_else(|| (2, format!("{name} requires a value")))?;
    if value.starts_with('-') {
        return Err((
            2,
            format!("{name} requires a value, but received option {value}"),
        ));
    }
    Ok(Some(value.clone()))
}

fn reject_unknown_args(args: &[String], allowed: &[&str]) -> Result<(), (i32, String)> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !allowed.contains(&arg.as_str()) {
            return Err((2, format!("unknown argument: {arg}")));
        }
        if arg == "--profile" || arg == "--platform" {
            index += 1;
            if index >= args.len() {
                return Err((2, format!("{arg} requires a value")));
            }
        }
        index += 1;
    }
    Ok(())
}

fn print_help() {
    println!(
        "Sothis — suite distribution and compatibility

Usage:
  sothis <command> [options]

Commands:
  manifest validate|show        Validate or print the embedded suite contract
  doctor [--profile P]          Check a profile and Sirius integration contracts
  install [--profile P]         Install the profile's tools (staged, checksum + Sigstore verified)
  update [--profile P]          Move managed tools to the recommended versions
  uninstall [--profile P]       Remove Sothis-owned files in reverse order
  rollback <tool>               Return a tool to its previous retained version
  env                           Show the Sothis root and install location
  init [path]                   Initialize idempotent project-level .suite config

install/update/uninstall --plan resolves a dry-run plan and changes nothing
(--platform P may override the platform for a plan only).

install/update options:
  --mirror <dir>                Read assets from <dir>/<owner>/<repo>/v<version>/<asset>
                                (also SOTHIS_RELEASE_MIRROR) instead of GitHub
  --allow-missing-cosign        Proceed with a loud warning if cosign is not installed
                                (a cosign that runs and fails is always fatal)
  --allow-downgrade             Allow replacing a newer installed version
  --no-modify-path              Do not edit the user PATH
  --no-plugins                  Do not install Claude Code plugins
uninstall options:
  --plugins                     Also uninstall the Claude Code plugins
  --no-modify-path              Leave the user PATH alone

Global options:
  --json                        Emit machine-readable JSON
  --version                     Print the CLI version

Environment:
  SOTHIS_HOME                   Absolute directory that replaces the whole install layout
                                (root and <root>/bin); use it for tests and trials
  SOTHIS_RELEASE_MIRROR         Default for --mirror
  SOTHIS_COSIGN                 Absolute path to the cosign binary

Profiles: core, docs, bell, eval, full"
    );
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_parser_handles_profile() {
        let args = vec![
            "--plan".to_string(),
            "--profile".to_string(),
            "full".to_string(),
        ];
        assert_eq!(
            option_value(&args, "--profile").unwrap().as_deref(),
            Some("full")
        );
        assert!(reject_unknown_args(&args, &["--plan", "--profile"]).is_ok());
    }

    #[test]
    fn unknown_options_are_rejected() {
        let args = vec!["--plaen".to_string()];
        assert!(reject_unknown_args(&args, &["--plan"]).is_err());
    }

    #[test]
    fn profile_resolution_prefers_flag_then_config_then_core() {
        let manifest = manifest::embedded().unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sothis-resolve-profile-{nonce}"));
        std::fs::create_dir_all(&root).unwrap();

        // No config anywhere under a temp root: default is core.
        assert_eq!(
            resolve_profile(&manifest, None, &root).unwrap(),
            "core".to_string()
        );

        // The flag wins over everything.
        assert_eq!(
            resolve_profile(&manifest, Some("full".to_string()), &root).unwrap(),
            "full".to_string()
        );

        // A .suite/config.json profile is honored when no flag is given.
        workspace::init(&root).unwrap();
        std::fs::write(
            root.join(".suite").join("config.json"),
            "{\n  \"schemaVersion\": 1,\n  \"profile\": \"docs\"\n}\n",
        )
        .unwrap();
        assert_eq!(
            resolve_profile(&manifest, None, &root).unwrap(),
            "docs".to_string()
        );

        // Unknown profile from the flag is a usage error (exit 2)...
        let (code, message) =
            resolve_profile(&manifest, Some("nope".to_string()), &root).unwrap_err();
        assert_eq!(code, 2);
        assert!(message.contains("--profile"));

        // ...and from the workspace config an environment error (exit 1).
        std::fs::write(
            root.join(".suite").join("config.json"),
            "{\n  \"schemaVersion\": 1,\n  \"profile\": \"nope\"\n}\n",
        )
        .unwrap();
        let (code, message) = resolve_profile(&manifest, None, &root).unwrap_err();
        assert_eq!(code, 1);
        assert!(message.contains(".suite/config.json"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_and_missing_option_values_are_rejected() {
        let duplicate = vec![
            "--profile".to_string(),
            "core".to_string(),
            "--profile".to_string(),
            "full".to_string(),
        ];
        assert!(option_value(&duplicate, "--profile").is_err());

        let missing = vec!["--profile".to_string(), "--plan".to_string()];
        assert!(option_value(&missing, "--profile").is_err());
    }
}
