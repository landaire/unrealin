use crate::{
    object::{DeserializeUnrealObject, NAME_NONE},
    reader::UnrealReadExt,
};

#[derive(Copy, Clone, Debug, Default)]
pub struct FName(i32);

impl FName {
    pub fn from_raw(idx: i32) -> Self {
        FName(idx)
    }

    pub fn is_none(&self) -> bool {
        self.0 as usize == NAME_NONE
    }
}

impl DeserializeUnrealObject for FName {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut crate::runtime::UnrealRuntime,
        linker: &std::rc::Rc<std::cell::RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: crate::reader::LinRead,
    {
        *self = FName::from_raw(reader.read_packed_int()?);

        Ok(())
    }
}
