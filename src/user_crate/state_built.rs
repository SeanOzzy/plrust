use crate::user_crate::{CrateState, StateLoaded};
use pgx::pg_sys;
use std::path::{Path, PathBuf};

#[must_use]
pub(crate) struct StateBuilt {
    fn_oid: pg_sys::Oid,
    shared_object: PathBuf,
}

impl CrateState for StateBuilt {}

impl StateBuilt {
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(fn_oid: pg_sys::Oid, shared_object: PathBuf) -> Self {
        Self {
            fn_oid,
            shared_object,
        }
    }
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn shared_object(&self) -> &Path {
        self.shared_object.as_path()
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub unsafe fn load<'a>(self) -> eyre::Result<StateLoaded<'a>> {
        StateLoaded::load(self.fn_oid, &self.shared_object)
    }
}
