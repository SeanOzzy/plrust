use crate::{
    user_crate::{target, CrateState, StateBuilt},
    PlRustError,
};
use color_eyre::{Section, SectionExt};
use eyre::{eyre, WrapErr};
use pgx::pg_sys;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[must_use]
pub(crate) struct StateProvisioned {
    db_oid: pg_sys::Oid,
    fn_oid: pg_sys::Oid,
    crate_name: String,
    crate_dir: PathBuf,
}

impl CrateState for StateProvisioned {}

impl StateProvisioned {
    #[tracing::instrument(level = "debug", skip_all, fields(db_oid = %db_oid, fn_oid = %fn_oid, crate_name = %crate_name, crate_dir = %crate_dir.display()))]
    pub(crate) fn new(
        db_oid: pg_sys::Oid,
        fn_oid: pg_sys::Oid,
        crate_name: String,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            db_oid,
            fn_oid,
            crate_name,
            crate_dir,
        }
    }
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            db_oid = %self.db_oid,
            fn_oid = %self.fn_oid,
            crate_dir = %self.crate_dir.display(),
            target_dir = tracing::field::display(target_dir.display()),
        ))]
    pub(crate) fn build(
        self,
        pg_config: PathBuf,
        target_dir: &Path,
    ) -> eyre::Result<(StateBuilt, Output)> {
        let mut command = Command::new("cargo");
        let target = target::tuple()?;
        let target_str = &target;

        command.current_dir(&self.crate_dir);
        command.arg("rustc");
        command.arg("--release");
        command.arg("--target");
        command.arg(target_str);
        command.env("PGX_PG_CONFIG_PATH", pg_config);
        command.env("CARGO_TARGET_DIR", &target_dir);
        command.env(
            "RUSTFLAGS",
            "-Ctarget-cpu=native -Clink-args=-Wl,-undefined,dynamic_lookup",
        );

        let output = command.output().wrap_err("`cargo` execution failure")?;

        if output.status.success() {
            use std::env::consts::DLL_SUFFIX;

            let crate_name = self.crate_name;

            #[cfg(any(
                all(target_os = "macos", target_arch = "x86_64"),
                feature = "force_enable_x86_64_darwin_generations"
            ))]
            let crate_name = {
                let mut crate_name = crate_name;
                let next = crate::generation::next_generation(&crate_name, true)
                    .map(|gen_num| gen_num)
                    .unwrap_or_default();

                crate_name.push_str(&format!("_{}", next));
                crate_name
            };

            let built_shared_object_name = &format!("lib{crate_name}{DLL_SUFFIX}");
            let built_shared_object = target_dir
                .join(target_str)
                .join("release")
                .join(&built_shared_object_name);

            Ok((
                StateBuilt::new(self.db_oid, self.fn_oid, built_shared_object),
                output,
            ))
        } else {
            let stdout =
                String::from_utf8(output.stdout).wrap_err("`cargo`'s stdout was not  UTF-8")?;
            let stderr =
                String::from_utf8(output.stderr).wrap_err("`cargo`'s stderr was not  UTF-8")?;

            Err(eyre!(PlRustError::CargoBuildFail)
                .section(stdout.header("`cargo build` stdout:"))
                .section(stderr.header("`cargo build` stderr:"))
                .with_section(|| {
                    std::fs::read_to_string(&self.crate_dir.join("src").join("lib.rs"))
                        .wrap_err("Writing generated `lib.rs`")
                        .expect("Reading generated `lib.rs` to output during error")
                        .header("Source Code:")
                }))?
        }
    }

    pub(crate) fn fn_oid(&self) -> &u32 {
        &self.fn_oid
    }

    pub(crate) fn db_oid(&self) -> &u32 {
        &self.db_oid
    }
    pub(crate) fn crate_dir(&self) -> &Path {
        &self.crate_dir
    }
}
