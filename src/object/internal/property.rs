use std::cell::RefCell;
use std::rc::Rc;

use crate::de::Linker;
use crate::object::{DeserializeUnrealObject, NAME_NONE};
use crate::reader::{LinRead, UnrealReadExt};
use crate::runtime::UnrealRuntime;

#[derive(Default)]
pub struct PropertyTag {
    pub name: i32,
}

impl DeserializeUnrealObject for PropertyTag {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        self.name = reader.read_packed_int()?;
        if self.name as usize == NAME_NONE {
            return Ok(());
        }

        Ok(())
    }
}
