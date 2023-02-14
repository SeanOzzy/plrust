/*
Portions Copyright 2020-2021 ZomboDB, LLC.
Portions Copyright 2021-2023 Technology Concepts & Design, Inc. <support@tcdi.com>

All rights reserved.

Use of this source code is governed by the PostgreSQL license that can be found in the LICENSE.md file.
*/

use std::ffi::CStr;
use std::path::PathBuf;
use std::str::FromStr;

use once_cell::sync::Lazy;
use pgx::guc::{GucContext, GucRegistry, GucSetting};
use pgx::pg_sys;
use pgx::pg_sys::AsPgCStr;

use crate::target;
use crate::target::{CompilationTarget, CrossCompilationTarget, TargetErr};

/// This is the default set of lints we apply to PL/Rust user functions, and require of PL/Rust user
/// functions before we'll load and execute them.
///
/// The defaults **can** be changed with the `plrust.compile_lints` and `plrust.required_lints` GUCS
///
/// This is
// Hello from the futurepast!
// The only situation in which you should be removing this
// `#![forbid(unsafe_code)]` is if you are moving the forbid
// command somewhere else  or reconfiguring PL/Rust to also
// allow it to be run in a fully "Untrusted PL/Rust" mode.
// This enables the code checking not only for `unsafe {}`
// but also "unsafe attributes" which are considered unsafe
// but don't have the `unsafe` token.
const BUILTIN_LINTS: &'static str = "plrust_extern_blocks, plrust_lifetime_parameterized_traits, implied_bounds_entailment, unsafe_code, unknown_lints";

static PLRUST_WORK_DIR: GucSetting<Option<&'static str>> = GucSetting::new(None);
pub(crate) static PLRUST_PATH_OVERRIDE: GucSetting<Option<&'static str>> = GucSetting::new(None);
static PLRUST_TRACING_LEVEL: GucSetting<Option<&'static str>> = GucSetting::new(None);
pub(crate) static PLRUST_ALLOWED_DEPENDENCIES: GucSetting<Option<&'static str>> =
    GucSetting::new(None);
static PLRUST_COMPILATION_TARGETS: GucSetting<Option<&'static str>> = GucSetting::new(None);
pub(crate) static PLRUST_COMPILE_LINTS: GucSetting<Option<&'static str>> =
    GucSetting::new(Some(BUILTIN_LINTS));
pub(crate) static PLRUST_REQUIRED_LINTS: GucSetting<Option<&'static str>> =
    GucSetting::new(Some(BUILTIN_LINTS));

pub(crate) static PLRUST_ALLOWED_DEPENDENCIES_CONTENTS: Lazy<toml::value::Table> =
    Lazy::new(|| {
        let path = PathBuf::from_str(
            &PLRUST_ALLOWED_DEPENDENCIES
                .get()
                .expect("plrust.allowed_dependencies is not set in postgresql.conf"),
        )
        .expect("plrust.allowed_dependencies is not a valid path");

        let contents =
            std::fs::read_to_string(&path).expect("Unable to read allow listed dependencies");

        toml::from_str(&contents).expect("Unable to format allow listed dependencies")
    });

pub(crate) fn init() {
    GucRegistry::define_string_guc(
        "plrust.work_dir",
        "The directory where pl/rust will build functions with cargo",
        "The directory where pl/rust will build functions with cargo",
        &PLRUST_WORK_DIR,
        GucContext::Sighup,
    );

    GucRegistry::define_string_guc(
          "plrust.PATH_override", 
          "The $PATH setting to use for building plrust user functions",
          "It may be necessary to override $PATH in order to find compilation dependencies such as `cargo`, `cc`, etc",
          &PLRUST_PATH_OVERRIDE,
          GucContext::Sighup);

    GucRegistry::define_string_guc(
        "plrust.tracing_level",
        "The tracing level to use while running pl/rust",
        "The tracing level to use while running pl/rust. Should be `error`, `warn`, `info`, `debug`, or `trace`",
        &PLRUST_TRACING_LEVEL,
        GucContext::Sighup,
    );

    GucRegistry::define_string_guc(
        "plrust.allowed_dependencies",
        "The full path of a toml file containing crates and versions allowed when creating PL/Rust functions",
        "The full path of a toml file containing crates and versions allowed when creating PL/Rust functions",
        &PLRUST_ALLOWED_DEPENDENCIES,
        GucContext::Postmaster,
    );

    GucRegistry::define_string_guc(
        "plrust.compilation_targets",
        "A comma-separated list of architectures to target for cross compilation.  Supported values are: x86_64, aarch64",
        "Useful for when it's known a system will replicate to a Postgres server on a different CPU architecture",
        &PLRUST_COMPILATION_TARGETS,
        GucContext::Postmaster,
    );

    GucRegistry::define_string_guc(
        "plrust.compile_lints",
        "A comma-separated list of Rust code lints to apply to user functions during compilation",
        "If unspecified, PL/Rust will use a set of defaults",
        &PLRUST_COMPILE_LINTS,
        GucContext::Sighup,
    );

    GucRegistry::define_string_guc(
        "plrust.required_lints",
        "A comma-separated list of Rust code lints that are required to have been applied to a PL/Rust user function before PL/Rust will execute it",
        "If unspecified, PL/Rust will use a set of defaults",
        &PLRUST_REQUIRED_LINTS,
        GucContext::Sighup,
    );
}

pub(crate) fn work_dir() -> PathBuf {
    PathBuf::from_str(
        &PLRUST_WORK_DIR
            .get()
            .expect("plrust.work_dir is not set in postgresql.conf"),
    )
    .expect("plrust.work_dir is not a valid path")
}

pub(crate) fn tracing_level() -> tracing::Level {
    PLRUST_TRACING_LEVEL
        .get()
        .map(|v| v.parse().expect("plrust.tracing_level was invalid"))
        .unwrap_or(tracing::Level::INFO)
}

/// Returns the compilation targets a function should be compiled for.
///
/// The return format is `( <This Host's Target Triple>, <Other Configured Target Triples> )`
pub(crate) fn compilation_targets() -> eyre::Result<(
    &'static CompilationTarget,
    impl Iterator<Item = CrossCompilationTarget>,
)> {
    let this_target = target::tuple()?;
    let other_targets = match PLRUST_COMPILATION_TARGETS.get() {
        None => vec![],
        Some(targets) => targets
            .split(',')
            .map(str::trim)
            .filter(|s| s != &std::env::consts::ARCH) // make sure we don't include this architecture in the list of other targets
            .map(|s| s.try_into())
            .collect::<Result<Vec<CrossCompilationTarget>, TargetErr>>()?,
    };

    Ok((this_target, other_targets.into_iter()))
}

pub(crate) fn get_linker_for_target(target: &CrossCompilationTarget) -> Option<String> {
    unsafe {
        let guc_name = format!("plrust.{target}_linker");
        // SAFETY:  GetConfigOption returns a possibly NULL `char *` because `missing_ok` is true
        // but that's okay as we account for that possibility.  The named GUC not being in the
        // configuration is a perfectly fine thing.
        let value = pg_sys::GetConfigOption(guc_name.as_pg_cstr(), true, true);
        if value.is_null() {
            None
        } else {
            // SAFETY:  GetConfigOption gave us a valid `char *` that is usable as a CStr
            let value_cstr = CStr::from_ptr(value);
            Some(value_cstr.to_string_lossy().to_string())
        }
    }
}

pub(crate) fn get_pgx_bindings_for_target(target: &CrossCompilationTarget) -> Option<String> {
    unsafe {
        let guc_name = format!("plrust.{target}_pgx_bindings_path");
        // SAFETY:  GetConfigOption returns a possibly NULL `char *` because `missing_ok` is true
        // but that's okay as we account for that possibility.  The named GUC not being in the
        // configuration is a perfectly fine thing.
        let value = pg_sys::GetConfigOption(guc_name.as_pg_cstr(), true, true);
        if value.is_null() {
            None
        } else {
            // SAFETY:  GetConfigOption gave us a valid `char *` that is usable as a CStr
            let value_cstr = CStr::from_ptr(value);
            Some(value_cstr.to_string_lossy().to_string())
        }
    }
}
