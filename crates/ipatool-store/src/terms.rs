//! Port of Go `internal/store/terms`: recognizes the Store's account terms flow.

use crate::gourl;

/// Go's `IsBuyHost`: the Store purchase service and its numbered account pods.
#[must_use]
pub fn is_buy_host(host: &str) -> bool {
    let host = host.to_lowercase();
    if host == "buy.itunes.apple.com" {
        return true;
    }
    let Some(pod) = host.strip_suffix("-buy.itunes.apple.com") else {
        return false;
    };
    let Some(digits) = pod.strip_prefix('p') else {
        return false;
    };
    // Go checks the byte length before stripping the `p`, so `p` alone is rejected.
    !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_digit())
}

/// Go's `IsPageURL`: the terms action returned by `buyProduct`.
#[must_use]
pub fn is_page_url(raw: &str) -> bool {
    gourl::parse(raw).is_ok_and(|u| {
        u.scheme == "https"
            && is_buy_host(u.hostname())
            && (u.port().is_empty() || u.port() == "443")
            && u.user.is_none()
            && u.fragment.is_empty()
            && u.raw_path.is_empty()
            && u.path
                == "/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage"
    })
}
