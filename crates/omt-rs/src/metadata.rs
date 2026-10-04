//! Tally and sender information: the two pieces of OMT metadata with a fixed
//! meaning, in the exact form the official libomt writes and expects.

/// Program / preview tally.
///
/// A receiver tells a sender whether it has that source on program or
/// preview; the sender combines (ORs) the tally of every receiver and sends
/// the result back to all of them, so each receiver can show whether the
/// source is live anywhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Tally {
    pub program: bool,
    pub preview: bool,
}

// The official implementation compares these strings exactly, and they carry
// a double `==` typo that it keeps for compatibility; so must we.
const TALLY_NONE: &str = r#"<OMTTally Preview="false" Program=="false" />"#;
const TALLY_PREVIEW: &str = r#"<OMTTally Preview="true" Program=="false" />"#;
const TALLY_PROGRAM: &str = r#"<OMTTally Preview="false" Program=="true" />"#;
const TALLY_BOTH: &str = r#"<OMTTally Preview="true" Program=="true" />"#;

impl Tally {
    pub const NONE: Tally = Tally {
        program: false,
        preview: false,
    };

    /// The metadata string for this tally, byte for byte as libomt writes it.
    pub fn to_xml(self) -> &'static str {
        match (self.preview, self.program) {
            (false, false) => TALLY_NONE,
            (true, false) => TALLY_PREVIEW,
            (false, true) => TALLY_PROGRAM,
            (true, true) => TALLY_BOTH,
        }
    }

    /// Parses a tally metadata string; `None` if `xml` is not one. Exact
    /// match, as libomt does.
    pub fn from_xml(xml: &str) -> Option<Self> {
        match xml.trim_end_matches('\0') {
            TALLY_NONE => Some(Self {
                preview: false,
                program: false,
            }),
            TALLY_PREVIEW => Some(Self {
                preview: true,
                program: false,
            }),
            TALLY_PROGRAM => Some(Self {
                preview: false,
                program: true,
            }),
            TALLY_BOTH => Some(Self {
                preview: true,
                program: true,
            }),
            _ => None,
        }
    }

    /// Either flag set in `self` or `other`.
    pub fn or(self, other: Tally) -> Tally {
        Tally {
            program: self.program || other.program,
            preview: self.preview || other.preview,
        }
    }
}

/// What a sender says about itself. Sent to every receiver on connect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderInfo {
    pub product_name: String,
    pub manufacturer: String,
    pub version: String,
}

impl SenderInfo {
    /// `<OMTInfo ProductName="..." Manufacturer="..." Version="..." />`, as
    /// libomt's `OMTSenderInfo.ToXML` writes it.
    pub fn to_xml(&self) -> String {
        format!(
            r#"<OMTInfo ProductName="{}" Manufacturer="{}" Version="{}" />"#,
            escape(&self.product_name),
            escape(&self.manufacturer),
            escape(&self.version)
        )
    }

    /// Parses an `OMTInfo` element; `None` if `xml` is not one. Missing
    /// attributes come back empty, as in libomt.
    pub fn from_xml(xml: &str) -> Option<Self> {
        let xml = xml.trim_start();
        if !xml.starts_with("<OMTInfo") {
            return None;
        }
        Some(Self {
            product_name: attribute(xml, "ProductName").unwrap_or_default(),
            manufacturer: attribute(xml, "Manufacturer").unwrap_or_default(),
            version: attribute(xml, "Version").unwrap_or_default(),
        })
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The value of attribute `name` in a single XML element (either quote
/// style). Small on purpose: OMT metadata elements are one line.
fn attribute(xml: &str, name: &str) -> Option<String> {
    let mut rest = xml;
    while let Some(i) = rest.find(name) {
        let before = rest[..i].chars().last();
        let after = &rest[i + name.len()..];
        let after_trim = after.trim_start();
        if before.is_some_and(|c| c.is_whitespace()) && after_trim.starts_with('=') {
            let value = after_trim[1..].trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = value[1..].find(quote)?;
                return Some(unescape(&value[1..1 + end]));
            }
        }
        rest = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tally_strings_match_libomt_including_its_typo() {
        assert_eq!(
            Tally {
                program: true,
                preview: false
            }
            .to_xml(),
            r#"<OMTTally Preview="false" Program=="true" />"#
        );
        for program in [false, true] {
            for preview in [false, true] {
                let t = Tally { program, preview };
                assert_eq!(Tally::from_xml(t.to_xml()), Some(t));
            }
        }
        assert_eq!(
            Tally::from_xml(r#"<OMTTally Preview="false" Program="true" />"#),
            None
        );
    }

    #[test]
    fn sender_info_round_trips_and_escapes() {
        let info = SenderInfo {
            product_name: "Helm \"Pro\" & <More>".into(),
            manufacturer: "Meros".into(),
            version: "1.2.3".into(),
        };
        let xml = info.to_xml();
        assert!(xml.starts_with("<OMTInfo "));
        assert_eq!(SenderInfo::from_xml(&xml), Some(info));
    }

    #[test]
    fn sender_info_parses_libomts_indented_form() {
        // XmlTextWriter output, with single-quoted and missing attributes.
        let xml = "<OMTInfo ProductName='vMix' Manufacturer=\"StudioCoast\" />";
        let info = SenderInfo::from_xml(xml).unwrap();
        assert_eq!(
            (
                info.product_name.as_str(),
                info.manufacturer.as_str(),
                info.version.as_str()
            ),
            ("vMix", "StudioCoast", "")
        );
        assert_eq!(SenderInfo::from_xml("<OMTTally />"), None);
    }
}
