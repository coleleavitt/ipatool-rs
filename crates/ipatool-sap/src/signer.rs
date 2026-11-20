//! The SAP action signer used by login and purchase. Port of Go `internal/sap/signer.go` and
//! `signer_local.go`.

use std::time::Duration;

use crate::assets;
use crate::error::{Context, Error, Result};
use crate::machine::Machine;
use crate::protocol::SetupProtocol;

const SUPPORTED_VERSION: u32 = 200;
const SIGNER_CLOSED: &str = "SAP signer is closed";

pub struct Config {
    /// Go's `HTTPClient`; `None` uses a client with a 30 s timeout.
    pub http_client: Option<reqwest::Client>,
    pub setup_url: String,
    pub certificate_url: String,
    pub version: u32,
    pub hardware_id: Vec<u8>,
}

pub trait ActionSigner {
    fn sign(&mut self, input: &[u8]) -> Result<Vec<u8>>;
    fn close(&mut self) -> Result<()>;
}

fn validate_config(config: &Config) -> Result<()> {
    if config.version != SUPPORTED_VERSION {
        return Err(Error::msg(format!(
            "unsupported SAP version {}",
            config.version
        )));
    }
    if config.hardware_id.is_empty() || config.hardware_id.len() > 20 {
        return Err(Error::msg(
            "SAP hardware ID must contain between 1 and 20 bytes",
        ));
    }
    validate_endpoint("setup", &config.setup_url)?;
    validate_endpoint("certificate", &config.certificate_url)
}

fn validate_endpoint(label: &str, value: &str) -> Result<()> {
    let valid = url::Url::parse(value).is_ok_and(|endpoint| {
        endpoint.scheme() == "https"
            && endpoint.host_str().is_some_and(|host| !host.is_empty())
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
    });
    if valid {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "SAP {label} URL must be an absolute HTTPS URL"
        )))
    }
}

/// Go's `sap.Signer`: one SAP session set up with Apple, signing requests until closed.
pub struct Signer {
    machine: Option<Machine>,
    context: u64,
    hardware: Vec<u8>,
    closed: bool,
}

impl Signer {
    /// Go's `NewSigner`. The guest runs on this thread, so the future is not `Send`.
    pub async fn new(config: Config) -> Result<Self> {
        validate_config(&config)?;
        let bundle = assets::load().context("load Apple SAP assets")?;
        let guest = Machine::open(&bundle).context("start Apple SAP runtime")?;
        let mut signer = Self {
            machine: Some(guest),
            context: 0,
            hardware: config.hardware_id.clone(),
            closed: false,
        };
        let protocol = SetupProtocol {
            client: match config.http_client.clone() {
                Some(client) => client,
                None => reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|error| Error::msg(error.to_string()))?,
            },
        };
        signer.set_up(&protocol, &config).await?;
        Ok(signer)
    }

    async fn set_up(&mut self, protocol: &SetupProtocol, config: &Config) -> Result<()> {
        let hardware = self.hardware.clone();
        let guest = self.machine()?;
        let context = guest
            .initialize(&hardware)
            .context("initialize Apple SAP session")?;
        self.context = context;
        let certificate = protocol.certificate(&config.certificate_url).await?;
        let (request, state) = self
            .machine()?
            .exchange(config.version, &hardware, context, &certificate)
            .context("create SAP setup message")?;
        if state != 1 {
            return Err(Error::msg(format!(
                "SAP setup entered unexpected state {state}"
            )));
        }
        if request.is_empty() {
            return Err(Error::msg("SAP setup message is empty"));
        }
        let reply = protocol.exchange(&config.setup_url, &request).await?;
        let (_, state) = self
            .machine()?
            .exchange(config.version, &hardware, context, &reply)
            .context("complete SAP setup")?;
        if state != 0 {
            return Err(Error::msg(format!(
                "SAP setup completed in unexpected state {state}"
            )));
        }
        Ok(())
    }

    fn machine(&mut self) -> Result<&mut Machine> {
        self.machine
            .as_mut()
            .ok_or_else(|| Error::msg(SIGNER_CLOSED))
    }
}

impl ActionSigner for Signer {
    fn sign(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::msg(SIGNER_CLOSED));
        }
        let context = self.context;
        let signature = self
            .machine()?
            .sign(context, input)
            .context("sign Apple request")?;
        if signature.is_empty() {
            return Err(Error::msg("sign Apple request: signature is empty"));
        }
        Ok(signature)
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut errors = Vec::new();
        if let Some(machine) = self.machine.as_mut() {
            if self.context != 0 {
                errors.push(machine.teardown(self.context).err());
                self.context = 0;
            }
            errors.push(machine.close().err());
        }
        self.machine = None;
        self.hardware.fill(0);
        Error::join(errors).map_or(Ok(()), Err)
    }
}

impl Drop for Signer {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Config {
        Config {
            http_client: None,
            setup_url: "https://example.apple.com/setup".to_owned(),
            certificate_url: "https://example.apple.com/certificate".to_owned(),
            version: 200,
            hardware_id: vec![1, 2, 3, 4, 5, 6],
        }
    }

    #[test]
    fn validates_config() {
        validate_config(&valid()).unwrap();
        let cases: [(&str, fn(&mut Config), &str); 6] = [
            (
                "version",
                |config| config.version = 210,
                "unsupported SAP version 210",
            ),
            (
                "empty hardware ID",
                |config| config.hardware_id.clear(),
                "SAP hardware ID must contain between 1 and 20 bytes",
            ),
            (
                "long hardware ID",
                |config| config.hardware_id = vec![0; 21],
                "SAP hardware ID must contain between 1 and 20 bytes",
            ),
            (
                "relative setup URL",
                |config| config.setup_url = "/setup".to_owned(),
                "SAP setup URL must be an absolute HTTPS URL",
            ),
            (
                "HTTP certificate URL",
                |config| config.certificate_url = "http://example.apple.com/cert".to_owned(),
                "SAP certificate URL must be an absolute HTTPS URL",
            ),
            (
                "credentialed URL",
                |config| config.setup_url = "https://user@example.apple.com/setup".to_owned(),
                "SAP setup URL must be an absolute HTTPS URL",
            ),
        ];
        for (name, mutate, message) in cases {
            let mut config = valid();
            mutate(&mut config);
            let error = validate_config(&config).unwrap_err();
            assert_eq!(error.to_string(), message, "{name}");
        }
    }

    #[tokio::test]
    #[ignore = "contacts Apple's SAP setup endpoints"]
    async fn signs_after_live_setup() {
        let mut signer = Signer::new(Config {
            setup_url: "https://fpinit.itunes.apple.com/v1/signSapSetup/legacy".to_owned(),
            certificate_url: "https://s.mzstatic.com/sap/setupCert.plist".to_owned(),
            ..valid()
        })
        .await
        .unwrap();
        assert_ne!(signer.sign(b"message").unwrap(), Vec::<u8>::new());
        signer.close().unwrap();
    }

    #[test]
    fn closed_signer_rejects_signing() {
        let mut signer = Signer {
            machine: None,
            context: 0,
            hardware: Vec::new(),
            closed: true,
        };
        assert_eq!(
            signer.sign(b"message").unwrap_err().to_string(),
            SIGNER_CLOSED
        );
        signer.close().unwrap();
    }
}
