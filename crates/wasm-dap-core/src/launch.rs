//! Launch configuration: the `launch` request arguments and their resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::debugger::{EntryPoint, LaunchOptions};
use crate::dwarf::SourceMap;
use crate::error::{Error, Result};
use crate::module::ExternKind;
use crate::runtime::{Program, StdinSource, WasiOptions};

/// Arguments accepted by the `launch` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LaunchArguments {
    /// Path to the `.wasm` binary or `.wat` text file.
    pub program: Option<String>,
    /// Command-line arguments passed to the program after `argv[0]`.
    pub args: Vec<String>,
    pub env: BTreeMap<String, Value>,
    pub inherit_env: bool,
    pub cwd: Option<String>,
    /// Preopened directories as `HOST[::GUEST]`.
    pub dirs: Vec<String>,
    /// Exported function to call instead of `_start`.
    pub invoke: Option<String>,
    pub invoke_args: Vec<Value>,
    /// Legacy form of `invoke`/`invokeArgs`.
    pub run: Option<LegacyRun>,
    pub stop_on_entry: bool,
    /// Maps path prefixes recorded in DWARF to local paths.
    pub source_map: BTreeMap<String, String>,
    pub stdin: Option<String>,
    pub no_debug: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LegacyRun {
    pub function: String,
    pub args: Vec<Value>,
}

/// A launch configuration resolved against the file system and the module.
pub struct PreparedLaunch {
    pub program: Program,
    pub options: LaunchOptions,
    pub source_map: SourceMap,
}

impl LaunchArguments {
    pub fn from_json(value: Option<&Value>) -> Result<Self> {
        match value {
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|e| Error::Config(format!("invalid launch configuration: {e}"))),
            None => Ok(Self::default()),
        }
    }

    fn working_directory(&self) -> Result<PathBuf> {
        match &self.cwd {
            Some(cwd) if !cwd.trim().is_empty() => {
                let path = PathBuf::from(cwd);
                if !path.is_dir() {
                    return Err(Error::Config(format!("cwd `{cwd}` is not a directory")));
                }
                Ok(path)
            }
            _ => std::env::current_dir().map_err(Error::Io),
        }
    }

    /// Loads the module and builds the debuggee configuration.
    pub fn prepare(&self) -> Result<PreparedLaunch> {
        let program = self
            .program
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| {
                Error::Config("launch requires a `program` (path to a .wasm or .wat file)".into())
            })?;
        let cwd = self.working_directory()?;
        let program_path = resolve(&cwd, program);
        if !program_path.is_file() {
            return Err(Error::Config(format!(
                "program `{}` does not exist",
                program_path.display()
            )));
        }
        let loaded = Program::load(&program_path)?;
        let loaded_paths: Vec<String> = loaded
            .debug
            .as_ref()
            .map(|d| d.lines().files().to_vec())
            .unwrap_or_default();
        let entry = self.entry_point(&loaded)?;

        let argv0 = program_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.to_string());
        let mut args = vec![argv0];
        args.extend(self.args.iter().cloned());
        let env = self
            .env
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                (k.clone(), value)
            })
            .collect();
        let dirs = self
            .dirs
            .iter()
            .map(|spec| parse_dir(&cwd, spec))
            .collect::<Result<Vec<_>>>()?;
        let stdin = match &self.stdin {
            Some(path) if !path.trim().is_empty() => StdinSource::File(resolve(&cwd, path)),
            _ => StdinSource::Empty,
        };

        Ok(PreparedLaunch {
            program: loaded,
            options: LaunchOptions {
                entry,
                wasi: WasiOptions {
                    args,
                    env,
                    inherit_env: self.inherit_env,
                    dirs,
                    stdin,
                    output: None,
                },
                stop_on_entry: self.stop_on_entry && !self.no_debug,
                no_debug: self.no_debug,
            },
            source_map: SourceMap::new(with_rust_std_sources(&loaded_paths, &self.source_map)),
        })
    }

    fn entry_point(&self, program: &Program) -> Result<EntryPoint> {
        if let Some(name) = self.invoke.as_deref().filter(|n| !n.trim().is_empty()) {
            return Ok(EntryPoint::Function {
                name: name.to_string(),
                args: self.invoke_args.clone(),
            });
        }
        if let Some(run) = self.run.as_ref().filter(|r| !r.function.is_empty()) {
            return Ok(EntryPoint::Function {
                name: run.function.clone(),
                args: run.args.clone(),
            });
        }
        default_entry(program)
    }
}

/// Adds mappings for Rust standard library sources.
///
/// Precompiled std records its sources under `/rustc/<commit>/`. When the
/// `rust-src` component is installed those files exist in the sysroot, so each
/// such prefix that the user has not mapped is pointed there.
fn with_rust_std_sources(
    dwarf_paths: &[String],
    user: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut map = user.clone();
    let prefixes: std::collections::BTreeSet<String> = dwarf_paths
        .iter()
        .filter_map(|p| rustc_source_prefix(p))
        .filter(|prefix| !user.keys().any(|k| prefix.starts_with(k.as_str())))
        .collect();
    if prefixes.is_empty() {
        return map;
    }
    let Some(sources) = rust_src_directory() else {
        return map;
    };
    for prefix in prefixes {
        map.insert(prefix, sources.clone());
    }
    map
}

/// `/rustc/<40 hex digits>` prefix of a path inside the Rust source tree.
fn rustc_source_prefix(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/rustc/")?;
    let hash = rest.split('/').next()?;
    (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("/rustc/{hash}"))
}

fn rust_src_directory() -> Option<String> {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let sources = sysroot.join("lib").join("rustlib").join("src").join("rust");
    sources
        .join("library")
        .is_dir()
        .then(|| sources.to_string_lossy().into_owned())
}

/// Entry point used when none is configured: `_start`, then the start function.
pub fn default_entry(program: &Program) -> Result<EntryPoint> {
    let info = &program.info;
    if info.exported_function("_start").is_some() {
        return Ok(EntryPoint::Command);
    }
    if info.start.is_some() {
        return Ok(EntryPoint::Instantiate);
    }
    let exported: Vec<&str> = info
        .exports
        .iter()
        .filter(|e| e.kind == ExternKind::Func)
        .map(|e| e.name.as_str())
        .collect();
    Err(Error::Config(format!(
        "the module has no entry point (no `_start` export and no start function); \
         set `invoke` to one of its exported functions: {}",
        if exported.is_empty() {
            "none".to_string()
        } else {
            exported.join(", ")
        }
    )))
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Parses `HOST[::GUEST]`; the guest path defaults to the host path as written.
pub fn parse_dir(cwd: &Path, spec: &str) -> Result<(PathBuf, String)> {
    let (host, guest) = match spec.split_once("::") {
        Some((host, guest)) => (host, guest.to_string()),
        None => (spec, spec.to_string()),
    };
    if host.is_empty() || guest.is_empty() {
        return Err(Error::Config(format!(
            "invalid directory mapping `{spec}`; use HOST[::GUEST]"
        )));
    }
    let host_path = resolve(cwd, host);
    if !host_path.is_dir() {
        return Err(Error::Config(format!(
            "directory `{}` does not exist",
            host_path.display()
        )));
    }
    Ok((host_path, guest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camel_case_arguments() {
        let args = LaunchArguments::from_json(Some(&serde_json::json!({
            "program": "app.wasm",
            "args": ["--flag"],
            "env": { "A": "1", "B": 2 },
            "invoke": "add",
            "invokeArgs": [1, "2"],
            "stopOnEntry": true,
            "sourceMap": { "/build": "C:/src" },
            "noDebug": false
        })))
        .unwrap();
        assert_eq!(args.program.as_deref(), Some("app.wasm"));
        assert_eq!(args.invoke.as_deref(), Some("add"));
        assert_eq!(args.invoke_args.len(), 2);
        assert!(args.stop_on_entry);
        assert_eq!(
            args.source_map.get("/build").map(String::as_str),
            Some("C:/src")
        );
    }

    #[test]
    fn directory_mappings_resolve_against_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let (host, guest) = parse_dir(&cwd, ".::/work").unwrap();
        assert_eq!(guest, "/work");
        assert!(host.is_dir());
        assert!(parse_dir(&cwd, "::/x").is_err());
        assert!(parse_dir(&cwd, "definitely-missing-dir-123").is_err());
    }

    #[test]
    fn rustc_prefixes_are_recognised() {
        let hash = "58cf8f9c35b355199727627d07ae34a4e2846d67";
        assert_eq!(
            rustc_source_prefix(&format!("/rustc/{hash}/library/core/src/lib.rs")),
            Some(format!("/rustc/{hash}"))
        );
        assert_eq!(rustc_source_prefix("/rustc/short/x.rs"), None);
        assert_eq!(rustc_source_prefix("/home/me/x.rs"), None);
        let user = BTreeMap::from([(format!("/rustc/{hash}"), "C:/custom".to_string())]);
        let mapped =
            with_rust_std_sources(&[format!("/rustc/{hash}/library/core/src/lib.rs")], &user);
        assert_eq!(
            mapped.get(&format!("/rustc/{hash}")).map(String::as_str),
            Some("C:/custom")
        );
    }

    #[test]
    fn missing_program_is_reported() {
        let error = LaunchArguments::default().prepare().err().unwrap();
        assert!(error.to_string().contains("program"));
    }
}
