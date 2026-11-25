//! The token-count limits' startup parsing (`count_limit`).
use std::env::VarError;

use super::count_limit;

const NAME: &str = "GATEWAY_COUNT_TOKENS_CONCURRENCY";

fn parse(value: Option<&str>) -> anyhow::Result<u32> {
    let value = value.map(str::to_owned);
    count_limit(
        move |name| {
            assert_eq!(name, NAME);
            value.clone().ok_or(VarError::NotPresent)
        },
        NAME,
        8,
        1_024,
    )
}

#[test]
fn an_unset_limit_is_the_default() {
    assert_eq!(parse(None).unwrap(), 8);
}

#[test]
fn whole_numbers_in_range_are_accepted_including_leading_zeros() {
    assert_eq!(parse(Some("1")).unwrap(), 1);
    assert_eq!(parse(Some("1024")).unwrap(), 1_024);
    assert_eq!(parse(Some("007")).unwrap(), 7);
}

#[test]
fn anything_else_stops_startup_naming_the_setting() {
    for value in [
        "0",
        "",
        " ",
        "abc",
        " 5",
        "5 ",
        "\t5",
        "+5",
        "-5",
        "1025",
        "4294967296",
        "5.0",
        "0x10",
        "५",
    ] {
        let error = parse(Some(value)).expect_err(value).to_string();
        assert_eq!(
            error,
            format!("{NAME} must be an integer from 1 to 1024"),
            "{value:?}"
        );
    }
    let error = count_limit(
        |_| Err(VarError::NotUnicode(std::ffi::OsString::new())),
        NAME,
        8,
        1_024,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), format!("{NAME} is not valid Unicode"));
}
