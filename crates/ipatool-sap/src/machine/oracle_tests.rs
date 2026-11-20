//! Byte-for-byte comparison with Go `internal/sap/machine`. The fixture is recorded by
//! `scripts/machine-oracle.sh` (Go with patched randomness and time, plus Apple's live SAP
//! setup certificate and reply); this test replays the same calls with the same randomness,
//! time and Apple messages and compares every output.
//!
//! Opt-in: `IPATOOL_TEST_SAP_CACHE=1` (needs the SAP asset cache filled).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Deserialize;

use super::shim_platform::{Host, TEST_HOST};
use super::{Machine, ModernAdi, generate_kb_sync};
use crate::assets;

#[derive(Deserialize)]
struct Record {
    hardware_id: String,
    context: u64,
    certificate: String,
    request: String,
    request_state: i32,
    reply: String,
    #[serde(rename = "final")]
    final_output: String,
    final_state: i32,
    signs: Vec<Sign>,
    kbsync_dsid: u64,
    kbsync: String,
    host_uuid: String,
    adi_otp: String,
    sign_nanos: u64,
    flow_nanos: u64,
}

#[derive(Deserialize)]
struct Sign {
    input: String,
    output: String,
}

/// Go `oracleRandRead` and `oracleNow` from `tests/oracle/machine/seam.go`.
fn oracle_host() -> Host {
    let counter = Rc::new(Cell::new(0u64));
    Host {
        random: Rc::new(move |data| {
            for byte in data.iter_mut() {
                let value = counter.get().wrapping_mul(2_654_435_761) >> 13;
                *byte = value.to_le_bytes()[0];
                counter.set(counter.get() + 1);
            }
            Ok(())
        }),
        now: Rc::new(|| UNIX_EPOCH + Duration::new(1_800_000_000, 123_456_789)),
    }
}

fn hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    data.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn outcome(result: crate::Result<Vec<u8>>) -> String {
    result.map_or_else(|error| format!("error: {error}"), |data| hex(&data))
}

#[test]
fn matches_go_machine() {
    if std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_none() {
        return;
    }
    let record: Record =
        serde_json::from_str(include_str!("../../tests/fixtures/machine-oracle.json")).unwrap();
    let bundle = assets::load().unwrap();
    TEST_HOST.set(Some(oracle_host()));
    let hardware = unhex(&record.hardware_id);

    let start = Instant::now();
    let mut guest = Machine::open(&bundle).unwrap();
    let context = guest.initialize(&hardware).unwrap();
    assert_eq!(context, record.context);
    let (request, state) = guest
        .exchange(200, &hardware, context, &unhex(&record.certificate))
        .unwrap();
    assert_eq!(
        (hex(&request), state),
        (record.request, record.request_state)
    );
    let (output, state) = guest
        .exchange(200, &hardware, context, &unhex(&record.reply))
        .unwrap();
    assert_eq!(
        (hex(&output), state),
        (record.final_output, record.final_state)
    );
    let mut sign_time = Duration::ZERO;
    for (index, sign) in record.signs.iter().enumerate() {
        let sign_start = Instant::now();
        let signature = guest.sign(context, &unhex(&sign.input));
        if index == 0 {
            sign_time = sign_start.elapsed();
        }
        assert_eq!(outcome(signature), sign.output, "sign {}", sign.input);
    }
    guest.teardown(context).unwrap();
    let flow_time = start.elapsed();
    guest.close().unwrap();
    eprintln!(
        "one sign: Rust {sign_time:?}, Go {:?}; open + setup + 3 signs + teardown: Rust {flow_time:?}, Go {:?}",
        Duration::from_nanos(record.sign_nanos),
        Duration::from_nanos(record.flow_nanos)
    );

    assert_eq!(
        outcome(generate_kb_sync(&bundle, &hardware, record.kbsync_dsid)),
        record.kbsync
    );

    let otp = match ModernAdi::open(&bundle, &unhex(&record.host_uuid), &[]) {
        Err(error) => format!("error: {error}"),
        Ok(mut adi) => {
            let otp = adi.request_otp(record.kbsync_dsid).map_or_else(
                |error| format!("error: {error}"),
                |(mid, otp)| format!("{}:{}", hex(&mid), hex(&otp)),
            );
            adi.close().unwrap();
            otp
        }
    };
    assert_eq!(otp, record.adi_otp);
    TEST_HOST.set(None);
}
