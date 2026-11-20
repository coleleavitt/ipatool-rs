//! Go `auth.go`: the credential-verification task, without host analytics or persistence.

use super::runtime::{invoke, set_ivar};
use super::{Error, Guest, Payload, Result, Uc};

pub(crate) fn new_credential_task(
    uc: &mut Uc,
    account: u64,
    bag: u64,
    password: &str,
    source: u64,
) -> Result<u64> {
    if uc.class_address("AMSVerifyCredentialsTask") == 0 {
        return Err(Error::msg(
            "authentication plugin is missing from the profile",
        ));
    }
    let options = uc.new_object("AMSAuthenticateOptions", Payload::Nil)?;
    let options = invoke(uc, options, "init", &[])?;
    let password = uc.new_object("NSString", Payload::String(password.to_owned()))?;
    for (selector, value) in [
        ("setEphemeral:", 1),
        ("setCanMakeAccountActive:", 0),
        ("setAllowServerDialogs:", 0),
        ("setPassword:", password),
        ("setCredentialSource:", source),
    ] {
        invoke(uc, options, selector, &[value])?;
    }
    // These entry points consume account, options and bag; the full task initializer also
    // starts analytics and account persistence services. Keep the request boundary
    // independent of both.
    let task = uc.new_object("AMSVerifyCredentialsTask", Payload::Nil)?;
    let task = invoke(uc, task, "init", &[])?;
    for (name, value) in [("_account", account), ("_options", options), ("_bag", bag)] {
        set_ivar(uc, task, name, value)?;
    }
    Ok(task)
}
