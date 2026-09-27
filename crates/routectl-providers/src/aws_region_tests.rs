use super::*;

const ACCEPTED: &[(&str, AwsPartition)] = &[
    ("us-west-2", AwsPartition::Commercial),
    ("us-east-1", AwsPartition::Commercial),
    ("eu-central-2", AwsPartition::Commercial),
    ("ap-southeast-7", AwsPartition::Commercial),
    ("ca-west-1", AwsPartition::Commercial),
    ("sa-east-1", AwsPartition::Commercial),
    ("me-central-1", AwsPartition::Commercial),
    ("af-south-1", AwsPartition::Commercial),
    ("il-central-1", AwsPartition::Commercial),
    ("mx-central-1", AwsPartition::Commercial),
    ("us-gov-west-1", AwsPartition::GovCloud),
    ("us-gov-east-1", AwsPartition::GovCloud),
    ("cn-north-1", AwsPartition::China),
    ("cn-northwest-1", AwsPartition::China),
];

#[test]
fn accepts_commercial_govcloud_and_china_regions_with_their_partition() {
    for &(region, partition) in ACCEPTED {
        let parsed = parse_aws_region(region)
            .unwrap_or_else(|e| panic!("{region:?} must be accepted, got {e:?}"));

        assert_eq!(parsed.as_str(), region);
        assert_eq!(parsed.partition(), partition, "{region}");
    }
}

#[test]
fn rejects_empty_region() {
    assert_eq!(parse_aws_region(""), Err(Rejection::Empty));
}

#[test]
fn rejects_url_structural_characters_dots_whitespace_and_uppercase() {
    let hostile = [
        "us-west-2.example.com",
        "us-west-2.",
        ".us-west-2",
        "x@127.0.0.1",
        "us-west-2/",
        "us-west-2/..",
        "us-west-2#",
        "us-west-2?x=1",
        "us-west-2:443",
        "us-west-2%2f",
        "us-west-2\\",
        "us-west-2 ",
        " us-west-2",
        "us-west-2\n",
        "us-west-2\r\nHost: evil",
        "us\twest-2",
        "US-WEST-2",
        "us-West-2",
        "us_west_2",
        "us-w\u{e9}st-2",
        "us-west-2\u{0}",
        "   ",
    ];

    for region in hostile {
        assert_eq!(
            parse_aws_region(region),
            Err(Rejection::IllegalCharacter),
            "{region:?}"
        );
    }
}

#[test]
fn rejects_well_charactered_values_that_are_not_region_shaped() {
    let malformed = [
        "us",
        "us-west",
        "us-west-",
        "-us-west-2",
        "us--west-2",
        "us-west-2-",
        "us-west-x",
        "us-2",
        "us-gov-west",
        "cn-1",
        "us-west-2-extra-1",
        "us-gov-west-1-1",
        "1-west-2",
        "us-w3st-2",
        "aws-global",
    ];

    for region in malformed {
        assert_eq!(
            parse_aws_region(region),
            Err(Rejection::Malformed),
            "{region:?}"
        );
    }
}

#[test]
fn rejects_region_shapes_outside_the_supported_partitions() {
    let unsupported = [
        "xx-west-2",
        "us-iso-east-1",
        "us-isob-east-1",
        "eu-isoe-west-1",
        "eusc-de-east-1",
        "localhost-a-1",
    ];

    for region in unsupported {
        assert_eq!(
            parse_aws_region(region),
            Err(Rejection::UnsupportedPartition),
            "{region:?}"
        );
    }
}

#[test]
fn accepts_a_region_exactly_at_the_length_bound() {
    let name = "a".repeat(MAX_AWS_REGION_LEN - "us--1".len());
    let region = format!("us-{name}-1");
    assert_eq!(region.len(), MAX_AWS_REGION_LEN);

    let parsed = parse_aws_region(&region);

    assert!(parsed.is_ok(), "{parsed:?}");
}

#[test]
fn rejects_a_region_one_byte_over_the_length_bound() {
    let name = "a".repeat(MAX_AWS_REGION_LEN + 1 - "us--1".len());
    let region = format!("us-{name}-1");
    assert_eq!(region.len(), MAX_AWS_REGION_LEN + 1);

    assert_eq!(parse_aws_region(&region), Err(Rejection::TooLong));
}

#[test]
fn rejects_a_very_long_hostile_value_as_too_long_before_scanning_it() {
    let region = format!("us-west-2{}", "@evil.example".repeat(10_000));

    assert_eq!(parse_aws_region(&region), Err(Rejection::TooLong));
}

#[test]
fn error_message_names_the_expected_shape_without_echoing_the_input() {
    let err = parse_aws_region("us-west-2\u{1b}[31m@evil").unwrap_err();

    let msg = err.to_string();

    assert!(msg.contains("us-west-2"), "names an example: {msg}");
    assert!(!msg.contains("evil"), "must not echo input: {msg}");
    assert!(!msg.contains('\u{1b}'), "must not echo input: {msg}");
}

#[test]
fn validate_aws_region_accepts_every_supported_partition() {
    for &(region, _) in ACCEPTED {
        let result = validate_aws_region(region);

        assert!(result.is_ok(), "{region:?}: {result:?}");
    }
}

#[test]
fn validate_aws_region_rejects_host_altering_values() {
    for region in [
        "",
        "us-west-2.evil.example",
        "x@127.0.0.1:9/",
        "US-WEST-2",
        "aws-global",
    ] {
        let result = validate_aws_region(region);

        assert!(result.is_err(), "{region:?} must be refused");
    }
}
