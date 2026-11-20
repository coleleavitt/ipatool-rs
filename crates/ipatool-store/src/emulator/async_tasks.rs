//! Go `async.go`: Swift concurrency runtime services on the serial host scheduler.

use std::rc::Rc;

use super::runtime::{
    Callback, REG_LR, REG_PC, REG_SP, REG_X0, REG_X1, REG_X20, REG_X22, REG_X29, RET, host_block,
    next_pending, pending_callback,
};
use super::{Guest, Payload, Result, Uc, fail};

/// Go's `asyncTask`, keyed by its `HostSwiftTask` handle.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AsyncTask {
    pub(crate) continuation: u64,
    pub(crate) handle: u64,
    pub(crate) context: u64,
    pub(crate) entry: u64,
    pub(crate) closure: u64,
    pub(crate) result: u64,
    pub(crate) executor: [u64; 2],
    pub(crate) done: bool,
}

fn task_local_owner(uc: &Uc) -> u64 {
    uc.sti().current_task.unwrap_or_default()
}

pub(crate) fn task_local_value(uc: &Uc, key: u64) -> u64 {
    let owner = task_local_owner(uc);
    uc.sti()
        .task_local_frames
        .get(&owner)
        .and_then(|frames| frames.iter().rev().find(|frame| frame.0 == key))
        .map_or(0, |frame| frame.1)
}

fn current_task<'a>(uc: &'a mut Uc<'_>) -> Option<&'a mut AsyncTask> {
    let handle = uc.sti().current_task?;
    uc.st().tasks.get_mut(&handle)
}

/// Value size from a type's value witness table.
pub(crate) fn value_size(uc: &Uc, metadata: u64) -> Result<u64> {
    let witnesses = uc.read64(metadata.wrapping_sub(8))?;
    uc.read64(witnesses + 64)
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
pub(crate) fn async_symbol(uc: &mut Uc, name: &str) -> Result<bool> {
    match name {
        "swift_task_localValueGet" => {
            let value = task_local_value(uc, uc.arg(0)?);
            uc.set(value)?;
        }
        "swift_task_localValuePush" => {
            let (key, source, metadata) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?);
            let size = value_size(uc, metadata)?;
            let value = uc.alloc(size)?;
            let data = uc.read_mem(source, size)?;
            uc.write_mem(value, &data)?;
            let owner = task_local_owner(uc);
            uc.st()
                .task_local_frames
                .entry(owner)
                .or_default()
                .push((key, value));
            uc.set(0)?;
        }
        "swift_task_localValuePop" => {
            let owner = task_local_owner(uc);
            let frames = uc.st().task_local_frames.entry(owner).or_default();
            if frames.pop().is_none() {
                fail!("empty TaskLocal binding stack");
            }
        }
        "swift_task_localsCopyTo" => {
            let target = uc.arg(0)?;
            if !uc.sti().tasks.contains_key(&target) {
                fail!("unknown task for local bindings");
            }
            let owner = task_local_owner(uc);
            let frames = uc
                .sti()
                .task_local_frames
                .get(&owner)
                .cloned()
                .unwrap_or_default();
            uc.st().task_local_frames.insert(target, frames);
        }
        "swift_continuation_init" => {
            let (context, flags) = (uc.arg(0)?, uc.arg(1)?);
            let Some(task) = current_task(uc) else {
                fail!("continuation initialized outside task");
            };
            if flags & 4 != 0 {
                fail!("preawaited continuation unsupported");
            }
            task.continuation = context;
            let handle = task.handle;
            uc.write64(context + 16, (flags & 1) | ((flags & 8) >> 2))?;
            uc.write64(context + 24, 0)?;
            uc.write64(context + 32, 0)?;
            if flags & 2 == 0 {
                uc.write64(context + 48, 0)?;
                uc.write64(context + 56, 0)?;
            }
            uc.set(handle)?;
        }
        "swift_continuation_resume"
        | "swift_continuation_throwingResume"
        | "swift_continuation_throwingResumeWithError" => {
            let context = match uc.sti().tasks.get(&uc.arg(0)?) {
                Some(task) if task.continuation != 0 => task.continuation,
                _ => fail!("unknown Swift continuation"),
            };
            if uc.read64(context + 24)? == 2 {
                fail!("Swift continuation resumed twice");
            }
            if name == "swift_continuation_throwingResumeWithError" {
                let error = uc.arg(1)?;
                uc.write64(context + 32, error)?;
            }
            uc.write64(context + 24, 2)?;
            uc.set(0)?;
        }
        "swift_continuation_await" => {
            let context = uc.arg(0)?;
            let task = match uc.sti().current_task {
                Some(handle)
                    if uc
                        .sti()
                        .tasks
                        .get(&handle)
                        .is_some_and(|task| task.continuation == context) =>
                {
                    handle
                }
                _ => fail!("awaiting unknown Swift continuation"),
            };
            await_continuation(uc, task, context)?;
        }
        "swift_task_create" => create_task(uc)?,
        "swift_task_alloc" => {
            let value = uc.alloc(uc.arg(0)?)?;
            uc.set(value)?;
        }
        "swift_task_dealloc" => {}
        "swift_task_getCurrent" => {
            let handle = uc.sti().current_task.unwrap_or_default();
            uc.set(handle)?;
        }
        // The CLI has no cancellation source inside a guest call.
        "swift_task_isCancelled" => uc.set(0)?,
        "swift_task_switch" => {
            let (function, first, second) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?);
            let Some(task) = current_task(uc) else {
                fail!("executor switch outside a Swift task");
            };
            task.executor = [first, second];
            uc.set_reg(REG_PC, function)?;
        }
        "swift_task_getCurrentExecutor" => {
            let executor = current_task(uc).map_or([0; 2], |task| task.executor);
            uc.set(executor[0])?;
            uc.set_reg(REG_X1, executor[1])?;
        }
        "swift_task_isCurrentExecutor" => {
            let executor = current_task(uc).map_or([0; 2], |task| task.executor);
            let wanted = [uc.arg(0)?, uc.arg(1)?];
            uc.set(u64::from(executor == wanted))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn await_continuation(uc: &mut Uc, task: u64, context: u64) -> Result<()> {
    if uc.read64(context + 24)? == 2 {
        if let Some(found) = uc.st().tasks.get_mut(&task) {
            found.continuation = 0;
        }
        uc.st().current_task = Some(task);
        uc.set_reg(REG_X22, context)?;
        let resume = uc.read64(context + 8)?;
        return uc.set_reg(REG_PC, resume);
    }
    let block = next_pending(uc);
    if block == 0 {
        fail!("Swift continuation has no runnable completion");
    }
    uc.st().current_task = None;
    pending_callback(
        uc,
        block,
        Box::new(move |uc| {
            uc.st().current_task = Some(task);
            await_continuation(uc, task, context)
        }),
    )
}

fn create_task(uc: &mut Uc) -> Result<()> {
    let (flags, options, result_type, descriptor, closure) =
        (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?, uc.arg(3)?, uc.arg(4)?);
    if options != 0 || flags & !0x1cff != 0 {
        fail!("unsupported Swift task options");
    }
    let bits = uc.read64(descriptor)?;
    let size = bits >> 32;
    if !(16..=1 << 20).contains(&size) {
        fail!("invalid Swift async context size");
    }
    let handle = uc.new_object("HostSwiftTask", Payload::Nil)?;
    let context = uc.alloc(size + 32)? + 32;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "Go's relative pointer arithmetic"
    )]
    let entry = (descriptor as i64).wrapping_add(i64::from(bits as u32 as i32)) as u64;
    let value_size = if result_type == 0 {
        0
    } else {
        value_size(uc, result_type)?
    };
    let result = uc.alloc(value_size)?;
    uc.st().tasks.insert(
        handle,
        super::async_tasks::AsyncTask {
            handle,
            context,
            entry,
            closure,
            result,
            ..AsyncTask::default()
        },
    );
    if flags & (1 << 10) != 0 {
        let owner = task_local_owner(uc);
        let frames = uc
            .sti()
            .task_local_frames
            .get(&owner)
            .cloned()
            .unwrap_or_default();
        uc.st().task_local_frames.insert(handle, frames);
    }
    let block = host_block(uc, Rc::new(move |uc: &mut Uc| start_async(uc, handle)))?;
    uc.st().pending.push(block);
    uc.set(handle)?;
    uc.set_reg(REG_X1, context)
}

const SAVED_REGISTERS: [i32; 13] = [
    REG_SP,
    REG_LR,
    REG_X29,
    REG_X0 + 19,
    REG_X20,
    REG_X0 + 21,
    REG_X22,
    REG_X0 + 23,
    REG_X0 + 24,
    REG_X0 + 25,
    REG_X0 + 26,
    REG_X0 + 27,
    REG_X0 + 28,
];

fn start_async(uc: &mut Uc, handle: u64) -> Result<()> {
    let Some(task) = uc.sti().tasks.get(&handle).copied() else {
        fail!("{}", super::runtime::NIL_DEREFERENCE);
    };
    if task.done {
        fail!("attempt to restart a completed Swift task");
    }
    let mut saved = [0u64; SAVED_REGISTERS.len()];
    for (slot, &register) in saved.iter_mut().zip(&SAVED_REGISTERS) {
        *slot = uc.reg(register)?;
    }
    let previous = uc.sti().current_task;
    uc.st().current_task = Some(handle);
    let completion = uc.alloc(16)?;
    uc.write_mem(completion, &RET)?;
    uc.st().callbacks.insert(
        completion,
        Callback::Shared(Rc::new(move |uc: &mut Uc| {
            let Some(task) = uc.st().tasks.get_mut(&handle) else {
                fail!("{}", super::runtime::NIL_DEREFERENCE);
            };
            if task.done {
                fail!("Swift task completed twice");
            }
            task.done = true;
            uc.st().current_task = previous;
            for (&register, &value) in SAVED_REGISTERS.iter().zip(&saved) {
                uc.set_reg(register, value)?;
            }
            uc.set(0)?;
            uc.set_reg(REG_PC, saved[1])
        })),
    );
    uc.write64(task.context, 0)?;
    uc.write64(task.context + 8, completion)?;
    uc.write64(task.context - 32, task.result)?;
    uc.write64(task.context - 24, task.entry)?;
    uc.write64(task.context - 16, task.closure)?;
    uc.set_reg(REG_X0, task.result)?;
    uc.set_reg(REG_X20, task.closure)?;
    uc.set_reg(REG_X0 + 21, 0)?;
    uc.set_reg(REG_X22, task.context)?;
    uc.set_reg(REG_PC, task.entry)
}
