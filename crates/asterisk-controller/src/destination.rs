// crates/asterisk-controller/src/destination.rs

//! Whether the node may dial a number, and through whom.
//!
//! The telephony module has asked the same question before it sent the
//! command. The node asks it again, by itself: outbound calls are where
//! telephone fraud takes its money, and this line of defence has to hold
//! even if the application's own check was wrong. So the answer here comes
//! from the node's settings and from numbering data compiled into the
//! controller — nothing the command says about its own number is believed.

use node_protocol::messages::{CommandRejection, Line, LineId, Operator, PhoneNumber, Settings};
use phonenumber::Type;

/// How a number that may be dialled is dialled.
pub struct Route<'a> {
    /// The line the call is made on: its number is the one shown.
    pub line: &'a Line,
    /// The operator that carries the call.
    pub operator: &'a Operator,
}

/// Judge a number against a line of the settings.
///
/// # Errors
///
/// The reason the number is not dialled, as the application is told it.
pub fn judge<'a>(
    settings: &'a Settings,
    line: &LineId,
    number: &PhoneNumber,
) -> Result<Route<'a>, CommandRejection> {
    let line = settings
        .lines
        .iter()
        .find(|known| known.id == *line)
        .ok_or(CommandRejection::UnknownLine)?;
    let country = country_of(number).ok_or(CommandRejection::DestinationNotAllowed)?;
    if !line
        .allowed_countries
        .iter()
        .any(|allowed| allowed.as_str() == country)
    {
        return Err(CommandRejection::DestinationNotAllowed);
    }
    // Checked settings name the operator of every line; a line without one
    // dials nothing.
    let operator = settings
        .operators
        .iter()
        .find(|operator| operator.id == line.operator)
        .filter(|operator| {
            operator
                .countries
                .iter()
                .any(|carried| carried.as_str() == country)
        })
        .ok_or(CommandRejection::NoOperatorForDestination)?;
    Ok(Route { line, operator })
}

/// The country a number belongs to, as its two-letter code — for a number
/// that may be dialled at all.
///
/// `None` for a number the numbering data does not know as valid, for one
/// that belongs to no single country, and for a premium-rate number: those
/// are never dialled, whatever a line allows.
fn country_of(number: &PhoneNumber) -> Option<String> {
    let parsed = phonenumber::parse(None, number.as_str()).ok()?;
    if !parsed.is_valid()
        || parsed.number_type(&phonenumber::metadata::DATABASE) == Type::PremiumRate
    {
        return None;
    }
    parsed
        .country()
        .id()
        .map(|country| country.as_ref().to_owned())
}

#[cfg(test)]
mod tests {
    use node_protocol::messages::{CommandRejection, Settings};
    use serde_json::json;

    use super::{country_of, judge};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn settings() -> Result<Settings, Box<dyn std::error::Error>> {
        Ok(node_protocol::decode_value(json!({
            "operators": [{
                "id": "carrier", "host": "192.0.2.1", "port": 5060, "transport": "udp",
                "source_networks": [], "countries": ["US", "GB"]
            }],
            "lines": [
                { "id": "main", "number": "+19715870050", "operator": "carrier",
                  "allowed_countries": ["US", "CA"], "concurrent_outbound_limit": 2 },
                { "id": "closed", "number": "+19715870050", "operator": "carrier",
                  "allowed_countries": [], "concurrent_outbound_limit": 2 }
            ],
            "entries": [],
            "prompts": []
        }))?)
    }

    #[test]
    fn a_country_is_told_inside_a_shared_country_code() -> TestResult {
        // The United States, Canada and Jamaica all begin with +1.
        for (number, country) in [
            ("+15035550100", Some("US")),
            ("+15062345678", Some("CA")),
            ("+18762101234", Some("JM")),
            ("+442071838750", Some("GB")),
            ("+995322123456", Some("GE")),
        ] {
            assert_eq!(country_of(&number.parse()?).as_deref(), country, "{number}");
        }
        Ok(())
    }

    #[test]
    fn a_premium_rate_or_impossible_number_has_no_country_to_be_allowed_in() -> TestResult {
        for number in ["+19002345678", "+10005550100", "+4490912345"] {
            assert_eq!(country_of(&number.parse()?), None, "{number}");
        }
        Ok(())
    }

    #[test]
    fn a_number_is_judged_by_the_line_and_then_by_its_operator() -> TestResult {
        let settings = settings()?;
        let verdict = |line: &str, number: &str| -> Result<_, Box<dyn std::error::Error>> {
            Ok(judge(&settings, &line.parse()?, &number.parse()?)
                .err()
                .map(|reason| reason.to_string()))
        };
        let reason = |reason: CommandRejection| Some(reason.to_string());
        assert_eq!(verdict("main", "+15035550100")?, None);
        assert_eq!(
            verdict("nowhere", "+15035550100")?,
            reason(CommandRejection::UnknownLine)
        );
        // Allowed by no line: Jamaica, a premium-rate number, a line that allows nothing.
        for (line, number) in [
            ("main", "+18762101234"),
            ("main", "+19002345678"),
            ("closed", "+15035550100"),
        ] {
            assert_eq!(
                verdict(line, number)?,
                reason(CommandRejection::DestinationNotAllowed),
                "{line} {number}"
            );
        }
        // Allowed by the line, carried by no operator.
        assert_eq!(
            verdict("main", "+15062345678")?,
            reason(CommandRejection::NoOperatorForDestination)
        );
        Ok(())
    }
}
