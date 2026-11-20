//! Storefront routing for phone-number Apple Accounts (`appstore_login_storefront.go`).

/// Go's `phoneNumberStorefrontCountryCode`: `CN` or `IN` for phone-number Apple IDs of
/// mainland China and India, else empty. The Apple ID sent to Apple is not changed.
pub(crate) fn phone_number_storefront_country_code(apple_id: &str) -> &'static str {
    let number = apple_id.trim().replace([' ', '-'], "");
    let (international, number) = if let Some(rest) = number.strip_prefix('+') {
        (true, rest)
    } else if let Some(rest) = number.strip_prefix("00") {
        (true, rest)
    } else {
        (false, number.as_str())
    };
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return "";
    }
    // Check the full length before removing a country code: a local Indian number can
    // itself start with 91. International numbers must use a supported country code.
    let mut number = number;
    if number.len() == 13 && number.starts_with("861") {
        return "CN";
    } else if number.len() == 12 && number.starts_with("91") {
        number = &number[2..];
    } else if international {
        return "";
    }
    if number.len() == 11 && number.starts_with('1') {
        return "CN";
    }
    // India also permits a trunk prefix in national numbers.
    if !international && number.len() == 11 && number.starts_with('0') {
        number = &number[1..];
    }
    if number.len() == 10 && number.starts_with(['6', '7', '8', '9']) {
        return "IN";
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_phone_number_storefronts() {
        for (apple_id, country) in [
            ("13912345678", "CN"),
            ("+8613912345678", "CN"),
            ("008613912345678", "CN"),
            ("8613912345678", "CN"),
            (" +86 139-1234-5678 ", "CN"),
            ("9123456789", "IN"),
            ("8612345678", "IN"),
            ("6123456789", "IN"),
            ("09123456789", "IN"),
            ("+919123456789", "IN"),
            ("00919123456789", "IN"),
            ("919123456789", "IN"),
            ("+91 91234-56789", "IN"),
            ("person@example.test", ""),
            ("13912345678@example.test", ""),
            ("some-account", ""),
            ("", ""),
            (" -- ", ""),
            ("+", ""),
            ("00", ""),
            ("139x12345678", ""),
            ("139/1234/5678", ""),
            ("+８６１３９１２３４５６７８", ""),
            ("＋8613912345678", ""),
            ("139\n12345678", ""),
            ("139+12345678", ""),
            ("++8613912345678", ""),
            ("+008613912345678", ""),
            ("+13912345678", ""),
            ("0013912345678", ""),
            ("+8612345678", ""),
            ("+9123456789", ""),
            ("+861391234567", ""),
            ("+86139123456789", ""),
            ("8609123456789", ""),
            ("+8623912345678", ""),
            ("+91912345678", ""),
            ("+9191234567890", ""),
            ("+915123456789", ""),
            ("+9109123456789", ""),
        ] {
            assert_eq!(
                phone_number_storefront_country_code(apple_id),
                country,
                "{apple_id:?}"
            );
        }
    }
}
