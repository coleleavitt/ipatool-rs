//! `auth login` (Go `cmd/auth.go` `loginCmd`), with the fork's 2FA re-prompt.

use ipatool_appstore::{LoginInput, LoginOutput};
use serde_json::json;

use crate::BoxError;
use crate::output::Output;

/// Go's `retry.Attempts(3)`: the first try and two 2FA prompts.
const LOGIN_ATTEMPTS: usize = 3;

#[derive(clap::Args, Clone, Debug, Default)]
pub struct LoginArgs {
    /// Apple ID email address or phone number (required)
    #[arg(short, long, default_value = "", hide_default_value = true)]
    pub email: String,
    /// password for the Apple ID (required)
    #[arg(short, long, default_value = "", hide_default_value = true)]
    pub password: String,
    /// 2FA code for the Apple ID
    #[arg(long, default_value = "", hide_default_value = true)]
    pub auth_code: String,
    #[command(flatten)]
    pub _ignored: crate::cobra::IgnoredArgs,
}

/// Runs the login command. `prompt` is Go's `readPrompt` (text, masked); `login` is
/// `AppStore.Login`.
pub async fn run(
    args: LoginArgs,
    interactive: bool,
    out: &Output,
    mut prompt: impl FnMut(&str, bool) -> Result<String, String>,
    mut login: impl AsyncFnMut(LoginInput) -> ipatool_appstore::Result<LoginOutput>,
) -> Result<(), BoxError> {
    let LoginArgs {
        mut email,
        mut password,
        mut auth_code,
        ..
    } = args;
    if email.is_empty() {
        if !interactive {
            return Err("an Apple ID is required when not running in interactive mode; use the \"--email\" flag".into());
        }
        email = prompt("enter Apple ID (email address or phone number): ", false)
            .map_err(|err| format!("failed to read Apple ID: {err}"))?;
    }
    if password.is_empty() {
        if !interactive {
            return Err("password is required when not running in interactive mode; use the \"--password\" flag".into());
        }
        password = prompt("enter password: ", true)
            .map_err(|err| format!("failed to read password: {err}"))?;
        if password.trim().is_empty() {
            return Err("password cannot be empty".into());
        }
    }

    out.message("preparing authentication; the first login may take a few minutes");
    let mut invalid_code = None;
    for attempt in 1..=LOGIN_ATTEMPTS {
        if let Some(invalid) = invalid_code
            && interactive
        {
            let text = if invalid {
                "invalid 2FA code, please try again: "
            } else {
                "enter 2FA code: "
            };
            auth_code =
                prompt(text, false).map_err(|err| format!("failed to read auth code: {err}"))?;
        }
        out.verbose(
            json!({ "email": email, "authCodeProvided": !auth_code.is_empty() }),
            "logging in",
        );
        let error = match login(LoginInput {
            email: email.clone(),
            password: password.clone(),
            auth_code: auth_code.clone(),
        })
        .await
        {
            Ok(output) => {
                out.info(json!({
                    "name": output.account.name,
                    "email": output.account.email,
                    "success": true,
                }));
                return Ok(());
            }
            Err(error) => error,
        };
        let required = error.is_auth_code_required();
        let invalid = error.is_invalid_auth_code();
        if required && !interactive {
            out.message("2FA code is required; run the command again and supply a code using the `--auth-code` flag");
            return Ok(());
        }
        if invalid && !interactive {
            out.message("invalid 2FA code provided; run the command again with a valid code using the `--auth-code` flag");
            return Ok(());
        }
        if !(required || invalid) || attempt == LOGIN_ATTEMPTS {
            return Err(error.into());
        }
        invalid_code = Some(invalid);
    }
    unreachable!("the last attempt returns")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use ipatool_appstore::{Account, Error};

    use super::*;
    use crate::output::Format;

    fn args(email: &str, password: &str) -> LoginArgs {
        LoginArgs {
            email: email.to_owned(),
            password: password.to_owned(),
            auth_code: String::new(),
            ..LoginArgs::default()
        }
    }

    fn quiet() -> Output {
        Output::new(Format::Json, false)
    }

    #[tokio::test]
    async fn requires_credentials_in_non_interactive_mode() {
        for (args, message) in [
            (
                args("", "secret"),
                "an Apple ID is required when not running in interactive mode; use the \"--email\" flag",
            ),
            (
                args("user@example.com", ""),
                "password is required when not running in interactive mode; use the \"--password\" flag",
            ),
        ] {
            let error = run(
                args,
                false,
                &quiet(),
                |_, _| unreachable!(),
                async |_| unreachable!(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), message);
        }
    }

    #[tokio::test]
    async fn uses_supplied_credentials() {
        for interactive in [true, false] {
            for apple_id in ["user@example.com", "+8613912345678", "9123456789"] {
                let inputs = RefCell::new(Vec::new());
                run(
                    args(apple_id, "secret"),
                    interactive,
                    &quiet(),
                    |_, _| unreachable!(),
                    async |input| {
                        inputs.borrow_mut().push(input);
                        Ok(LoginOutput::default())
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    inputs.into_inner(),
                    [LoginInput {
                        email: apple_id.to_owned(),
                        password: "secret".to_owned(),
                        auth_code: String::new(),
                    }]
                );
            }
        }
    }

    #[tokio::test]
    async fn prompts_for_an_apple_id() {
        for (answer, message) in [
            (Ok("user@example.com"), None),
            (Ok("+91 91234-56789"), None),
            (
                Err("failed to read input: EOF"),
                Some("failed to read Apple ID: failed to read input: EOF"),
            ),
        ] {
            let prompts = RefCell::new(Vec::new());
            let inputs = RefCell::new(Vec::new());
            let result = run(
                args("", "secret"),
                true,
                &quiet(),
                |text, masked| {
                    prompts.borrow_mut().push((text.to_owned(), masked));
                    answer.map(str::to_owned).map_err(str::to_owned)
                },
                async |input| {
                    inputs.borrow_mut().push(input.email);
                    Ok(LoginOutput::default())
                },
            )
            .await;
            assert_eq!(
                prompts.into_inner(),
                [(
                    "enter Apple ID (email address or phone number): ".to_owned(),
                    false
                )]
            );
            match message {
                None => {
                    result.unwrap();
                    assert_eq!(inputs.into_inner(), [answer.unwrap()]);
                }
                Some(message) => {
                    assert_eq!(result.unwrap_err().to_string(), message);
                    assert_eq!(inputs.into_inner(), Vec::<String>::new());
                }
            }
        }
    }

    #[tokio::test]
    async fn rejects_an_empty_password() {
        let error = run(
            args("user@example.com", ""),
            true,
            &quiet(),
            |text, masked| {
                assert_eq!((text, masked), ("enter password: ", true));
                Ok(" \t".to_owned())
            },
            async |_| unreachable!(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "password cannot be empty");
    }

    #[tokio::test]
    async fn reprompts_for_the_2fa_code_up_to_three_attempts() {
        let prompts = RefCell::new(Vec::new());
        let codes = RefCell::new(Vec::new());
        let error = run(
            args("user@example.com", "secret"),
            true,
            &quiet(),
            |text, _| {
                prompts.borrow_mut().push(text.to_owned());
                Ok(format!("00000{}", prompts.borrow().len()))
            },
            async |input| {
                codes.borrow_mut().push(input.auth_code.clone());
                Err(if input.auth_code.is_empty() {
                    Error::AuthCodeRequired
                } else {
                    Error::InvalidAuthCode(None)
                })
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "invalid or expired 2FA code");
        assert_eq!(codes.into_inner(), ["", "000001", "000002"]);
        assert_eq!(
            prompts.into_inner(),
            ["enter 2FA code: ", "invalid 2FA code, please try again: "]
        );
    }

    #[tokio::test]
    async fn succeeds_after_a_2fa_prompt() {
        let calls = RefCell::new(0);
        run(
            args("user@example.com", "secret"),
            true,
            &quiet(),
            |_, _| Ok("123456".to_owned()),
            async |input| {
                *calls.borrow_mut() += 1;
                if input.auth_code.is_empty() {
                    return Err(Error::AuthCodeRequired);
                }
                Ok(LoginOutput {
                    account: Account::default(),
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(calls.into_inner(), 2);
    }

    #[tokio::test]
    async fn stops_without_an_error_when_a_code_is_needed_non_interactively() {
        for error in [
            Error::AuthCodeRequired,
            Error::InvalidAuthCode(Some("detail")),
        ] {
            let calls = RefCell::new(0);
            let error = RefCell::new(Some(error));
            run(
                args("user@example.com", "secret"),
                false,
                &quiet(),
                |_, _| unreachable!(),
                async |_| {
                    *calls.borrow_mut() += 1;
                    Err(error.borrow_mut().take().unwrap())
                },
            )
            .await
            .unwrap();
            assert_eq!(calls.into_inner(), 1);
        }
    }

    #[tokio::test]
    async fn returns_other_errors_without_retrying() {
        let calls = RefCell::new(0);
        let error = run(
            args("user@example.com", "secret"),
            true,
            &quiet(),
            |_, _| unreachable!(),
            async |_| {
                *calls.borrow_mut() += 1;
                Err(Error::Invalid("account is disabled".to_owned()))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "account is disabled");
        assert_eq!(calls.into_inner(), 1);
    }
}
