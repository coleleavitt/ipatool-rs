//! Port of Go `profilegen/realize.go`: Swift classes with Store methods are realized by
//! their metadata accessors, which fixes their instance sizes and ivar offsets.

use ipatool_store::profile::Class;

use crate::error::{Error, Result, Wrap};
use crate::objc::ivars;
use crate::source::{le32, relative_to};
use crate::vm::{Guest, Vm};

pub(crate) fn realize_store_classes(vm: &mut Vm<'_, '_>, classes: &mut [Class]) -> Result<()> {
    for class in classes {
        if class.methods.as_ref().is_none_or(Vec::is_empty) || vm.u64(class.address + 32)? & 2 == 0
        {
            continue;
        }
        let descriptor = vm.u64(class.address + 64)?;
        if descriptor == 0 {
            continue;
        }
        let flags = le32(&vm.read(descriptor, 4)?);
        if flags & 0x80 != 0 {
            continue;
        }
        let displacement = le32(&vm.read(descriptor + 12, 4)?);
        if displacement == 0 {
            continue;
        }
        let accessor = relative_to(descriptor + 12, displacement);
        let (address, state) = vm
            .call_address(accessor, &[0])
            .wrap(format!("realize {}", class.name))?;
        if address != class.address || state != 0 {
            return Err(Error::failed(format!(
                "unexpected class metadata for {}: {address:#x} state {state}",
                class.name
            )));
        }
        let size = (u64::from(le32(&vm.read(class.address + 48, 4)?)) + 7) & !7;
        class.size = class.size.max(size);
        let ro = vm.u64(class.address + 32)? & !7;
        let list = vm.u64(ro + 48)?;
        if list != 0 {
            let count = le32(&vm.read(list + 4, 4)?);
            let stride = le32(&vm.read(list, 4)?);
            for index in 0..u64::from(count) {
                let entry = list + 8 + index * u64::from(stride);
                let name_address = vm.u64(entry + 8)?;
                let name = String::from_utf8_lossy(&vm.string(name_address)?).into_owned();
                let offset = vm.u64(entry)?;
                if offset != 0 {
                    let value = u64::from(le32(&vm.read(offset, 4)?));
                    ivars(class).insert(name, value);
                }
            }
        }
    }
    Ok(())
}
