//! The `commerce` `StoreAgent` `FairPlay` runtime: account keybag context, kbsync and `.pkg`
//! payload decryption. Port of Go `storeagent.go`.

use std::io::{ErrorKind, Read, Write};

use super::shims::ShimOptions;
use super::{ImageSpec, Machine, RuntimeOptions, hardware_block, open_runtime, status_code};
use crate::assets::Bundle;
use crate::error::{Context, Error, Result};

pub(crate) const STORE_AGENT_BASE: u64 = 0x0000_1000_c000_0000;
pub(crate) const STORE_AGENT_GLOBAL_INIT: u64 = STORE_AGENT_BASE + 0x0013_12fc;
pub(crate) const STORE_AGENT_KB_SYNC_ENTRY: u64 = STORE_AGENT_BASE + 0x0019_ae34;
const STORE_AGENT_SESSION_INIT: u64 = STORE_AGENT_BASE + 0x0013_ac60;
const STORE_AGENT_DECRYPT_ENTRY: u64 = STORE_AGENT_BASE + 0x0017_5390;
pub(crate) const STORE_AGENT_GLOBAL_CLOSE: u64 = STORE_AGENT_BASE + 0x0013_0eb4;
const STORE_AGENT_CHUNK_SIZE: usize = 0x8000;
pub(crate) const STORE_AGENT_SC_INFO_PATH: &str = "/Users/Shared/SC Info";

pub(crate) const STORE_AGENT_ZERO_RETURN_ALIASES: &[&str] = &[
    "_pthread_rwlock_rdlock",
    "_pthread_rwlock_rdlock$UNIX2003",
    "_pthread_mutex_init",
    "_pthread_mutex_destroy",
    "_pthread_rwlock_destroy",
];

/// The SAP runtime plus the `commerce` image, as `StoreAgent` and the ADI client use it.
pub(crate) fn commerce_runtime(bundle: &Bundle) -> RuntimeOptions<'_> {
    RuntimeOptions {
        extra_images: vec![ImageSpec::new(
            "commerce",
            &bundle.commerce,
            STORE_AGENT_BASE,
        )],
        shims: ShimOptions {
            zero_return_aliases: STORE_AGENT_ZERO_RETURN_ALIASES,
            pause_run_loop: false,
        },
    }
}

pub struct StoreAgent {
    pub(crate) guest: Option<Machine>,
    global: u32,
    session: u64,
    decrypt_entry: u64,
    close_entry: u64,
    closed: bool,
}

impl StoreAgent {
    /// Go's `OpenStoreAgent`: a decryption session for one `.pkg` described by `dp_info`.
    pub fn open(bundle: &Bundle, hardware_id: &[u8], dp_info: &[u8]) -> Result<Self> {
        if dp_info.is_empty() {
            return Err(Error::msg("StoreAgent dpInfo is empty"));
        }
        let (mut agent, global) = Self::open_global(bundle, hardware_id)?;
        match agent.initialize_session(global, dp_info) {
            Ok(session) => {
                agent.session = session;
                Ok(agent)
            }
            Err(error) => {
                let _ = agent.close();
                Err(error)
            }
        }
    }

    pub(crate) fn open_global(bundle: &Bundle, hardware_id: &[u8]) -> Result<(Self, u32)> {
        let hardware = hardware_block(hardware_id)?;
        let (mut guest, exports) =
            open_runtime(bundle, commerce_runtime(bundle)).context("start StoreAgent runtime")?;
        guest.state_mut().entry.dispose = exports["_jEHf8Xzsv8K"];
        let mut agent = Self::with_guest(guest, STORE_AGENT_GLOBAL_CLOSE);
        match agent.initialize_global(&hardware) {
            Ok(global) => {
                agent.global = global;
                Ok((agent, global))
            }
            Err(error) => {
                let _ = agent.close();
                Err(error)
            }
        }
    }

    fn with_guest(guest: Machine, close_entry: u64) -> Self {
        Self {
            guest: Some(guest),
            global: 0,
            session: 0,
            decrypt_entry: STORE_AGENT_DECRYPT_ENTRY,
            close_entry,
            closed: false,
        }
    }

    fn guest(&mut self) -> Result<&mut Machine> {
        self.guest
            .as_mut()
            .ok_or_else(|| Error::msg("SAP guest machine is closed"))
    }

    fn initialize_global(&mut self, hardware: &[u8]) -> Result<u32> {
        let guest = self.guest()?;
        guest.begin_call();
        let result = (|| {
            let hardware_address = guest.scratch(hardware, hardware.len() as u64)?;
            let mut path = STORE_AGENT_SC_INFO_PATH.as_bytes().to_vec();
            path.push(0);
            let path_address = guest.scratch(&path, path.len() as u64)?;
            let context_field = guest.scratch(&[], 4)?;
            let status = guest
                .invoke(
                    STORE_AGENT_GLOBAL_INIT,
                    &[0, hardware_address, path_address, context_field],
                )
                .context("initialize StoreAgent global context")?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "StoreAgent global initialization returned {status}"
                )));
            }
            let value = guest.read_u32(context_field)?;
            if value == 0 {
                return Err(Error::msg(
                    "StoreAgent global initialization returned a null context",
                ));
            }
            Ok(value)
        })();
        guest.clear_scratch();
        result
    }

    fn initialize_session(&mut self, global: u32, dp_info: &[u8]) -> Result<u64> {
        let guest = self.guest()?;
        guest.begin_call();
        let result = (|| {
            let dp_info_address = guest.scratch(dp_info, dp_info.len() as u64)?;
            let session_field = guest.scratch(&[], 8)?;
            let status = guest
                .invoke(
                    STORE_AGENT_SESSION_INIT,
                    &[
                        u64::from(global),
                        dp_info_address,
                        dp_info.len() as u64,
                        session_field,
                    ],
                )
                .context("initialize StoreAgent session")?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "StoreAgent session initialization returned {status}"
                )));
            }
            let session = guest.read_u64(session_field)?;
            if session == 0 {
                return Err(Error::msg(
                    "StoreAgent session initialization returned a null session",
                ));
            }
            Ok(session)
        })();
        guest.clear_scratch();
        result
    }

    /// Decrypts `src` into `dst` in 0x8000-byte chunks, returning the bytes written.
    pub fn decrypt(&mut self, dst: &mut impl Write, src: &mut impl Read) -> Result<u64> {
        if self.closed {
            return Err(Error::msg("StoreAgent is closed"));
        }
        decrypt_stream(dst, src, |chunk| self.decrypt_chunk(chunk))
    }

    fn decrypt_chunk(&mut self, data: &mut [u8]) -> Result<()> {
        let (session, entry) = (self.session, self.decrypt_entry);
        let guest = self.guest()?;
        guest.begin_call();
        let result = (|| {
            let address = guest.scratch(data, data.len() as u64)?;
            let status = guest
                .invoke(entry, &[session, address, data.len() as u64, address, 0])
                .context("decrypt StoreAgent chunk")?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "StoreAgent decryption returned {status}"
                )));
            }
            guest
                .read_mem_into(data, address)
                .context("read decrypted StoreAgent chunk")
        })();
        guest.clear_scratch();
        result
    }

    /// Go's `StoreAgent.Close`: releases the global context, then the machine. Idempotent.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut errors = Vec::new();
        if self.global != 0
            && let Some(guest) = self.guest.as_mut()
        {
            let global = self.global;
            self.global = 0;
            self.session = 0;
            match guest.invoke(self.close_entry, &[u64::from(global)]) {
                Err(error) => {
                    errors.push(Some(error.wrap("close StoreAgent global context")));
                }
                Ok(status) if status_code(status) != 0 => errors.push(Some(Error::msg(format!(
                    "StoreAgent global context close returned {}",
                    status_code(status)
                )))),
                Ok(_) => {}
            }
        }
        if let Some(mut guest) = self.guest.take() {
            errors.push(guest.close().err());
        }
        Error::join(errors).map_or(Ok(()), Err)
    }
}

impl Drop for StoreAgent {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn decrypt_stream(
    dst: &mut impl Write,
    src: &mut impl Read,
    mut decrypt: impl FnMut(&mut [u8]) -> Result<()>,
) -> Result<u64> {
    let mut buffer = vec![0u8; STORE_AGENT_CHUNK_SIZE];
    let mut written = 0u64;
    loop {
        let (count, read_error) = read_full(src, &mut buffer);
        if count != 0 {
            let chunk = &mut buffer[..count];
            decrypt(chunk)?;
            let n = write_once(dst, chunk);
            written += n.as_ref().map_or(0, |&n| n as u64);
            match n {
                Err(error) => {
                    return Err(Error::from(error).wrap("write decrypted StoreAgent stream"));
                }
                Ok(n) if n != chunk.len() => {
                    return Err(Error::msg("short write").wrap("write decrypted StoreAgent stream"));
                }
                Ok(_) => {}
            }
        }
        match read_error {
            ReadOutcome::Full => {}
            ReadOutcome::End => return Ok(written),
            ReadOutcome::Failed(error) => {
                return Err(Error::from(error).wrap("read encrypted StoreAgent stream"));
            }
        }
    }
}

enum ReadOutcome {
    Full,
    End,
    Failed(std::io::Error),
}

/// Go's `io.ReadFull`: the bytes read before the buffer filled, the stream ended or failed.
fn read_full(src: &mut impl Read, buffer: &mut [u8]) -> (usize, ReadOutcome) {
    let mut count = 0;
    while count < buffer.len() {
        match src.read(&mut buffer[count..]) {
            Ok(0) => return (count, ReadOutcome::End),
            Ok(n) => count += n,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return (count, ReadOutcome::Failed(error)),
        }
    }
    (count, ReadOutcome::Full)
}

/// One `Write::write` call, as Go's `Writer.Write` reports a short count instead of retrying.
fn write_once(dst: &mut impl Write, chunk: &[u8]) -> std::io::Result<usize> {
    loop {
        match dst.write(chunk) {
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::machine::shims::Abi;
    use crate::machine::tests::service_machine;

    #[test]
    fn pins_verified_image_base_and_entry_offsets() {
        assert_eq!(STORE_AGENT_BASE, 0x0000_1000_c000_0000);
        assert_eq!(STORE_AGENT_GLOBAL_INIT, STORE_AGENT_BASE + 0x0013_12fc);
        assert_eq!(STORE_AGENT_SESSION_INIT, STORE_AGENT_BASE + 0x0013_ac60);
        assert_eq!(STORE_AGENT_DECRYPT_ENTRY, STORE_AGENT_BASE + 0x0017_5390);
        assert_eq!(STORE_AGENT_GLOBAL_CLOSE, STORE_AGENT_BASE + 0x0013_0eb4);
    }

    #[test]
    fn adds_only_store_agent_synchronization_aliases() {
        let mut machine = service_machine(ShimOptions {
            zero_return_aliases: STORE_AGENT_ZERO_RETURN_ALIASES,
            pause_run_loop: false,
        });
        for name in STORE_AGENT_ZERO_RETURN_ALIASES {
            let address = machine.state().symbols[*name];
            assert_eq!(machine.invoke(address, &[]).unwrap(), 0);
        }
    }

    #[test]
    fn default_profile_has_no_store_agent_aliases() {
        let mut machine = service_machine(ShimOptions::default());
        let address =
            crate::machine::shims::resolve(&mut machine.uc, "_pthread_mutex_init").unwrap();
        let error = machine.invoke(address, &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported import _pthread_mutex_init"),
            "{error}"
        );
    }

    #[test]
    fn uses_full_chunks_and_a_final_partial_chunk() {
        let input = vec![0x3c; 2 * STORE_AGENT_CHUNK_SIZE + 17];
        let mut output = Vec::new();
        let mut sizes = Vec::new();
        let written = decrypt_stream(&mut output, &mut Cursor::new(&input), |chunk| {
            sizes.push(chunk.len());
            for byte in chunk.iter_mut() {
                *byte ^= 0xff;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(written, input.len() as u64);
        assert_eq!(sizes, [STORE_AGENT_CHUNK_SIZE, STORE_AGENT_CHUNK_SIZE, 17]);
        assert_eq!(output, vec![0xc3; input.len()]);
    }

    #[test]
    fn does_not_invoke_decryptor_for_empty_stream() {
        let mut called = false;
        let written = decrypt_stream(&mut std::io::sink(), &mut Cursor::new([]), |_| {
            called = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(written, 0);
        assert!(!called);
    }

    struct FailingReader(bool);

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("source failed"));
            }
            self.0 = true;
            buffer[..7].copy_from_slice(b"partial");
            Ok(7)
        }
    }

    #[test]
    fn writes_partial_read_before_returning_source_error() {
        let mut output = Vec::new();
        let error = decrypt_stream(&mut output, &mut FailingReader(false), |_| Ok(())).unwrap_err();
        assert_eq!(output, b"partial");
        assert!(error.to_string().contains("source failed"), "{error}");
    }

    #[test]
    fn returns_decryptor_errors_without_writing_failed_chunk() {
        let mut output = Vec::new();
        let error = decrypt_stream(&mut output, &mut Cursor::new(b"encrypted"), |_| {
            Err(Error::msg("decrypt failed"))
        })
        .unwrap_err();
        assert_eq!(output, b"");
        assert_eq!(error.to_string(), "decrypt failed");
    }

    struct ShortWriter;

    impl Write for ShortWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Ok(1)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reports_short_writes() {
        let error = decrypt_stream(&mut ShortWriter, &mut Cursor::new(b"encrypted"), |_| Ok(()))
            .unwrap_err();
        assert!(error.to_string().contains("short write"), "{error}");
    }

    #[test]
    fn closes_global_context_and_machine_idempotently() {
        let mut machine = service_machine(ShimOptions::default());
        let closed = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = closed.clone();
        let close_entry = crate::machine::shims::add_function(
            &mut machine.uc,
            "test.storeagent.close",
            std::rc::Rc::new(move |uc| {
                seen.set(uc.argument(0)?);
                uc.set_result(0)
            }),
        )
        .unwrap();
        let mut agent = StoreAgent::with_guest(machine, close_entry);
        agent.global = 0x1234;
        agent.session = 0x5678;
        agent.close().unwrap();
        assert_eq!(closed.get(), 0x1234);
        assert_eq!(agent.global, 0);
        assert_eq!(agent.session, 0);
        assert!(agent.guest.is_none());
        agent.close().unwrap();
    }
}
