use std::cell::RefCell;
use std::rc::Rc;

use tracing::{Level, debug, span, trace};

use crate::de::{Linker, RcLinker};
use crate::object::DeserializeUnrealObject;
use crate::object::internal::fname::FName;
use crate::reader::{LinRead, UnrealReadExt};
use crate::runtime::UnrealRuntime;

#[derive(Default)]
pub struct PropertyTag {
    pub name: FName,
}

impl DeserializeUnrealObject for PropertyTag {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_property_tag");
        let _enter = span.enter();

        debug!("Deserializing name");

        self.name.deserialize::<E, _>(runtime, linker, reader)?;
        if self.name.is_none() {
            trace!("Name is none");
            return Ok(());
        }

        Ok(())
    }
}
