//! `uvr activate` — put the project's isolated R environment into the
//! current shell, so a bare `R` or `Rscript` uses the project without an
//! `uvr run` prefix.
//!
//! # Why a shim that calls back into uvr
//!
//! Python's `.venv/bin/activate` can bake absolute paths into a file because
//! the venv *is* the interpreter — it cannot change underneath the script. A
//! uvr environment is computed from `uvr.toml` and `.r-version`, which the
//! user edits (`uvr r use`, `uvr r pin`). A generated script with paths baked
//! in would keep pointing at the old R after such an edit.
//!
//! So `.uvr/activate` holds no paths at all. It asks the binary to recompute
//! the environment every time it is sourced, which makes staleness
//! structurally impossible rather than a thing to remember.

use std::path::Path;

use anyhow::{Context, Result};

use uvr_core::project::{Project, DOT_UVR_DIR};
use uvr_core::r_env::{REnv, PATH_SEP};
use uvr_core::r_version::detector::find_r_binary;

use crate::ui;
use crate::ui::palette;

/// Shells `uvr activate --emit` can generate code for. Same set `uvr
/// completions` supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ActivateShell {
    Sh,
    Bash,
    Zsh,
    Fish,
    Powershell,
}

/// Name of the POSIX shim inside `.uvr/`.
pub const SHIM_SH: &str = "activate";
/// Name of the fish shim inside `.uvr/`.
pub const SHIM_FISH: &str = "activate.fish";
/// Name of the PowerShell shim inside `.uvr/`.
///
/// Lowercase on purpose: Windows filenames are case-insensitive, so
/// `. .uvr/activate.ps1` works either way, and a single `.uvr/activate*`
/// gitignore entry then covers all three shims.
pub const SHIM_PS1: &str = "activate.ps1";

/// Environment variables activation takes over, and therefore must save and
/// restore. `PATH` is handled specially (it is prepended to, not replaced);
/// the rest are set to literal values from [`REnv::vars`]. `UVR_PROJECT` is
/// uvr's own marker and is simply unset on deactivate.
fn managed_vars(env: &REnv) -> Vec<&'static str> {
    let mut names = vec!["PATH"];
    names.extend(env.vars().iter().map(|(k, _)| *k));
    names.push("UVR_PROJECT");
    names
}

pub fn run(emit: Option<ActivateShell>, write_shim: bool) -> Result<()> {
    if write_shim {
        let project = find_project()?;
        write_shims(&project.root)?;
        ui::success("Wrote activation shims");
        for name in [SHIM_SH, SHIM_FISH, SHIM_PS1] {
            println!("  {}", palette::dim(format!("{DOT_UVR_DIR}/{name}")));
        }
        return Ok(());
    }

    if let Some(shell) = emit {
        let (name, env) = resolve()?;
        print!("{}", emit_script(shell, &name, &env));
        return Ok(());
    }

    // No flags: tell a human what to type. Emitting shell code here would be
    // hostile — the user would get a wall of exports printed to the terminal.
    let project = find_project()?;
    if !project.root.join(DOT_UVR_DIR).join(SHIM_SH).exists() {
        write_shims(&project.root)?;
    }
    ui::info(format!(
        "Activate this project with:\n\n    source {DOT_UVR_DIR}/{SHIM_SH}\n\nThen run `deactivate` to restore your shell."
    ));
    Ok(())
}

fn find_project() -> Result<Project> {
    Project::find_cwd()
        .map_err(|_| anyhow::anyhow!("Not inside a uvr project — no uvr.toml found."))
}

/// Resolve the project name and its isolated environment.
///
/// Deliberately re-resolves R on every call: that is what keeps a sourced
/// shim correct after `uvr r use` / `uvr r pin`.
fn resolve() -> Result<(String, REnv)> {
    let project = find_project()?;
    project
        .ensure_library_dir()
        .context("Failed to create .uvr/library/")?;

    // Matches `uvr run`: the constraint comes from uvr.toml, and
    // `find_r_binary` itself honours a `.r-version` pin walked up from cwd.
    let r_binary = find_r_binary(project.manifest.project.r_version.as_deref())
        .context("R not found. Install R or use `uvr r install <version>`")?;

    let env = REnv {
        r_binary,
        library: project.library_path(),
        with_library: None,
        extra_libs: uvr_core::env_vars::extra_libs(),
    };
    Ok((project.manifest.project.name.clone(), env))
}

fn emit_script(shell: ActivateShell, project: &str, env: &REnv) -> String {
    match shell {
        ActivateShell::Sh | ActivateShell::Bash | ActivateShell::Zsh => emit_posix(project, env),
        ActivateShell::Fish => emit_fish(project, env),
        ActivateShell::Powershell => emit_powershell(project, env),
    }
}

/// Single-quote a value for POSIX shells, escaping embedded single quotes.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Single-quote for fish, which honours `\\` and `\'` inside single quotes.
fn fish_quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
}

/// Single-quote for PowerShell, where an embedded quote is doubled.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn emit_posix(project: &str, env: &REnv) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# uvr activation for project {project} — generated by `uvr activate --emit sh`.\n"
    ));

    // Unwind any previous activation first, so re-activating is idempotent
    // rather than stacking a second copy of the R bin dir onto PATH.
    out.push_str("if command -v deactivate >/dev/null 2>&1; then deactivate; fi\n");

    // Save prior state. The `${VAR+1}` marker distinguishes "was set to
    // empty" from "was never set", so deactivate can restore either exactly.
    for name in managed_vars(env) {
        out.push_str(&format!(
            "UVR_OLD_{name}=\"${{{name}-}}\"; UVR_OLD_{name}_SET=\"${{{name}+1}}\"; \
             export UVR_OLD_{name} UVR_OLD_{name}_SET\n"
        ));
    }

    // Apply. PATH is prepended so the project's R wins over any system R.
    out.push_str(&format!(
        "PATH={}\"{PATH_SEP}${{PATH-}}\"; export PATH\n",
        sh_quote(&env.r_bin_dir().to_string_lossy())
    ));
    for (name, value) in env.vars() {
        out.push_str(&format!("{name}={}; export {name}\n", sh_quote(&value)));
    }
    out.push_str(&format!(
        "UVR_PROJECT={}; export UVR_PROJECT\n",
        sh_quote(project)
    ));

    // deactivate: restore every saved variable, drop our bookkeeping, and
    // remove itself so the shell is left exactly as it was found.
    out.push_str("deactivate() {\n");
    for name in managed_vars(env) {
        out.push_str(&format!(
            "    if [ -n \"${{UVR_OLD_{name}_SET-}}\" ]; then {name}=\"${{UVR_OLD_{name}-}}\"; \
             export {name}; else unset {name}; fi\n"
        ));
    }
    let bookkeeping: Vec<String> = managed_vars(env)
        .iter()
        .flat_map(|n| [format!("UVR_OLD_{n}"), format!("UVR_OLD_{n}_SET")])
        .collect();
    out.push_str(&format!("    unset {}\n", bookkeeping.join(" ")));
    out.push_str("    unset -f deactivate\n");
    out.push_str("}\n");
    out
}

fn emit_fish(project: &str, env: &REnv) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# uvr activation for project {project} — generated by `uvr activate --emit fish`.\n"
    ));
    out.push_str("if functions -q deactivate\n    deactivate\nend\n");

    // fish has no `${VAR+1}`; `set -q` is the set/unset test.
    for name in managed_vars(env) {
        out.push_str(&format!(
            "if set -q {name}\n    set -g UVR_OLD_{name} ${name}\n    set -g UVR_OLD_{name}_SET 1\n\
             else\n    set -g UVR_OLD_{name}_SET ''\nend\n"
        ));
    }

    // PATH is a genuine list in fish, so prepending is a list operation
    // rather than string concatenation with a separator.
    out.push_str(&format!(
        "set -gx PATH {} $PATH\n",
        fish_quote(&env.r_bin_dir().to_string_lossy())
    ));
    for (name, value) in env.vars() {
        out.push_str(&format!("set -gx {name} {}\n", fish_quote(&value)));
    }
    out.push_str(&format!(
        "set -gx UVR_PROJECT {}\n",
        fish_quote(project)
    ));

    out.push_str("function deactivate\n");
    for name in managed_vars(env) {
        out.push_str(&format!(
            "    if test -n \"$UVR_OLD_{name}_SET\"\n        set -gx {name} $UVR_OLD_{name}\n\
             \x20   else\n        set -e {name}\n    end\n"
        ));
    }
    let bookkeeping: Vec<String> = managed_vars(env)
        .iter()
        .flat_map(|n| [format!("UVR_OLD_{n}"), format!("UVR_OLD_{n}_SET")])
        .collect();
    out.push_str(&format!("    set -e {}\n", bookkeeping.join(" ")));
    out.push_str("    functions -e deactivate\nend\n");
    out
}

fn emit_powershell(project: &str, env: &REnv) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# uvr activation for project {project} — generated by `uvr activate --emit powershell`.\n"
    ));
    out.push_str("if (Test-Path Function:\\deactivate) { deactivate }\n");

    for name in managed_vars(env) {
        out.push_str(&format!(
            "if (Test-Path Env:\\{name}) {{ $env:UVR_OLD_{name} = $env:{name}; \
             $env:UVR_OLD_{name}_SET = '1' }} else {{ $env:UVR_OLD_{name}_SET = '' }}\n"
        ));
    }

    out.push_str(&format!(
        "$env:PATH = {} + '{PATH_SEP}' + $env:PATH\n",
        ps_quote(&env.r_bin_dir().to_string_lossy())
    ));
    for (name, value) in env.vars() {
        out.push_str(&format!("$env:{name} = {}\n", ps_quote(&value)));
    }
    out.push_str(&format!("$env:UVR_PROJECT = {}\n", ps_quote(project)));

    out.push_str("function global:deactivate {\n");
    for name in managed_vars(env) {
        out.push_str(&format!(
            "    if ($env:UVR_OLD_{name}_SET) {{ $env:{name} = $env:UVR_OLD_{name} }} \
             else {{ Remove-Item Env:\\{name} -ErrorAction SilentlyContinue }}\n"
        ));
    }
    for name in managed_vars(env) {
        out.push_str(&format!(
            "    Remove-Item Env:\\UVR_OLD_{name} -ErrorAction SilentlyContinue\n\
             \x20   Remove-Item Env:\\UVR_OLD_{name}_SET -ErrorAction SilentlyContinue\n"
        ));
    }
    out.push_str("    Remove-Item Function:\\deactivate -ErrorAction SilentlyContinue\n}\n");
    out
}

/// The sourceable shim. Contains no paths on purpose — see the module docs.
const SHIM_SH_BODY: &str = r#"# uvr project environment — source this file to activate:
#
#     source .uvr/activate
#
# Generated by uvr; do not edit. This file contains no paths: it asks the
# uvr binary to recompute the environment on every activation, so changing
# the project's R version never leaves it stale.
if ! command -v uvr >/dev/null 2>&1; then
    echo "uvr: not found on PATH — cannot activate this project." >&2
else
    # Only eval on success, so a failed resolve (no project, no R) leaves
    # the shell untouched instead of half-activated.
    __uvr_activate_out="$(uvr activate --emit sh)" && eval "$__uvr_activate_out"
    unset __uvr_activate_out
fi
"#;

const SHIM_FISH_BODY: &str = r#"# uvr project environment — source this file to activate:
#
#     source .uvr/activate.fish
#
# Generated by uvr; do not edit. Contains no paths: the environment is
# recomputed by the uvr binary on every activation.
if not command -q uvr
    echo "uvr: not found on PATH — cannot activate this project." >&2
else
    # Only eval on success, so a failed resolve leaves the shell untouched.
    set -l __uvr_activate_out (uvr activate --emit fish | string collect)
    if test $status -eq 0
        eval $__uvr_activate_out
    end
end
"#;

const SHIM_PS1_BODY: &str = r#"# uvr project environment — dot-source this file to activate:
#
#     . .uvr/activate.ps1
#
# Generated by uvr; do not edit. Contains no paths: the environment is
# recomputed by the uvr binary on every activation.
if (-not (Get-Command uvr -ErrorAction SilentlyContinue)) {
    Write-Error "uvr: not found on PATH — cannot activate this project."
} else {
    # Only invoke on success, so a failed resolve leaves the shell untouched.
    $uvrActivateOut = & uvr activate --emit powershell
    if ($LASTEXITCODE -eq 0) {
        Invoke-Expression ($uvrActivateOut -join "`n")
    }
}
"#;

/// Write the activation shims into `<root>/.uvr/`.
///
/// All shells get a shim regardless of the current platform — a project is
/// shared, and the person who cloned it may not use the shell that ran
/// `uvr init`.
pub fn write_shims(root: &Path) -> Result<()> {
    let dir = root.join(DOT_UVR_DIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    for (name, body) in [
        (SHIM_SH, SHIM_SH_BODY),
        (SHIM_FISH, SHIM_FISH_BODY),
        (SHIM_PS1, SHIM_PS1_BODY),
    ] {
        std::fs::write(dir.join(name), body)
            .with_context(|| format!("Failed to write {DOT_UVR_DIR}/{name}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env() -> REnv {
        REnv {
            r_binary: PathBuf::from("/opt/R/4.4.2/bin/R"),
            library: PathBuf::from("/proj/.uvr/library"),
            with_library: None,
            extra_libs: None,
        }
    }

    #[test]
    fn posix_prepends_the_project_r_to_path() {
        let out = emit_posix("demo", &env());
        assert!(
            out.contains(&format!("PATH='/opt/R/4.4.2/bin'\"{PATH_SEP}${{PATH-}}\"")),
            "PATH not prepended:\n{out}"
        );
    }

    #[test]
    fn posix_exports_every_variable_the_run_command_sets() {
        // Anti-drift: activation must export exactly what `uvr run` applies.
        // If someone adds a variable to REnv::vars, this fails until the
        // emitter carries it too.
        let env = env();
        let out = emit_posix("demo", &env);
        for (name, value) in env.vars() {
            assert!(
                out.contains(&format!("{name}={}; export {name}", sh_quote(&value))),
                "{name} missing from emitted script:\n{out}"
            );
        }
    }

    #[test]
    fn posix_defines_a_deactivate_that_removes_itself() {
        let out = emit_posix("demo", &env());
        assert!(out.contains("deactivate() {"));
        assert!(out.contains("unset -f deactivate"));
        // Restores rather than blindly unsetting.
        assert!(out.contains(r#"if [ -n "${UVR_OLD_PATH_SET-}" ]; then PATH="${UVR_OLD_PATH-}""#));
    }

    #[test]
    fn posix_unwinds_a_previous_activation_before_applying() {
        // Without this, re-activating stacks a second R bin dir onto PATH.
        let out = emit_posix("demo", &env());
        let deactivate_guard = out
            .find("if command -v deactivate")
            .expect("no unwind guard");
        let path_apply = out.find("PATH='/opt/R").expect("no PATH assignment");
        assert!(
            deactivate_guard < path_apply,
            "unwind must come before applying:\n{out}"
        );
    }

    #[test]
    fn posix_exports_the_project_name() {
        assert!(emit_posix("my-proj", &env()).contains("UVR_PROJECT='my-proj'; export UVR_PROJECT"));
    }

    #[test]
    fn posix_quotes_paths_containing_spaces_and_quotes() {
        let env = REnv {
            library: PathBuf::from("/pro j/it's/.uvr/library"),
            ..env()
        };
        let out = emit_posix("demo", &env);
        assert!(
            out.contains(r"'/pro j/it'\''s/.uvr/library'"),
            "path not safely quoted:\n{out}"
        );
    }

    #[test]
    fn shim_delegates_instead_of_baking_in_paths() {
        // The staleness guarantee: no absolute path may appear in the shim.
        assert!(SHIM_SH_BODY.contains("uvr activate --emit sh"));
        assert!(!SHIM_SH_BODY.contains("R_LIBS_USER"));
        assert!(!SHIM_SH_BODY.contains("/bin/R"));
    }

    #[test]
    fn shim_only_evals_when_resolution_succeeds() {
        // A failed resolve must leave the shell untouched, not half-applied.
        assert!(SHIM_SH_BODY.contains(r#"" && eval "#));
    }

    #[test]
    fn write_shims_creates_a_sourceable_file_for_every_shell() {
        let tmp = tempfile::tempdir().unwrap();
        write_shims(tmp.path()).unwrap();
        for (name, expect) in [
            (SHIM_SH, "uvr activate --emit sh"),
            (SHIM_FISH, "uvr activate --emit fish"),
            (SHIM_PS1, "uvr activate --emit powershell"),
        ] {
            let shim = tmp.path().join(DOT_UVR_DIR).join(name);
            assert!(shim.exists(), "{name} not written");
            assert!(
                std::fs::read_to_string(&shim).unwrap().contains(expect),
                "{name} does not delegate to the binary"
            );
        }
    }

    #[test]
    fn fish_prepends_to_the_path_list_and_exports_every_var() {
        let env = env();
        let out = emit_fish("demo", &env);
        // fish PATH is a real list — prepend as a list element, not by
        // splicing a separator into a string.
        assert!(out.contains("set -gx PATH '/opt/R/4.4.2/bin' $PATH"), "{out}");
        for (name, value) in env.vars() {
            assert!(
                out.contains(&format!("set -gx {name} {}", fish_quote(&value))),
                "{name} missing:\n{out}"
            );
        }
        assert!(out.contains("function deactivate"));
        assert!(out.contains("functions -e deactivate"));
        assert!(out.contains("if functions -q deactivate"));
    }

    #[test]
    fn powershell_prepends_to_path_and_exports_every_var() {
        let env = env();
        let out = emit_powershell("demo", &env);
        assert!(
            out.contains(&format!("$env:PATH = '/opt/R/4.4.2/bin' + '{PATH_SEP}' + $env:PATH")),
            "{out}"
        );
        for (name, value) in env.vars() {
            assert!(
                out.contains(&format!("$env:{name} = {}", ps_quote(&value))),
                "{name} missing:\n{out}"
            );
        }
        assert!(out.contains("function global:deactivate {"));
        assert!(out.contains("Remove-Item Function:\\deactivate"));
        assert!(out.contains("if (Test-Path Function:\\deactivate) { deactivate }"));
    }

    #[test]
    fn every_shell_manages_the_same_variables() {
        // The shells must not drift apart: whatever one takes over, the
        // others must take over (and therefore restore) too.
        let env = env();
        let scripts = [
            emit_posix("demo", &env),
            emit_fish("demo", &env),
            emit_powershell("demo", &env),
        ];
        for name in managed_vars(&env) {
            for script in &scripts {
                assert!(
                    script.contains(&format!("UVR_OLD_{name}")),
                    "{name} not saved in:\n{script}"
                );
            }
        }
    }

    #[test]
    fn shell_specific_quoting_escapes_embedded_quotes() {
        // fish escapes with a backslash; PowerShell doubles the quote.
        assert_eq!(fish_quote("it's"), r"'it\'s'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        // A Windows path's backslashes must survive fish quoting.
        assert_eq!(fish_quote(r"C:\R\bin"), r"'C:\\R\\bin'");
    }
}
