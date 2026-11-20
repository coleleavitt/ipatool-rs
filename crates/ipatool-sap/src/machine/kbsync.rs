//! Account- and hardware-bound `FairPlay` `kbsync` data for the bag's `ent/download` endpoint.

use super::storeagent::{STORE_AGENT_KB_SYNC_ENTRY, StoreAgent};
use super::{Machine, status_code};
use crate::assets::Bundle;
use crate::error::{Context, Error, Result};

/// Go's `GenerateKBSync`, without opening a decryption session.
pub fn generate_kb_sync(bundle: &Bundle, hardware_id: &[u8], dsid: u64) -> Result<Vec<u8>> {
    if dsid == 0 {
        return Err(Error::msg("kbsync requires a nonzero account DSID"));
    }
    let (mut agent, global) = StoreAgent::open_global(bundle, hardware_id)?;
    let guest = agent
        .guest
        .as_mut()
        .ok_or_else(|| Error::msg("SAP guest machine is closed"))?;
    guest.generate_kb_sync(STORE_AGENT_KB_SYNC_ENTRY, global, dsid)
}

impl Machine {
    fn generate_kb_sync(&mut self, entry: u64, global: u32, dsid: u64) -> Result<Vec<u8>> {
        self.begin_call();
        let result = self.generate_kb_sync_call(entry, global, dsid);
        self.clear_scratch();
        result
    }

    fn generate_kb_sync_call(&mut self, entry: u64, global: u32, dsid: u64) -> Result<Vec<u8>> {
        let pointer_field = self.scratch(&[], 8)?;
        // StoreAgent writes a uint32 length. The zeroed upper half lets `consume_output`
        // share the SAP buffer bounds checking and storage disposal.
        let length_field = self.scratch(&[], 8)?;
        let status = self
            .invoke(
                entry,
                &[u64::from(global), dsid, 0, 1, pointer_field, length_field],
            )
            .context("generate StoreAgent kbsync")?;
        let output = self.consume_output(pointer_field, length_field);
        let status = status_code(status);
        if status != 0 {
            let error = Error::msg(format!("StoreAgent kbsync returned {status}"));
            return Err(match output {
                Ok(_) => error,
                Err(output) => Error::Joined(vec![error, output]),
            });
        }
        let output = output?;
        if output.is_empty() {
            return Err(Error::msg("StoreAgent kbsync returned an empty buffer"));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::machine::shims::{Abi, Guest, ShimOptions, add_function};
    use crate::machine::tests::service_machine;
    use crate::machine::{HEAP_BASE, MAX_OUTPUT_SIZE, SCRATCH_BASE};

    #[test]
    fn pins_kbsync_entry() {
        assert_eq!(
            STORE_AGENT_KB_SYNC_ENTRY,
            super::super::storeagent::STORE_AGENT_BASE + 0x0019_ae34
        );
    }

    #[test]
    fn rejects_missing_account_identity_before_loading_assets() {
        let error = generate_kb_sync(&Bundle::default(), &[], 0).unwrap_err();
        assert_eq!(error.to_string(), "kbsync requires a nonzero account DSID");
    }

    fn run(pointer: u64, length: u64, status: u32) -> (Result<Vec<u8>>, Vec<u64>, Machine) {
        let mut guest = service_machine(ShimOptions::default());
        guest.uc.write_mem(HEAP_BASE, &[0, 4, 0, 1, 42]).unwrap();
        let disposed = Rc::new(RefCell::new(Vec::new()));
        let record = disposed.clone();
        let dispose = add_function(
            &mut guest.uc,
            "test.kbsync.dispose",
            Rc::new(move |uc| {
                record.borrow_mut().push(uc.argument(0)?);
                uc.set_result(0)
            }),
        )
        .unwrap();
        guest.state_mut().entry.dispose = dispose;
        let entry = add_function(
            &mut guest.uc,
            "test.kbsync.generate",
            Rc::new(move |uc| {
                for (index, expected) in [0x1234, 0x1234_5678_9abc_def0, 0, 1]
                    .into_iter()
                    .enumerate()
                {
                    if uc.argument(index)? != expected {
                        return Err(Error::msg(format!("argument {index} is not {expected:#x}")));
                    }
                }
                let pointer_field = uc.argument(4)?;
                uc.write_u64(pointer_field, pointer)?;
                let length_field = uc.argument(5)?;
                // Only the low 32 bits are written by the real entry point.
                uc.write_u32(length_field, super::super::shims::low32(length))?;
                uc.set_result(u64::from(status))
            }),
        )
        .unwrap();
        let output = guest.generate_kb_sync(entry, 0x1234, 0x1234_5678_9abc_def0);
        let disposed = disposed.borrow().clone();
        (output, disposed, guest)
    }

    #[test]
    fn copies_and_disposes_guest_output_with_store_agent_abi() {
        for (pointer, length, status, want) in [
            (HEAP_BASE, 5, 0, ""),
            (HEAP_BASE, 5, !41u32, "kbsync returned -42"),
            (HEAP_BASE, MAX_OUTPUT_SIZE + 1, 0, "maximum is"),
            (0, 5, 0, "null output pointer"),
            (HEAP_BASE, 0, 0, "empty buffer"),
        ] {
            let (output, disposed, guest) = run(pointer, length, status);
            match output {
                Ok(output) => {
                    assert_eq!(want, "");
                    assert_eq!(output, [0, 4, 0, 1, 42]);
                }
                Err(error) => assert!(error.to_string().contains(want), "{error} vs {want}"),
            }
            if pointer == 0 {
                assert_eq!(disposed, Vec::<u64>::new());
            } else {
                assert_eq!(disposed, [pointer]);
            }
            assert_eq!(guest.state().scratch_cursor, 0);
            assert_eq!(guest.uc.read_mem(SCRATCH_BASE, 32).unwrap(), [0; 32]);
        }
    }
}
