#![recursion_limit = "128"]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::if_not_else)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::similar_names)]
/*!

## Making deb packages

If you only want to make some `*.deb` files, and you're not a developer of tools
for Debian packaging, **[see `cargo deb` command usage described in the
README instead](https://github.com/kornelski/cargo-deb#readme)**.

```sh
cargo install cargo-deb
cargo deb # run this in your Cargo project directory
```

## Making tools for making deb packages

The library interface is experimental. See `main.rs` for usage.
*/

pub mod compress;
pub mod deb {
    pub mod archive;
    pub mod control;
    pub mod data;
}
#[macro_use]
mod util;
mod dh {
    pub(crate) mod dh_installsystemd;
    pub(crate) mod dh_lib;
}
pub mod listener;
pub(crate) mod parse {
    pub(crate) mod config;
    pub(crate) mod manifest;
}
pub use crate::assets::{Config, Package};
pub use crate::deb::archive::Archive;
pub use crate::error::*;

pub mod assets;
mod dependencies;
mod error;
mod tararchive;

use crate::assets::{Asset, AssetSource, DebugSymbols, IsBuilt, ProcessedFrom};
use crate::listener::Listener;
use rayon::prelude::*;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const TAR_REJECTS_CUR_DIR: bool = true;

/// Set by `build.rs`
const DEFAULT_TARGET: &str = env!("CARGO_DEB_DEFAULT_TARGET");

/// Run `dpkg` to install `deb` archive at the given path
pub fn install_deb(path: &Path) -> CDResult<()> {
    let status = Command::new("sudo").arg("dpkg").arg("-i").arg(path)
        .status()?;
    if !status.success() {
        return Err(CargoDebError::InstallFailed);
    }
    Ok(())
}

/// Creates empty (removes files if needed) target/debian/foo directory so that we can start fresh.
pub fn reset_deb_temp_directory(options: &Config) -> io::Result<()> {
    let deb_dir = options.default_deb_output_dir();
    let deb_temp_dir = options.deb_temp_dir();
    remove_deb_temp_directory(options);
    // For backwards compatibility with previous cargo-deb behavior, also delete .deb from target/debian,
    // but this time only debs from other versions of the same package
    let g = deb_dir.join(Archive::filename_glob(&options.deb));
    if let Ok(old_files) = glob::glob(g.to_str().expect("utf8 path")) {
        for old_file in old_files.flatten() {
            let _ = fs::remove_file(old_file);
        }
    }
    fs::create_dir_all(deb_temp_dir)
}

/// Removes the target/debian/foo
pub fn remove_deb_temp_directory(options: &Config) {
    let deb_temp_dir = options.deb_temp_dir();
    let _ = fs::remove_dir(deb_temp_dir);
}

/// Builds a binary with `cargo build`
pub fn cargo_build(options: &Config, target: Option<&str>, build_command: &str, build_flags: &[String], verbose: bool) -> CDResult<()> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&options.package_manifest_dir);
    cmd.arg(build_command);

    cmd.args(build_flags);

    if verbose {
        cmd.arg("--verbose");
    }
    if let Some(target) = target {
        cmd.args(["--target", target]);
        // Set helpful defaults for cross-compiling
        if env::var_os("PKG_CONFIG_ALLOW_CROSS").is_none() && env::var_os("PKG_CONFIG_PATH").is_none() {
            let pkg_config_path = format!("/usr/lib/{}/pkgconfig", debian_triple_from_rust_triple(target));
            if Path::new(&pkg_config_path).exists() {
                cmd.env("PKG_CONFIG_ALLOW_CROSS", "1");
                cmd.env("PKG_CONFIG_PATH", pkg_config_path);
            }
        }
    }
    if !options.default_features {
        cmd.arg("--no-default-features");
    }
    let features = &options.features;
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }

    log::debug!("cargo build {:?}", cmd.get_args());

    let status = cmd.status()
        .map_err(|e| CargoDebError::CommandFailed(e, "cargo"))?;
    if !status.success() {
        return Err(CargoDebError::BuildFailed);
    }
    Ok(())
}

// Maps Rust's blah-unknown-linux-blah to Debian's blah-linux-blah. This is debian's multiarch.
fn debian_triple_from_rust_triple(rust_target_triple: &str) -> String {
    let mut p = rust_target_triple.split('-');
    let arch = p.next().unwrap();
    let abi = p.last().unwrap_or("gnu");

    let (darch, dabi) = match (arch, abi) {
        ("i586" | "i686", _) => ("i386", "gnu"),
        ("x86_64", _) => ("x86_64", "gnu"),
        ("aarch64", _) => ("aarch64", "gnu"),
        (arm, abi) if arm.starts_with("arm") || arm.starts_with("thumb") => {
            ("arm", if abi.ends_with("hf") {"gnueabihf"} else {"gnueabi"})
        },
        ("mipsel", _) => ("mipsel", "gnu"),
        ("loongarch64", _) => ("loong64", "gnu"),
        (risc, _) if risc.starts_with("riscv64") => ("riscv64", "gnu"),
        (arch, abi) => (arch, abi),
    };
    format!("{darch}-linux-{dabi}")
}

/// Debianizes the architecture name. Weirdly, architecture and multiarch use different naming conventions in Debian!
pub(crate) fn debian_architecture_from_rust_triple(target: &str) -> &str {
    let mut parts = target.split('-');
    let arch = parts.next().unwrap();
    let abi = parts.last().unwrap_or("");
    match (arch, abi) {
        // https://wiki.debian.org/Multiarch/Tuples
        // rustc --print target-list
        // https://doc.rust-lang.org/std/env/consts/constant.ARCH.html
        ("aarch64", _) => "arm64",
        ("mips64", "gnuabin32") => "mipsn32",
        ("mips64el", "gnuabin32") => "mipsn32el",
        ("mipsisa32r6", _) => "mipsr6",
        ("mipsisa32r6el", _) => "mipsr6el",
        ("mipsisa64r6", "gnuabi64") => "mips64r6",
        ("mipsisa64r6", "gnuabin32") => "mipsn32r6",
        ("mipsisa64r6el", "gnuabi64") => "mips64r6el",
        ("mipsisa64r6el", "gnuabin32") => "mipsn32r6el",
        ("powerpc", "gnuspe") => "powerpcspe",
        ("powerpc64", _) => "ppc64",
        ("powerpc64le", _) => "ppc64el",
        ("riscv64gc", _) => "riscv64",
        ("i586" | "i686" | "x86", _) => "i386",
        ("x86_64", "gnux32") => "x32",
        ("x86_64", _) => "amd64",
        ("loongarch64", _) => "loong64",
        (arm, gnueabi) if arm.starts_with("arm") && gnueabi.ends_with("hf") => "armhf",
        (arm, _) if arm.starts_with("arm") => "armel",
        (other_arch, _) => other_arch,
    }
}
fn ensure_success(status: ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, status.to_string()))
    }
}

/// Strips the binary that was created with cargo
pub fn strip_binaries(options: &mut Config, target: Option<&str>, listener: &dyn Listener) -> CDResult<()> {
    let mut cargo_config = None;
    let objcopy_tmp;
    let strip_tmp;
    let mut objcopy_cmd = Path::new("objcopy");
    let mut strip_cmd = Path::new("strip");

    if let Some(target) = target {
        cargo_config = options.cargo_config()?;
        if let Some(ref conf) = cargo_config {
            if let Some(cmd) = conf.objcopy_command(target) {
                listener.info(format!("Using '{}' for '{target}'", cmd.display()));
                objcopy_tmp = cmd;
                objcopy_cmd = &objcopy_tmp;
            }

            if let Some(cmd) = conf.strip_command(target) {
                listener.info(format!("Using '{}' for '{target}'", cmd.display()));
                strip_tmp = cmd;
                strip_cmd = &strip_tmp;
            }
        }
    }

    let stripped_binaries_output_dir = options.default_deb_output_dir();
    let (separate_debug_symbols, compress_debug_symbols) = match options.debug_symbols {
        DebugSymbols::Keep | DebugSymbols::Strip => (false, false),
        DebugSymbols::Separate { compress } => (true, compress),
    };

    let added_debug_assets = options.deb.built_binaries_mut().into_par_iter().enumerate()
        .filter(|(_, asset)| !asset.source.archive_as_symlink_only()) // data won't be included, so nothing to strip
        .map(|(i, asset)| {
        let (new_source, new_debug_asset) = match asset.source.path() {
            Some(path) => {
                if !path.exists() {
                    return Err(CargoDebError::StripFailed(path.to_owned(), "The file doesn't exist".into()));
                }

                let conf_path = cargo_config.as_ref().map(|c| c.path())
                    .unwrap_or_else(|| Path::new(".cargo/config"));
                let file_name = path.file_stem().ok_or(CargoDebError::Str("bad path"))?.to_string_lossy();
                let stripped_temp_path = stripped_binaries_output_dir.join(format!("{file_name}.tmp{i}-stripped"));
                let _ = std::fs::remove_file(&stripped_temp_path);

                log::debug!("stripping with {} from {} into {}", strip_cmd.display(), path.display(), stripped_temp_path.display());
                Command::new(strip_cmd)
                   // same as dh_strip
                   .args(["--strip-unneeded", "--remove-section=.comment", "--remove-section=.note"])
                   .arg("-o").arg(&stripped_temp_path)
                   .arg(path)
                   .status()
                   .and_then(ensure_success)
                   .map_err(|err| {
                        if let Some(target) = target {
                            CargoDebError::StripFailed(path.to_owned(), format!("{}: {}.\nhint: Target-specific strip commands are configured in [target.{}] strip = {{ path = \"{}\" }} in {}", strip_cmd.display(), err, target, strip_cmd.display(), conf_path.display()))
                        } else {
                            CargoDebError::CommandFailed(err, "strip")
                        }
                    })?;

                if !stripped_temp_path.exists() {
                    return Err(CargoDebError::StripFailed(path.to_owned(), format!("{} command failed to create output '{}'", strip_cmd.display(), stripped_temp_path.display())));
                }

                let new_debug_asset = if separate_debug_symbols && asset.c.is_built() {
                    log::debug!("extracting debug info with {} from {}", objcopy_cmd.display(), path.display());

                    // parse the ELF and use debug-id-based path if available
                    let debug_target_path = get_target_debug_path(asset, path)?;

                    // --add-gnu-debuglink reads the file path given, so it can't get to-be-installed target path
                    // and the recommended fallback solution is to give it relative path in the same dir
                    let debug_temp_path = stripped_temp_path.with_file_name(debug_target_path.file_name().ok_or(CargoDebError::Str("bad path"))?);

                    let _ = std::fs::remove_file(&debug_temp_path);
                    let mut args: &[_] = &["--only-keep-debug", "--compress-debug-sections=zstd"];
                    if !compress_debug_symbols {
                        args = &args[..1];
                    }
                    Command::new(objcopy_cmd)
                        .args(args)
                        .arg(path)
                        .arg(&debug_temp_path)
                        .status()
                        .and_then(ensure_success)
                        .map_err(|err| {
                            if let Some(target) = target {
                                CargoDebError::StripFailed(path.to_owned(), format!("{}: {}.\nhint: Target-specific strip commands are configured in [target.{}] objcopy = {{ path =\"{}\" }} in {}", objcopy_cmd.display(), err, target, objcopy_cmd.display(), conf_path.display()))
                            } else {
                                CargoDebError::CommandFailed(err, "objcopy")
                            }
                        })?;

                    let relative_debug_temp_path = debug_temp_path.file_name().ok_or(CargoDebError::Str("bad path"))?;
                    log::debug!("linking debug info with {} from {} into {:?}", objcopy_cmd.display(), stripped_temp_path.display(), relative_debug_temp_path);
                    Command::new(objcopy_cmd)
                        .current_dir(debug_temp_path.parent().ok_or(CargoDebError::Str("bad path"))?)
                        .arg("--add-gnu-debuglink")
                        // intentionally relative - the file name must match debug_target_path
                        .arg(relative_debug_temp_path)
                        .arg(&stripped_temp_path)
                        .status()
                        .and_then(ensure_success)
                        .map_err(|err| CargoDebError::CommandFailed(err, "objcopy"))?;

                    Some(Asset::new(
                        AssetSource::Path(debug_temp_path),
                        debug_target_path,
                        0o644,
                        IsBuilt::No,
                        false,
                    ).processed(if compress_debug_symbols { "compress"} else {"separate"}, path.to_path_buf()))
                } else {
                    None // no new asset
                };
                listener.info(format!("Stripped '{}'", path.display()));

                (AssetSource::Path(stripped_temp_path), new_debug_asset)
            },
            None => {
                // This is unexpected - emit a warning if we come across it
                listener.warning(format!("Found built asset with non-path source '{asset:?}'"));
                return Ok(None);
            },
        };
        log::debug!("Replacing asset {} with stripped asset {}", asset.source.path().unwrap().display(), new_source.path().unwrap().display());
        let old_source = std::mem::replace(&mut asset.source, new_source);
        asset.processed_from = Some(ProcessedFrom {
            original_path: old_source.into_path(),
            action: "strip",
        });
        Ok::<_, CargoDebError>(new_debug_asset)
    }).collect::<Result<Vec<_>, _>>()?;

    options.deb.assets.resolved
        .extend(added_debug_assets.into_iter().filter_map(|debug_file| debug_file));

    Ok(())
}

fn get_target_debug_path(asset: &Asset, asset_path: &Path) -> Result<PathBuf, CargoDebError> {
    let target_debug_path = match elf_gnu_debug_id(asset_path) {
        Ok(Some(path)) => {
            log::debug!("got gnu debug-id: {} for {}", path.display(), asset_path.display());
            path
        },
        Ok(None) => {
            log::debug!("debug-id not found in {}", asset_path.display());
            asset.c.default_debug_target_path()
        },
        Err(e) => {
            log::debug!("elf: {e} in {}", asset_path.display());
            asset.c.default_debug_target_path()
        },
    };
    Ok(target_debug_path)
}

#[cfg(not(feature = "debug-id"))]
fn elf_gnu_debug_id(_: &Path) -> io::Result<Option<PathBuf>> {
    Ok(None)
}

#[cfg(feature = "debug-id")]
fn elf_gnu_debug_id(elf_file_path: &Path) -> Result<Option<PathBuf>, elf::ParseError> {
    use elf::endian::AnyEndian;
    use elf::note::Note;
    use elf::ElfStream;

    let mut stream = ElfStream::<AnyEndian, _>::open_stream(fs::File::open(elf_file_path)?)?;
    let Some(abi_shdr) = stream.section_header_by_name(".note.gnu.build-id")?
        else { return Ok(None) };

    let abi_shdr = abi_shdr.clone();
    for note in stream.section_data_as_notes(&abi_shdr)? {
        if let Note::GnuBuildId(note) = note {
            if let Some((byte, rest)) = note.0.split_first() {
                let mut s = format!("usr/lib/debug/.build-id/{byte:02x}/");
                for b in rest {
                    use std::fmt::Write;
                    write!(&mut s, "{b:02x}").unwrap();
                }
                s.push_str(".debug");
                return Ok(Some(s.into()));
            }
        }
    }
    Ok(None)
}
