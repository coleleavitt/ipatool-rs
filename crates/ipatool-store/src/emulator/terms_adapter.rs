//! Port of Go `internal/store/emulator/terms_adapter.go`.

use std::collections::BTreeMap;
use std::rc::Rc;

use super::callbacks::callback_function;
use super::runtime::host_block;
use super::{Guest, Payload, Result, Uc, method_key};
use crate::terms;

const CLASS: &str = "AMSFinanceActionResponse";
const SELECTOR: &str = "handleGotoURL:taskInfo:gotoType:accountURL:tidContinue:retryOnSuccess:";

/// Go's `installTermsActionAdapter`. The caller handles account terms over the Store API,
/// so only the presentation of that page is rejected: `Execute` then returns the original
/// finance response instead of falling back to `AppKit`. Other finance actions keep their
/// native behavior.
pub(crate) fn install_terms_action_adapter(uc: &mut Uc) -> Result<()> {
    if uc.sti().network.terms_action_adapter_installed {
        return Ok(());
    }
    let implementation = uc.lookup(CLASS, SELECTOR, true);
    let block = host_block(
        uc,
        Rc::new(move |uc: &mut Uc| {
            let url = uc.string(uc.arg(2)?)?;
            if !terms::is_page_url(&url) {
                let arguments = (0..8)
                    .map(|index| uc.arg(index))
                    .collect::<Result<Vec<_>>>()?;
                return callback_function(uc, implementation, &arguments, Box::new(|_| Ok(())));
            }
            let domain =
                uc.new_object("NSString", Payload::String("IPAToolStoreTerms".to_owned()))?;
            let code = uc.new_object("NSNumber", Payload::Int(1))?;
            let user_info = uc.new_object("NSDictionary", Payload::Dict(BTreeMap::new()))?;
            let failure = uc.new_object(
                "NSError",
                Payload::Dict(BTreeMap::from([
                    ("domain".to_owned(), domain),
                    ("code".to_owned(), code),
                    ("userInfo".to_owned(), user_info),
                ])),
            )?;
            let promise = uc.lookup("AMSPromise", "promiseWithError:", true);
            let class = uc.class_address("AMSPromise");
            let selector = uc.selector("promiseWithError:")?;
            callback_function(
                uc,
                promise,
                &[class, selector, failure],
                Box::new(|_| Ok(())),
            )
        }),
    )?;
    let entry = uc.read64(block + 16)?;
    uc.st()
        .methods
        .insert(method_key(CLASS, SELECTOR, true), entry);
    uc.st().network.terms_action_adapter_installed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Go's `terms_adapter_test.go`. Opt-in: `IPATOOL_TEST_SAP_CACHE=1` (needs the SAP
    //! asset cache filled; no request leaves the process).

    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::rc::Rc;

    use crate::emulator::{Request, Value, codec, runtime};
    use crate::http::{Header, HttpResponse};

    const BODY: &str = r#"<plist version="1.0"><dict><key>pings</key><array></array><key>jingleDocType</key><string>buyProductFailure</string><key>jingleAction</key><string>buyProduct</string><key>action</key><dict><key>kind</key><string>Goto</string><key>url</key><string>https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage</string><key>target</key><string>main</string></dict></dict></plist>"#;

    fn returns_the_original_terms_response(compressed: bool) {
        let bundle = ipatool_sap::assets::load().expect("cached Apple frameworks");
        let prepared = crate::profile::load().expect("embedded profile");
        let mut guest = runtime::open(&bundle.apple_media_services, prepared, true).expect("guest");
        let expected = codec::parse_plist(BODY.as_bytes()).expect("fixture plist");
        let mut headers = Header::new();
        headers.set("Content-Type", "text/xml; charset=UTF-8");
        let mut body = BODY.as_bytes().to_vec();
        if compressed {
            let mut writer =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            writer.write_all(&body).expect("gzip");
            body = writer.finish().expect("gzip");
            headers.set("Content-Encoding", "gzip");
        }
        let calls = Rc::new(Cell::new(0));
        let counter = Rc::clone(&calls);
        let parameters = BTreeMap::from([
            ("price".to_owned(), Value::String("0".to_owned())),
            ("productType".to_owned(), Value::String("C".to_owned())),
            ("salableAdamId".to_owned(), Value::String("1234".to_owned())),
            (
                "pricingParameters".to_owned(),
                Value::String("STDQ".to_owned()),
            ),
        ]);
        let mut response = guest
            .execute(Request {
                operation: "purchase".to_owned(),
                url: "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct"
                    .to_owned(),
                method: "POST".to_owned(),
                hardware_id: vec![0, 1, 2, 3, 4, 5],
                bag: Some(BTreeMap::from([(
                    "buyProduct".to_owned(),
                    Value::String("https://buy.itunes.apple.com/request".to_owned()),
                )])),
                parameters: Some(parameters),
                transport: Some(Box::new(move |_request| {
                    counter.set(counter.get() + 1);
                    Ok(HttpResponse::new(200, headers.clone(), body.clone()))
                })),
                ..Request::default()
            })
            .expect("the original finance response");
        let actual = response.read_body().expect("body");
        assert_eq!(calls.get(), 1);
        assert_eq!(
            codec::parse_plist(&actual).expect("decoded plist"),
            expected
        );
        assert_eq!(response.headers.get("Content-Encoding"), "");
    }

    #[test]
    fn returns_the_original_terms_response_to_the_caller() {
        if std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_none() {
            return;
        }
        returns_the_original_terms_response(false);
        // Issue 665: Apple compresses the finance response.
        returns_the_original_terms_response(true);
    }
}
