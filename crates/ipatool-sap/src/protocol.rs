//! Apple's SAP setup endpoints: the setup certificate and the setup message exchange.
//! Port of Go `internal/sap/protocol.go`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{CONTENT_TYPE, USER_AGENT};

use crate::error::{Context, Error, Result};

const SETUP_CERTIFICATE_KEY: &str = "sign-sap-setup-cert";
const SETUP_BUFFER_KEY: &str = "sign-sap-setup-buffer";
const MAX_SETUP_BODY: usize = 1 << 20;
/// Go `pkg/http.DefaultUserAgent`.
const DEFAULT_USER_AGENT: &str =
    "Configurator/2.17 (Macintosh; OS X 15.2; 24C5089c) AppleWebKit/0620.1.16.11.6";

pub(crate) struct SetupProtocol {
    pub(crate) client: reqwest::Client,
}

impl SetupProtocol {
    pub(crate) async fn certificate(&self, endpoint: &str) -> Result<Vec<u8>> {
        let request = self
            .client
            .get(endpoint)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .build()
            .map_err(|error| Error::msg(error.to_string()))
            .context("create SAP certificate request")?;
        let body = self.send(request).await.context("fetch SAP certificate")?;
        plist_bytes(&body, SETUP_CERTIFICATE_KEY)
    }

    pub(crate) async fn exchange(&self, endpoint: &str, input: &[u8]) -> Result<Vec<u8>> {
        let request = self
            .client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/x-plist")
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .body(setup_envelope(input))
            .build()
            .map_err(|error| Error::msg(error.to_string()))
            .context("create SAP setup request")?;
        let body = self
            .send(request)
            .await
            .context("exchange SAP setup message")?;
        plist_bytes(&body, SETUP_BUFFER_KEY)
    }

    async fn send(&self, request: reqwest::Request) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .execute(request)
            .await
            .map_err(|error| Error::msg(error.to_string()))
            .context("send SAP request")?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| Error::msg(error.to_string()))
            .context("read SAP response")?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_SETUP_BODY {
                break;
            }
        }
        if status != reqwest::StatusCode::OK {
            return Err(Error::msg(format!("apple returned {status}")));
        }
        if body.len() > MAX_SETUP_BODY {
            return Err(Error::msg(format!(
                "apple response exceeds {MAX_SETUP_BODY} bytes"
            )));
        }
        Ok(body)
    }
}

/// The XML plist Go's `howett.net/plist` writes for `{sign-sap-setup-buffer: input}`.
fn setup_envelope(input: &[u8]) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>{SETUP_BUFFER_KEY}</key><data>{}</data></dict></plist>",
        STANDARD.encode(input)
    )
}

fn plist_bytes(document: &[u8], key: &str) -> Result<Vec<u8>> {
    let values: plist::Dictionary = plist::from_bytes(document)
        .map_err(|error| Error::msg(error.to_string()))
        .context("decode Apple plist")?;
    match values.get(key) {
        Some(plist::Value::Data(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(Error::msg(format!("Apple plist is missing {key}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn data_plist(key: &str, value: &[u8]) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>{key}</key><data>{}</data></dict></plist>",
            STANDARD.encode(value)
        )
    }

    /// Serves `count` HTTP/1.1 requests, answering each with `respond(method, headers, body)`.
    fn serve(
        count: usize,
        respond: impl Fn(&str, &[String], &[u8]) -> (u16, Vec<u8>) + Send + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(count) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let method = line.split(' ').next().unwrap_or_default().to_owned();
                let mut headers = Vec::new();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end().to_owned();
                    if header.is_empty() {
                        break;
                    }
                    if let Some(value) =
                        header.to_ascii_lowercase().strip_prefix("content-length: ")
                    {
                        length = value.parse().unwrap();
                    }
                    headers.push(header);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (status, reply) = respond(&method, &headers, &body);
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                )
                .unwrap();
                stream.write_all(&reply).unwrap();
            }
        });
        format!("http://{address}")
    }

    fn protocol() -> SetupProtocol {
        SetupProtocol {
            client: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn fetches_certificate_and_exchanges_setup_message() {
        let base = serve(2, |method, headers, body| {
            assert!(
                headers
                    .iter()
                    .any(|header| header == &format!("user-agent: {DEFAULT_USER_AGENT}"))
            );
            if method == "POST" {
                assert!(
                    headers
                        .iter()
                        .any(|header| header == "content-type: application/x-plist")
                );
                assert_eq!(plist_bytes(body, SETUP_BUFFER_KEY).unwrap(), b"request");
                return (200, data_plist(SETUP_BUFFER_KEY, b"reply").into_bytes());
            }
            (
                200,
                data_plist(SETUP_CERTIFICATE_KEY, b"certificate").into_bytes(),
            )
        });
        let certificate = protocol()
            .certificate(&format!("{base}/certificate"))
            .await
            .unwrap();
        assert_eq!(certificate, b"certificate");
        let reply = protocol()
            .exchange(&format!("{base}/setup"), b"request")
            .await
            .unwrap();
        assert_eq!(reply, b"reply");
    }

    #[tokio::test]
    async fn rejects_oversized_response() {
        let base = serve(1, |_, _, _| (200, vec![b'x'; MAX_SETUP_BODY + 1]));
        let error = protocol()
            .certificate(&format!("{base}/certificate"))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "fetch SAP certificate: apple response exceeds 1048576 bytes"
        );
    }

    #[tokio::test]
    async fn reports_status_and_missing_key() {
        let base = serve(2, |method, _, _| {
            if method == "GET" {
                (404, Vec::new())
            } else {
                (200, data_plist("other", b"x").into_bytes())
            }
        });
        let error = protocol()
            .certificate(&format!("{base}/certificate"))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "fetch SAP certificate: apple returned 404 Not Found"
        );
        let error = protocol()
            .exchange(&format!("{base}/setup"), b"request")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Apple plist is missing sign-sap-setup-buffer"
        );
    }

    #[test]
    fn writes_go_setup_envelope() {
        assert_eq!(
            setup_envelope(&[0; 3]),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>sign-sap-setup-buffer</key><data>AAAA</data></dict></plist>"
        );
    }
}
