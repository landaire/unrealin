use std::{
    cell::RefCell,
    io::{self, SeekFrom},
    rc::Rc,
};

use crate::{
    de::{Linker, ObjectExport, RcLinker},
    object::{
        DeserializeUnrealObject, RcUnrealObject, UnrealObject,
        builtins::Link,
        internal::{fdependency::FDependency, fname::FName},
        ustate::State,
        ustruct::Struct,
    },
    reader::{LinRead, UnrealReadExt},
    runtime::{RcUnrealObjPointer, UnrealRuntime},
};
use byteorder::ReadBytesExt;
use tracing::{Level, debug, span};

#[derive(Default, Debug)]
pub struct Class {
    pub parent_object: State,

    pub old_class_record_size: Option<u32>,
    pub class_flags: u32,
    pub class_guid: (u32, u32, u32, u32),
    pub dependencies: Vec<FDependency>,
    pub class_within: Option<RcUnrealObject>,
    pub class_config_name: FName,
    pub hide_categories: Vec<FName>,
}

impl DeserializeUnrealObject for Class {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_class");
        let _enter = span.enter();

        debug!("parent_object");
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let version = linker.borrow().version();
        if version <= 61 {
            self.old_class_record_size = Some(reader.read_u32::<E>()?);
        }

        self.class_flags = reader.read_u32::<E>()?;
        self.class_guid = (
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
        );

        debug!("dependencies");
        self.dependencies = reader.read_serializable_array::<E, FDependency>(runtime, linker)?;

        if version >= 62 {
            debug!("class_within");
            self.class_within = reader.read_object::<E>(runtime, linker)?;
            debug!("class_config_name");
            self.class_config_name
                .deserialize::<E, _>(runtime, linker, reader)?;
        }

        if version >= 99 {
            debug!("hide_categories");
            self.hide_categories = reader.read_serializable_array::<E, FName>(runtime, linker)?;
        }

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Class]
            .iter()
            .cloned()
            .chain(crate::object::ustate::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Class::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
