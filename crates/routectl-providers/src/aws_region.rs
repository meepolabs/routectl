//! Canonical AWS region identifiers for region-derived endpoints.
//!
//! The native Bedrock and mantle lanes build their endpoint host by
//! interpolating an operator-supplied region into a hostname template and
//! sign the request under that same region. A value carrying URL
//! structure (`@`, `/`, `#`, `?`, `:`, `.`) or whitespace would change the
//! parsed host, so every region-derived builder and signer parses the
//! region here first and refuses anything that is not a canonical
//! identifier.

/// Longest region identifier accepted. The longest region in the three
/// supported partitions is 14 bytes (`ap-southeast-7`, `cn-northwest-1` in
/// the AWS SDK partition metadata); the bound leaves headroom for new
/// region names while keeping a hostile value from growing a hostname
/// label without limit.
pub const MAX_AWS_REGION_LEN: usize = 32;

/// The AWS partition a region belongs to, which selects its DNS suffixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AwsPartition {
    /// The commercial `aws` partition.
    Commercial,
    /// The `aws-us-gov` (GovCloud) partition.
    GovCloud,
    /// The `aws-cn` (China) partition.
    China,
}

impl AwsPartition {
    /// Regional service DNS suffix (`<service>.<region>.<suffix>`), per the
    /// AWS SDK partition metadata.
    pub const fn dns_suffix(self) -> &'static str {
        match self {
            Self::Commercial | Self::GovCloud => "amazonaws.com",
            Self::China => "amazonaws.com.cn",
        }
    }

    /// Dual-stack DNS suffix, the one the `bedrock-mantle` hosts live
    /// under, per the AWS SDK partition metadata.
    pub const fn dual_stack_dns_suffix(self) -> &'static str {
        match self {
            Self::Commercial | Self::GovCloud => "api.aws",
            Self::China => "api.amazonwebservices.com.cn",
        }
    }
}

/// A region identifier that passed [`parse_aws_region`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwsRegion<'a> {
    name: &'a str,
    partition: AwsPartition,
}

impl<'a> AwsRegion<'a> {
    /// The identifier exactly as supplied (already canonical).
    pub const fn as_str(&self) -> &'a str {
        self.name
    }

    /// The partition the identifier belongs to.
    pub const fn partition(&self) -> AwsPartition {
        self.partition
    }
}

/// Why a region identifier was refused. The rejected value is deliberately
/// not carried: it is untrusted input and may hold control characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidAwsRegion {
    /// The identifier is empty.
    Empty,
    /// The identifier exceeds [`MAX_AWS_REGION_LEN`] bytes.
    TooLong,
    /// A byte outside lowercase `a-z`, `0-9` and `-`.
    IllegalCharacter,
    /// The characters are legal but the shape is not `<area>-<name>-<n>`.
    Malformed,
    /// Region-shaped, but not in the commercial, GovCloud or China
    /// partition, whose endpoint suffixes this crate does not know.
    UnsupportedPartition,
}

impl std::fmt::Display for InvalidAwsRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Empty => "it is empty",
            Self::TooLong => "it is too long",
            Self::IllegalCharacter => {
                "it contains a character other than lowercase a-z, 0-9 or '-'"
            }
            Self::Malformed => "it is not of the form <area>-<name>-<number>",
            Self::UnsupportedPartition => {
                "it is not in the commercial, GovCloud or China partition"
            }
        };
        write!(
            f,
            "not a canonical AWS region identifier (e.g. \"us-west-2\", \"us-gov-west-1\", \
             \"cn-north-1\"): {reason}"
        )
    }
}

impl std::error::Error for InvalidAwsRegion {}

/// Parse `region` as a canonical AWS region identifier.
///
/// Accepted shapes, all lowercase ASCII, `<n>` one or more digits:
///   - commercial: `<area>-<name>-<n>` with `<area>` one of the commercial
///     area codes (e.g. `us-west-2`, `eu-central-2`, `il-central-1`)
///   - GovCloud: `us-gov-<name>-<n>` (e.g. `us-gov-west-1`)
///   - China: `cn-<name>-<n>` (e.g. `cn-north-1`)
///
/// The length bound is checked before anything else scans the input.
pub fn parse_aws_region(region: &str) -> Result<AwsRegion<'_>, InvalidAwsRegion> {
    if region.is_empty() {
        return Err(InvalidAwsRegion::Empty);
    }
    if region.len() > MAX_AWS_REGION_LEN {
        return Err(InvalidAwsRegion::TooLong);
    }
    if !region
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(InvalidAwsRegion::IllegalCharacter);
    }
    let partition = match region.split('-').collect::<Vec<_>>().as_slice() {
        [area, name, number] if is_word(area) && is_word(name) && is_number(number) => {
            partition_for_area(area)?
        }
        [area, qualifier, name, number]
            if is_word(area) && is_word(qualifier) && is_word(name) && is_number(number) =>
        {
            if *area == "us" && *qualifier == "gov" {
                AwsPartition::GovCloud
            } else {
                return Err(InvalidAwsRegion::UnsupportedPartition);
            }
        }
        _ => return Err(InvalidAwsRegion::Malformed),
    };
    Ok(AwsRegion {
        name: region,
        partition,
    })
}

/// [`parse_aws_region`] with the refusal mapped to [`routectl_core::Error::Config`],
/// for the endpoint builders and signers that return the crate `Result`.
pub fn require_aws_region(region: &str) -> routectl_core::Result<AwsRegion<'_>> {
    parse_aws_region(region)
        .map_err(|e| routectl_core::Error::Config(format!("invalid AWS region: {e}")))
}

/// Commercial area codes, matching the `aws` partition's region pattern in
/// the AWS SDK endpoint metadata.
const COMMERCIAL_AREAS: &[&str] = &["us", "eu", "ap", "sa", "ca", "me", "af", "il", "mx"];

fn partition_for_area(area: &str) -> Result<AwsPartition, InvalidAwsRegion> {
    if area == "cn" {
        Ok(AwsPartition::China)
    } else if COMMERCIAL_AREAS.contains(&area) {
        Ok(AwsPartition::Commercial)
    } else {
        Err(InvalidAwsRegion::UnsupportedPartition)
    }
}

fn is_word(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_lowercase())
}

fn is_number(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
#[path = "aws_region_tests.rs"]
mod tests;
