use std::io;

use crate::{
    de::RcLinker,
    object::{
        DeserializeUnrealObject, RcUnrealObject, UnrealObject,
        internal::{fdependency::FDependency, fname::FName, property::PropertyTag},
        ustate::State,
    },
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};
use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

#[derive(Default, Debug)]
pub struct Class {
    pub parent_object: State,

    pub old_class_record_size: Option<u32>,
    pub class_flags: u32,
    pub class_guid: (u32, u32, u32, u32),
    pub dependencies: Vec<FDependency>,
    pub package_imports: Vec<FName>,
    pub class_within: Option<RcUnrealObject>,
    pub class_config_name: FName,
    pub hide_categories: Vec<FName>,
    pub default_tags: Vec<PropertyTag>,
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

        debug!("package_imports");
        self.package_imports = reader.read_serializable_array::<E, FName>(runtime, linker)?;

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

        debug!("default_tags");
        loop {
            let mut tag = PropertyTag::default();
            tag.deserialize::<E, _>(runtime, linker, reader)?;
            if tag.name.is_none() {
                break;
            }
            trace!(
                "skipping tagged property value: type={} size={:#X}",
                tag.property_type, tag.size
            );
            if tag.size > 0 {
                let mut buf = vec![0u8; tag.size as usize];
                reader.cheat(&mut buf)?;
            }
            self.default_tags.push(tag);
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
