use crate::de::Linker;
use crate::object::DeserializeUnrealObject;
use crate::reader::UnrealReadExt;

#[derive(Copy, Clone, Debug, Default)]
pub struct FName(i32);

impl FName {
    pub fn from_raw(idx: i32) -> Self {
        FName(idx)
    }

    pub fn raw(&self) -> i32 {
        self.0
    }

    /// SC's `ULinkerLoad::operator<<(FName&)` is the inner FArchive's
    /// vtable[7] at xbe `0x3AA80` (not the ULinkerLoad primary-vtable
    /// stub at `0x467E0`, which is a no-op when the unused
    /// `data_2da840` hook global is null). It reads a packed_int into
    /// the linker's name_map, producing the global FName index, and
    /// the property tag loop terminates when that index is 0 (the
    /// global "None" FName). A raw zero check on the package-local
    /// index would only catch packages that placed "None" at local
    /// index 0; e.g. ETexRenderer.utx places it elsewhere.
    pub fn is_none(&self, linker: &Linker) -> bool {
        linker
            .package
            .names
            .get(self.0 as usize)
            .is_some_and(|n| n.name == "None")
    }
}

impl DeserializeUnrealObject for FName {
    fn deserialize<E, R>(
        &mut self,
        _runtime: &mut crate::runtime::UnrealRuntime,
        _linker: &std::rc::Rc<std::cell::RefCell<crate::de::Linker>>,
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
