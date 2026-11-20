//! Port of Go `internal/store/emulator/purchase_adapter.go`.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use super::callbacks::{callback_function, delegate_call};
use super::runtime::{host_block, invoke};
use super::{Guest, Payload, Result, Uc, method_key};
use crate::profile::Class;

const CLASS: &str = "IPAToolPurchaseRequestEncoder";
const SELECTOR: &str = "requestByEncodingRequestAndReturnSigningErrors:parameters:";

/// Go's `configuredPurchaseEncoder`. The purchase factory configures request properties
/// after the encoder options, so a caller subclass reapplies the legacy client's signing
/// policy before Apple's base encoder runs. No signature or authentication result is
/// fabricated.
pub(crate) fn configured_purchase_encoder(uc: &mut Uc, info: u64, bag: u64) -> Result<u64> {
    if uc.sti().class(CLASS).is_none() {
        let Some(base) = uc.sti().class("AMSPurchaseRequestEncoder").cloned() else {
            super::fail!("{}", super::runtime::NIL_DEREFERENCE);
        };
        let address = uc.alloc(16)?;
        let class = Class {
            name: CLASS.to_owned(),
            address,
            superclass: base.name.clone(),
            size: base.size,
            ivars: Some(BTreeMap::new()),
            ..Class::default()
        };
        uc.st().classes.insert(CLASS.to_owned(), Arc::new(class));
        uc.st().class_names.insert(address, CLASS.to_owned());
        let implementation = uc.lookup("AMSURLRequestEncoder", SELECTOR, false);
        let block = host_block(
            uc,
            Rc::new(move |uc: &mut Uc| {
                let (receiver, request, parameters) = (uc.arg(0)?, uc.arg(2)?, uc.arg(3)?);
                let encode = move |uc: &mut Uc| -> Result<()> {
                    let selector = uc.selector(SELECTOR)?;
                    callback_function(
                        uc,
                        implementation,
                        &[receiver, selector, request, parameters],
                        Box::new(|_| Ok(())),
                    )
                };
                let receiver_metrics = move |uc: &mut Uc| {
                    delegate_call(
                        uc,
                        receiver,
                        "setDisableLoadURLMetrics:",
                        vec![1],
                        Box::new(encode),
                    )
                };
                delegate_call(
                    uc,
                    receiver,
                    "setAnisetteType:",
                    vec![0],
                    Box::new(move |uc: &mut Uc| {
                        delegate_call(
                            uc,
                            receiver,
                            "setMescalType:",
                            vec![0],
                            Box::new(move |uc: &mut Uc| {
                                delegate_call(
                                    uc,
                                    request,
                                    "properties",
                                    Vec::new(),
                                    Box::new(move |uc: &mut Uc| {
                                        let properties = uc.arg(0)?;
                                        configure_properties(
                                            uc,
                                            properties,
                                            Box::new(receiver_metrics),
                                        )
                                    }),
                                )
                            }),
                        )
                    }),
                )
            }),
        )?;
        let entry = uc.read64(block + 16)?;
        uc.st()
            .methods
            .insert(method_key(CLASS, SELECTOR, false), entry);
    }
    let allocated = uc.new_object(CLASS, Payload::Nil)?;
    invoke(uc, allocated, "initWithPurchaseInfo:bag:", &[info, bag])
}

/// The request properties' share of the policy: no anisette, no Mescal, no load-URL
/// metrics.
fn configure_properties(
    uc: &mut Uc,
    properties: u64,
    then: Box<dyn FnOnce(&mut Uc) -> Result<()>>,
) -> Result<()> {
    delegate_call(
        uc,
        properties,
        "setAnisetteType:",
        vec![0],
        Box::new(move |uc: &mut Uc| {
            delegate_call(
                uc,
                properties,
                "setMescalType:",
                vec![0],
                Box::new(move |uc: &mut Uc| {
                    delegate_call(uc, properties, "setDisableLoadURLMetrics:", vec![1], then)
                }),
            )
        }),
    )
}
