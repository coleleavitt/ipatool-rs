//! Go `callbacks.go`: guest calls made from inside a bridge service, resumed by a host stub.

use super::runtime::{Callback, REG_LR, REG_PC, REG_X0, RET};
use super::{After, Guest, Result, Uc, fail};

/// Calls the invoke function of `block` with `block` and `args`.
pub(crate) fn callback(uc: &mut Uc, block: u64, args: &[u64], after: After) -> Result<()> {
    let function = uc.read64(block + 16)?;
    let mut arguments = Vec::with_capacity(args.len() + 1);
    arguments.push(block);
    arguments.extend_from_slice(args);
    callback_function(uc, function, &arguments, after)
}

/// Calls `function` and runs `after` when it returns to a one-shot host stub, which then
/// returns to the service's caller.
pub(crate) fn callback_function(
    uc: &mut Uc,
    function: u64,
    args: &[u64],
    after: After,
) -> Result<()> {
    let link = uc.reg(REG_LR)?;
    let resume = uc.alloc(16)?;
    uc.write_mem(resume, &RET)?;
    uc.st().callbacks.insert(
        resume,
        Callback::Once(Box::new(move |uc| {
            uc.set_reg(REG_LR, link)?;
            uc.set_reg(REG_PC, link)?;
            after(uc)
        })),
    );
    for (index, &value) in args.iter().enumerate() {
        uc.set_reg(REG_X0 + i32::try_from(index).unwrap_or_default(), value)?;
    }
    uc.set_reg(REG_LR, resume)?;
    uc.set_reg(REG_PC, function)
}

pub(crate) type AfterValue = Box<dyn FnOnce(&mut Uc, u64) -> Result<()>>;

/// Go's `copyBlock`: `Block_copy` for stack blocks, global and heap blocks unchanged.
pub(crate) fn copy_block(uc: &mut Uc, block: u64, after: AfterValue) -> Result<()> {
    if block == 0 {
        return after(uc, 0);
    }
    #[allow(clippy::cast_possible_truncation, reason = "Go's uint32(flags)")]
    let flags = uc.read64(block + 8)? as u32;
    if flags & (1 << 28) != 0 {
        return after(uc, block);
    }
    let descriptor = uc.read64(block + 24)?;
    let size = uc.read64(descriptor + 8)?;
    if !(32..=1 << 20).contains(&size) {
        fail!("invalid guest block size");
    }
    let copied = uc.alloc(size)?;
    let data = uc.read_mem(block, size)?;
    uc.write_mem(copied, &data)?;
    if flags & (1 << 25) != 0 {
        let helper = uc.read64(descriptor + 16)?;
        return callback_function(
            uc,
            helper,
            &[copied, block],
            Box::new(move |uc| after(uc, copied)),
        );
    }
    after(uc, copied)
}

/// Go's `copyValue`: `copyWithZone:` for objects with a native implementation, else the
/// object itself; anything else is a block.
pub(crate) fn copy_value(uc: &mut Uc, value: u64, after: AfterValue) -> Result<()> {
    if value == 0 {
        return after(uc, 0);
    }
    if let Some(found) = uc.sti().objects.get(&value) {
        let class = found.class.clone();
        let entry = uc.lookup(&class, "copyWithZone:", false);
        if entry != 0 {
            let selector = uc.selector("copyWithZone:")?;
            return callback_function(
                uc,
                entry,
                &[value, selector, 0],
                Box::new(move |uc| {
                    let copied = uc.arg(0)?;
                    after(uc, copied)
                }),
            );
        }
        return after(uc, value);
    }
    copy_block(uc, value, after)
}

/// Go's `copyByref`: moves a `__block` variable to the heap once.
pub(crate) fn copy_byref(uc: &mut Uc, value: u64, after: AfterValue) -> Result<()> {
    let forwarded = uc.read64(value + 8)?;
    if forwarded != 0 && forwarded != value {
        return after(uc, forwarded);
    }
    let header = uc.read64(value + 16)?;
    let size = header >> 32;
    #[allow(clippy::cast_possible_truncation, reason = "Go's uint32(header)")]
    let flags = header as u32;
    if !(24..=1 << 20).contains(&size) {
        fail!("invalid byref capture size");
    }
    let copied = uc.alloc(size)?;
    let data = uc.read_mem(value, size)?;
    uc.write_mem(copied, &data)?;
    uc.write64(copied + 8, copied)?;
    uc.write64(value + 8, copied)?;
    if flags & (1 << 25) != 0 {
        let helper = uc.read64(value + 24)?;
        return callback_function(
            uc,
            helper,
            &[copied, value],
            Box::new(move |uc| after(uc, copied)),
        );
    }
    after(uc, copied)
}

/// Go's `delegateCall`: calls a delegate method, following `forwardingTargetForSelector:`.
pub(crate) fn delegate_call(
    uc: &mut Uc,
    receiver: u64,
    selector: &str,
    args: Vec<u64>,
    after: After,
) -> Result<()> {
    let Some(found) = uc.sti().objects.get(&receiver) else {
        fail!("missing transport delegate");
    };
    let class = found.class.clone();
    let entry = uc.lookup(&class, selector, false);
    if entry == 0 {
        let forward = uc.lookup(&class, "forwardingTargetForSelector:", false);
        if forward == 0 {
            fail!("missing transport delegate method {selector}");
        }
        let forwarding = uc.selector("forwardingTargetForSelector:")?;
        let wanted = uc.selector(selector)?;
        let selector = selector.to_owned();
        return callback_function(
            uc,
            forward,
            &[receiver, forwarding, wanted],
            Box::new(move |uc| {
                let target = uc.arg(0)?;
                if target == 0 || target == receiver {
                    fail!("transport delegate cannot forward {selector}");
                }
                delegate_call(uc, target, &selector, args, after)
            }),
        );
    }
    let mut arguments = vec![receiver, uc.selector(selector)?];
    arguments.extend(args);
    callback_function(uc, entry, &arguments, after)
}
