//! Builds WebAssembly fixtures from Rust and WAT sources.
//!
//! Rust fixtures are compiled with the real `rustc` so tests exercise genuine
//! compiler-generated DWARF. Outputs are cached by content hash under the
//! workspace target directory.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Prefix that fixture source paths are remapped to inside DWARF.
pub const DWARF_PREFIX: &str = "/fixtures";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RustTarget {
    /// `wasm32-unknown-unknown` library exporting `extern "C"` functions.
    Library,
    /// `wasm32-wasip1` command with a `main` function.
    Wasi,
}

impl RustTarget {
    fn triple(self) -> &'static str {
        match self {
            RustTarget::Library => "wasm32-unknown-unknown",
            RustTarget::Wasi => "wasm32-wasip1",
        }
    }

    fn crate_type(self) -> &'static str {
        match self {
            RustTarget::Library => "cdylib",
            RustTarget::Wasi => "bin",
        }
    }
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Directory for generated fixture artifacts.
pub fn output_dir() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    let dir = base.join("wasm-dap-fixtures");
    std::fs::create_dir_all(&dir).expect("create fixture output directory");
    dir
}

/// Path of a fixture source file.
pub fn source_path(name: &str) -> PathBuf {
    fixtures_dir().join(name)
}

/// Source map entry that maps the DWARF prefix back to the fixtures directory.
pub fn source_map() -> (String, String) {
    (
        DWARF_PREFIX.to_string(),
        fixtures_dir().to_string_lossy().into_owned(),
    )
}

/// One-based line number of the first line in a fixture containing `marker`.
pub fn line_of(file: &str, marker: &str) -> u32 {
    let text = std::fs::read_to_string(source_path(file))
        .unwrap_or_else(|e| panic!("cannot read fixture {file}: {e}"));
    text.lines()
        .position(|l| l.contains(marker))
        .map(|i| i as u32 + 1)
        .unwrap_or_else(|| panic!("marker {marker:?} not found in {file}"))
}

/// Serializes fixture builds within a test process; parallel tests often need
/// the same fixture, and building it once lets the others reuse the result.
fn build_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn rustc_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let output = Command::new("rustc")
            .arg("-V")
            .output()
            .expect("rustc must be available to build test fixtures");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    })
}

/// Compiles `fixtures/<file>` at opt-level 0.
pub fn rust_fixture(file: &str, target: RustTarget) -> PathBuf {
    rust_fixture_opt(file, target, 0)
}

/// Compiles `fixtures/<file>` with the given optimization level.
pub fn rust_fixture_opt(file: &str, target: RustTarget, opt_level: u8) -> PathBuf {
    rust_fixture_with(file, target, opt_level, &[])
}

/// Compiles `fixtures/<file>` passing extra `rustc` arguments, such as
/// `["-C", "dwarf-version=5"]`.
pub fn rust_fixture_with(file: &str, target: RustTarget, opt_level: u8, extra: &[&str]) -> PathBuf {
    let dir = fixtures_dir();
    let source =
        std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("cannot read fixture {file}: {e}"));
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    target.hash(&mut hasher);
    opt_level.hash(&mut hasher);
    extra.hash(&mut hasher);
    rustc_version().hash(&mut hasher);
    let stem = Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("fixture");
    let out = output_dir().join(format!(
        "{stem}-{}-O{opt_level}-{:016x}.wasm",
        target.triple(),
        hasher.finish()
    ));
    if out.exists() {
        return out;
    }
    let _guard = build_lock();
    if out.exists() {
        return out;
    }

    // Build under the final file name (rustc derives the module name from it)
    // in a private directory, then move the result into place atomically.
    let build_dir = output_dir().join(format!(
        "build-{}-{:x}",
        std::process::id(),
        hasher.finish()
    ));
    std::fs::create_dir_all(&build_dir).expect("create fixture build directory");
    let tmp = build_dir.join(out.file_name().expect("fixture file name"));
    let mut command = Command::new("rustc");
    command
        .current_dir(&dir)
        .args(["--edition", "2021", "--target", target.triple()])
        .args(["--crate-type", target.crate_type()])
        .args(["-C", "debuginfo=2"])
        .arg("-C")
        .arg(format!("opt-level={opt_level}"))
        .arg(format!(
            "--remap-path-prefix={}={DWARF_PREFIX}",
            dir.display()
        ))
        .args(extra)
        .arg("-o")
        .arg(&tmp)
        .arg(file);
    if target == RustTarget::Library {
        command.args(["-C", "panic=abort"]);
    }
    let output = command.output().expect("failed to run rustc");
    if !output.status.success() {
        panic!(
            "failed to compile fixture {file} for {} (is the target installed? \
             run `rustup target add {}`):\n{}",
            target.triple(),
            target.triple(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if std::fs::rename(&tmp, &out).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::remove_dir_all(&build_dir);
    out
}

/// Assembles WAT text into a binary module (without DWARF).
pub fn wat_module(name: &str, wat: &str) -> PathBuf {
    let bytes = wat::parse_str(wat).unwrap_or_else(|e| panic!("invalid WAT fixture {name}: {e}"));
    write_unique(name, "wasm", &bytes)
}

/// Writes WAT text to a `.wat` file so it can be debugged at the source level.
pub fn wat_source(name: &str, wat: &str) -> PathBuf {
    write_unique(name, "wat", wat.as_bytes())
}

fn write_unique(name: &str, extension: &str, bytes: &[u8]) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let path = output_dir().join(format!("{name}-{:016x}.{extension}", hasher.finish()));
    let _guard = build_lock();
    if !path.exists() {
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes).expect("write fixture");
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    path
}

/// Zig executable from `WASM_DAP_ZIG`, used to build the C and Zig fixtures.
pub fn zig() -> Option<PathBuf> {
    std::env::var_os("WASM_DAP_ZIG")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

/// Compiles a C (`.c`, with Zig's bundled clang) or Zig (`.zig`) fixture for
/// `wasm32-wasi` with debug information.
pub fn zig_fixture(zig: &Path, file: &str) -> PathBuf {
    let dir = fixtures_dir();
    let source =
        std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("cannot read fixture {file}: {e}"));
    let version = Command::new(zig)
        .arg("version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    file.hash(&mut hasher);
    version.hash(&mut hasher);
    let stem = Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("fixture");
    let out = output_dir().join(format!("{stem}-zig-{:016x}.wasm", hasher.finish()));
    if out.exists() {
        return out;
    }
    let _guard = build_lock();
    if out.exists() {
        return out;
    }
    let build_dir = output_dir().join(format!(
        "build-{}-{:x}",
        std::process::id(),
        hasher.finish()
    ));
    std::fs::create_dir_all(&build_dir).expect("create fixture build directory");
    let tmp = build_dir.join(out.file_name().expect("fixture file name"));
    let mut command = Command::new(zig);
    command
        .current_dir(&dir)
        .env("ZIG_LOCAL_CACHE_DIR", output_dir().join("zig-cache"));
    if file.ends_with(".c") {
        command
            .args(["cc", "-target", "wasm32-wasi", "-g", "-O0", file, "-o"])
            .arg(&tmp);
    } else {
        command
            .args(["build-exe", file, "-target", "wasm32-wasi", "-O", "Debug"])
            .arg(format!("-femit-bin={}", tmp.display()));
    }
    let output = command.output().expect("failed to run zig");
    if !output.status.success() {
        panic!(
            "failed to compile fixture {file} with zig:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if std::fs::rename(&tmp, &out).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::remove_dir_all(&build_dir);
    out
}
