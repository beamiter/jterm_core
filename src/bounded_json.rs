//! Budget primitives for allocation-bounded `serde_json` decoding.
//!
//! Persisted UI snapshots are terminal-controlled data: the input file has its
//! own byte cap, but derived deserialization can still turn a compact JSON
//! array into thousands of owned elements before the caller's `sanitize` pass
//! runs. The jterm frontends therefore decode through `DeserializeSeed`
//! visitors that borrow every nested payload as a [`RawValue`] slice and only
//! descend under an explicit budget. This module holds the two
//! schema-independent pieces of that pattern: the cumulative text budget, the
//! deferred borrowed map field, and a recursive duplicate-member preflight for
//! trust boundaries that still decode into ordinary Serde types.

use serde::de::{Error as DeError, IgnoredAny, MapAccess};
use serde_json::value::RawValue;

/// Validate one complete JSON value and reject duplicate object members at
/// every depth without retaining a decoded [`serde_json::Value`] tree. This is
/// re-exported from jagent so Agent wire decoding and the family's credential,
/// IPC, and persistence boundaries cannot drift onto different uniqueness
/// semantics.
pub use jagent::validate_no_duplicate_members;

/// A cumulative byte budget charged as decoded text fields are retained.
///
/// The frontends decode one snapshot under one budget, so an oversized field
/// early in the file cannot be repeated under sibling subtrees to multiply
/// the retained total.
#[derive(Clone, Copy)]
pub struct TextBudget {
    remaining: usize,
}

impl TextBudget {
    pub fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    pub fn remaining(&self) -> usize {
        self.remaining
    }

    /// Charge `bytes` retained for `field`; fails the decode once the
    /// cumulative budget is exceeded.
    pub fn charge<E: DeError>(&mut self, field: &'static str, bytes: usize) -> Result<(), E> {
        let Some(remaining) = self.remaining.checked_sub(bytes) else {
            return Err(E::custom(format_args!(
                "snapshot exceeds its cumulative text budget while decoding '{field}'"
            )));
        };
        self.remaining = remaining;
        Ok(())
    }
}

/// A map field captured as a borrowed raw slice instead of an owned value.
///
/// Optional or out-of-order fields whose schema position is not yet known are
/// deferred this way so a deeply nested payload is not cloned once per
/// ancestor, and so the decoder can decline to descend into data the resolved
/// variant does not need. The duplicate bit lets an explicit `null` still
/// count as a present field while a repeated key fails as a duplicate.
#[derive(Default)]
pub struct DeferredRawField<'de> {
    value: Option<&'de RawValue>,
    duplicate: bool,
}

impl<'de> DeferredRawField<'de> {
    /// Read the next value of a map into this field, ignoring any repeat.
    pub fn read<A: MapAccess<'de>>(&mut self, map: &mut A) -> Result<(), A::Error> {
        if self.value.is_some() {
            self.duplicate = true;
            map.next_value::<IgnoredAny>()?;
        } else {
            self.value = Some(map.next_value::<&'de RawValue>()?);
        }
        Ok(())
    }

    /// Resolve a field that must be present exactly once.
    pub fn required<E: DeError>(self, field: &'static str) -> Result<&'de RawValue, E> {
        if self.duplicate {
            return Err(E::duplicate_field(field));
        }
        self.value.ok_or_else(|| E::missing_field(field))
    }

    /// Resolve a field that may be absent.
    pub fn optional<E: DeError>(self, field: &'static str) -> Result<Option<&'de RawValue>, E> {
        if self.duplicate {
            return Err(E::duplicate_field(field));
        }
        Ok(self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::Visitor;
    use serde::Deserializer as _;

    #[test]
    fn duplicate_member_preflight_is_recursive_and_does_not_echo_names() {
        for valid in [
            br#"null"#.as_slice(),
            br#"[true,7,"text",{"nested":[1,2,3]}]"#.as_slice(),
            br#"{"credential":{"token":"one"},"id":1}"#.as_slice(),
        ] {
            validate_no_duplicate_members(valid).unwrap();
        }

        for invalid in [
            br#"{"token":"first","token":"second"}"#.as_slice(),
            br#"{"credential":{"token":"first","token":"second"}}"#.as_slice(),
            br#"{"name":"first","\u006eame":"second"}"#.as_slice(),
        ] {
            let error = validate_no_duplicate_members(invalid)
                .unwrap_err()
                .to_string();
            assert!(error.contains("duplicate JSON object member"), "{error}");
            assert!(!error.contains("token"), "{error}");
            assert!(!error.contains("name"), "{error}");
        }
    }

    #[test]
    fn duplicate_member_preflight_requires_one_complete_json_value() {
        for invalid in [
            br#"{"ok":true} trailing"#.as_slice(),
            br#"{"ok":true"#.as_slice(),
        ] {
            assert!(validate_no_duplicate_members(invalid).is_err());
        }
    }

    #[test]
    fn text_budget_charges_cumulatively_and_fails_closed() {
        let mut budget = TextBudget::new(10);
        budget.charge::<serde::de::value::Error>("a", 4).unwrap();
        budget.charge::<serde::de::value::Error>("b", 6).unwrap();
        assert_eq!(budget.remaining(), 0);
        let error = budget
            .charge::<serde::de::value::Error>("c", 1)
            .unwrap_err();
        assert!(error.to_string().contains("cumulative text budget"));
        assert!(error.to_string().contains("'c'"));
    }

    struct MapProbeVisitor;

    impl<'de> Visitor<'de> for MapProbeVisitor {
        type Value = (Option<&'de RawValue>, Option<&'de RawValue>, bool, bool);

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a probe map")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut first = DeferredRawField::default();
            let mut second = DeferredRawField::default();
            while let Some(key) = map.next_key::<&str>()? {
                match key {
                    "first" => first.read(&mut map)?,
                    _ => second.read(&mut map)?,
                }
            }
            let first_missing = first.value.is_none() && !first.duplicate;
            let second_duplicate = second.duplicate;
            Ok((first.value, second.value, first_missing, second_duplicate))
        }
    }

    fn probe(input: &str) -> (Option<String>, Option<String>, bool, bool) {
        let mut deserializer = serde_json::Deserializer::from_str(input);
        let (first, second, first_missing, second_duplicate) =
            deserializer.deserialize_map(MapProbeVisitor).unwrap();
        (
            first.map(|raw| raw.get().to_string()),
            second.map(|raw| raw.get().to_string()),
            first_missing,
            second_duplicate,
        )
    }

    #[test]
    fn deferred_fields_stay_borrowed_and_track_duplicates() {
        let (first, second, first_missing, second_duplicate) =
            probe(r#"{"first": {"nested": [1, 2]}, "second": null}"#);
        assert_eq!(first.as_deref(), Some(r#"{"nested": [1, 2]}"#));
        assert_eq!(second.as_deref(), Some("null"));
        assert!(!first_missing);
        assert!(!second_duplicate);

        let (first, _, first_missing, _) = probe(r#"{"second": 1}"#);
        assert!(first.is_none());
        assert!(first_missing);

        let (_, _, _, second_duplicate) = probe(r#"{"first": 1, "second": 1, "second": 2}"#);
        assert!(second_duplicate);
    }

    #[test]
    fn duplicate_deferred_fields_fail_resolution() {
        let boxed: Box<RawValue> = serde_json::from_str("1").unwrap();
        let raw: &RawValue = &boxed;
        let duplicated = DeferredRawField {
            value: Some(raw),
            duplicate: true,
        };
        let error = duplicated
            .required::<serde::de::value::Error>("first")
            .unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
        let duplicated = DeferredRawField {
            value: Some(raw),
            duplicate: true,
        };
        let error = duplicated
            .optional::<serde::de::value::Error>("first")
            .unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
    }

    #[test]
    fn required_reports_missing_and_optional_allows_absent() {
        let absent = DeferredRawField::default();
        assert!(absent
            .optional::<serde::de::value::Error>("x")
            .unwrap()
            .is_none());
        let absent = DeferredRawField::default();
        let error = absent.required::<serde::de::value::Error>("x").unwrap_err();
        assert!(error.to_string().contains("missing"), "{error}");
    }
}
