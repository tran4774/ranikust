//! Canonical JSON: the exact bytes of every config, manifest and index
//! ranikust writes, and so of their digests.
//!
//! The OCI image-spec v1.1.1 asks for canonical JSON
//! ([considerations.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/considerations.md#json),
//! [OLPC canonical JSON](https://wiki.laptop.org/go/Canonical_JSON)). The form
//! written here:
//!
//! - every object's keys sorted by their UTF-8 bytes (code point order),
//!   struct fields included, so the bytes depend neither on Rust declaration
//!   order nor on `HashMap` iteration order;
//! - no whitespace outside strings;
//! - integers only: a floating-point number is an error;
//! - strings escaped as `serde_json` escapes them: `"`, `\` and control
//!   characters, everything else as raw UTF-8. OLPC leaves control
//!   characters raw, which is not valid JSON; Go's `encoding/json` also
//!   escapes `<`, `>` and `&`, so bytes differ from kaniko's when a string
//!   holds one of those;
//! - no Unicode normalization (as in `docker/go`, the implementation the
//!   spec names).

use serde::Serialize;
use serde_json::Value;

use crate::error::Error;

/// Serializes `value` as canonical JSON.
///
/// The same value always gives the same bytes, whatever the order its maps
/// were filled in.
pub fn to_canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let tree = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write_value(&tree, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), Error> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => {
            if number.is_f64() {
                return Err(Error::NotCanonical(
                    "floating-point numbers have no canonical form",
                ));
            }
            serde_json::to_writer(&mut *out, number)?;
        }
        Value::String(string) => serde_json::to_writer(&mut *out, string)?,
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // Sorted here rather than trusting the map's own order, which
            // depends on serde_json's `preserve_order` feature.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            out.push(b'{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_value(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fmt;

    use proptest::prelude::*;
    use proptest::test_runner::Config as ProptestConfig;
    use serde::Deserialize;
    use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

    use super::*;
    use crate::{
        ContainerConfig, Descriptor, Digest, History, ImageConfig, ImageFormat, ImageIndex,
        ImageManifest, MediaType, RootFs,
    };

    const FIXTURES: [&str; 7] = [
        include_str!("../testdata/docker-manifest.json"),
        include_str!("../testdata/docker-manifest-list.json"),
        include_str!("../testdata/docker-config.json"),
        include_str!("../testdata/oci-manifest.json"),
        include_str!("../testdata/oci-artifact-manifest.json"),
        include_str!("../testdata/oci-index.json"),
        include_str!("../testdata/oci-config.json"),
    ];

    fn canonical_str<T: Serialize + ?Sized>(value: &T) -> String {
        String::from_utf8(to_canonical_json(value).unwrap()).unwrap()
    }

    /// Deserializing into this checks, in document order, that every
    /// object's keys are strictly increasing.
    struct SortedKeys;

    impl<'de> Deserialize<'de> for SortedKeys {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(SortedKeysVisitor)
        }
    }

    struct SortedKeysVisitor;

    impl<'de> Visitor<'de> for SortedKeysVisitor {
        type Value = SortedKeys;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("any JSON value")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SortedKeys, A::Error> {
            let mut previous: Option<String> = None;
            while let Some(key) = map.next_key::<String>()? {
                if previous.as_ref().is_some_and(|p| *p >= key) {
                    return Err(de::Error::custom(format!("{key:?} after {previous:?}")));
                }
                map.next_value::<SortedKeys>()?;
                previous = Some(key);
            }
            Ok(SortedKeys)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<SortedKeys, A::Error> {
            while seq.next_element::<SortedKeys>()?.is_some() {}
            Ok(SortedKeys)
        }

        fn visit_bool<E>(self, _: bool) -> Result<SortedKeys, E> {
            Ok(SortedKeys)
        }
        fn visit_i64<E>(self, _: i64) -> Result<SortedKeys, E> {
            Ok(SortedKeys)
        }
        fn visit_u64<E>(self, _: u64) -> Result<SortedKeys, E> {
            Ok(SortedKeys)
        }
        fn visit_str<E>(self, _: &str) -> Result<SortedKeys, E> {
            Ok(SortedKeys)
        }
        fn visit_unit<E>(self) -> Result<SortedKeys, E> {
            Ok(SortedKeys)
        }
    }

    fn assert_sorted_keys(bytes: &[u8]) {
        if let Err(err) = serde_json::from_slice::<SortedKeys>(bytes) {
            panic!("{err}: {}", String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn sorted_keys_checker_rejects_unsorted_and_duplicate_keys() {
        assert!(
            serde_json::from_slice::<SortedKeys>(br#"{"a":{"x":[{"m":1,"n":2}]},"b":1}"#).is_ok()
        );
        for bad in [
            &br#"{"b":1,"a":2}"#[..],
            br#"{"a":1,"a":2}"#,
            br#"{"a":[{"n":1,"m":2}]}"#,
        ] {
            assert!(serde_json::from_slice::<SortedKeys>(bad).is_err());
        }
    }

    fn sample_config() -> ImageConfig {
        let mut labels = BTreeMap::new();
        labels.insert("b".to_owned(), "2".to_owned());
        labels.insert("a".to_owned(), "<&>".to_owned());
        ImageConfig {
            created: Some("1970-01-01T00:00:00Z".to_owned()),
            architecture: "amd64".to_owned(),
            os: "linux".to_owned(),
            config: Some(ContainerConfig {
                env: Some(vec!["PATH=/bin".to_owned()]),
                cmd: Some(vec!["sh".to_owned()]),
                exposed_ports: Some(BTreeSet::from(["80/tcp".to_owned(), "443/tcp".to_owned()])),
                labels: Some(labels),
                ..ContainerConfig::default()
            }),
            rootfs: RootFs {
                diff_ids: vec![Digest::from_sha256_bytes(&[0xab; 32])],
                ..RootFs::default()
            },
            history: Some(vec![History {
                created_by: Some("RUN \"quoted\"\n".to_owned()),
                empty_layer: Some(true),
                ..History::default()
            }]),
            ..ImageConfig::default()
        }
    }

    #[test]
    fn golden_config_bytes() {
        let diff_id = format!("sha256:{}", "ab".repeat(32));
        let expected = format!(
            concat!(
                r#"{{"architecture":"amd64","config":{{"Cmd":["sh"],"Env":["PATH=/bin"],"#,
                r#""ExposedPorts":{{"443/tcp":{{}},"80/tcp":{{}}}},"Labels":{{"a":"<&>","b":"2"}}}},"#,
                r#""created":"1970-01-01T00:00:00Z","history":[{{"created_by":"RUN \"quoted\"\n","#,
                r#""empty_layer":true}}],"os":"linux","rootfs":{{"diff_ids":["{}"],"type":"layers"}}}}"#,
            ),
            diff_id
        );
        assert_eq!(canonical_str(&sample_config()), expected);
    }

    #[test]
    fn golden_manifest_bytes() {
        let config = Descriptor::new(
            MediaType::OCI_CONFIG,
            Digest::from_sha256_bytes(&[1; 32]),
            2,
        );
        let m = ImageManifest::new(ImageFormat::Oci, config, Vec::new());
        let expected = format!(
            concat!(
                r#"{{"config":{{"digest":"sha256:{}","mediaType":"application/vnd.oci.image.config.v1+json","size":2}},"#,
                r#""layers":[],"mediaType":"application/vnd.oci.image.manifest.v1+json","schemaVersion":2}}"#,
            ),
            "01".repeat(32)
        );
        assert_eq!(canonical_str(&m), expected);
    }

    #[test]
    fn canonical_fixture_is_reproduced_byte_for_byte() {
        let text = include_str!("../testdata/oci-manifest.json").trim_end();
        let m = ImageManifest::from_json(text.as_bytes()).unwrap();
        assert_eq!(canonical_str(&m), text);
    }

    #[test]
    fn fixtures_have_a_stable_canonical_form() {
        for text in FIXTURES {
            let value: Value = serde_json::from_str(text).unwrap();
            let typed = if value.get("manifests").is_some() {
                to_canonical_json(&ImageIndex::from_json(text.as_bytes()).unwrap())
            } else if value.get("layers").is_some() {
                to_canonical_json(&ImageManifest::from_json(text.as_bytes()).unwrap())
            } else {
                to_canonical_json(&ImageConfig::from_json(text.as_bytes()).unwrap())
            }
            .unwrap();
            assert_sorted_keys(&typed);
            // A fixed point: canonical bytes parse and canonicalize to
            // themselves.
            let again: Value = serde_json::from_slice(&typed).unwrap();
            assert_eq!(to_canonical_json(&again).unwrap(), typed);
            assert!(!typed.contains(&b'\n'));
        }
    }

    #[test]
    fn hash_map_order_never_shows_over_100_runs() {
        let entries: Vec<(String, String)> = (0..64)
            .map(|i| (format!("org.example.label-{i}"), format!("value {i}")))
            .collect();
        let first = to_canonical_json(&entries.iter().cloned().collect::<HashMap<_, _>>()).unwrap();
        assert_sorted_keys(&first);
        for _ in 0..100 {
            // A fresh HashMap has a fresh random hasher, so a new order.
            let map: HashMap<String, String> = entries.iter().rev().cloned().collect();
            assert_eq!(to_canonical_json(&map).unwrap(), first);
        }
    }

    #[test]
    fn keys_sort_by_utf8_bytes() {
        let map: HashMap<&str, u8> =
            [("é", 1), ("a", 2), ("B", 3), ("", 4), ("aa", 5), ("Z", 6)].into();
        assert_eq!(
            canonical_str(&map),
            r#"{"":4,"B":3,"Z":6,"a":2,"aa":5,"é":1}"#
        );
    }

    #[test]
    fn strings_escape_only_what_json_requires() {
        let s = "tab\t nl\n quote\" back\\ nul\u{0} del\u{7f} <&> é \u{2028}";
        assert_eq!(
            canonical_str(s),
            "\"tab\\t nl\\n quote\\\" back\\\\ nul\\u0000 del\u{7f} <&> é \u{2028}\""
        );
    }

    #[test]
    fn scalars_and_floats() {
        assert_eq!(
            canonical_str(&(u64::MAX, i64::MIN, true, ())),
            format!("[{},{},true,null]", u64::MAX, i64::MIN)
        );
        assert!(matches!(
            to_canonical_json(&1.5f64),
            Err(Error::NotCanonical(_))
        ));
        assert!(matches!(
            to_canonical_json(&[0.0f32]),
            Err(Error::NotCanonical(_))
        ));
        // serde_json cannot write maps with non-string keys.
        let bad: HashMap<Vec<u8>, u8> = [(vec![1], 1)].into();
        assert!(matches!(to_canonical_json(&bad), Err(Error::Json(_))));
    }

    /// Any JSON value without floating-point numbers.
    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::from),
            any::<u64>().prop_map(Value::from),
            any::<String>().prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::btree_map(any::<String>(), inner, 0..8)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    fn label_map() -> impl Strategy<Value = BTreeMap<String, String>> {
        prop::collection::btree_map(any::<String>(), any::<String>(), 0..16)
    }

    proptest! {
        // Never write regression files into the source tree.
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn insertion_order_does_not_change_the_bytes(
            entries in label_map().prop_map(|m| m.into_iter().collect::<Vec<_>>()).prop_shuffle(),
            ports in prop::collection::vec("[0-9]{1,5}/(tcp|udp)", 0..8),
        ) {
            let sorted: BTreeMap<_, _> = entries.iter().cloned().collect();
            let hashed: HashMap<_, _> = entries.iter().cloned().collect();
            prop_assert_eq!(to_canonical_json(&sorted)?, to_canonical_json(&hashed)?);

            // The same through an image config filled in two orders.
            let mut forward = ContainerConfig::default();
            let mut backward = ContainerConfig::default();
            for (k, v) in &entries {
                forward.labels.get_or_insert_default().insert(k.clone(), v.clone());
            }
            for (k, v) in entries.iter().rev() {
                backward.labels.get_or_insert_default().insert(k.clone(), v.clone());
            }
            for p in &ports {
                forward.exposed_ports.get_or_insert_default().insert(p.clone());
            }
            for p in ports.iter().rev() {
                backward.exposed_ports.get_or_insert_default().insert(p.clone());
            }
            prop_assert_eq!(to_canonical_json(&forward)?, to_canonical_json(&backward)?);
        }

        #[test]
        fn any_value_round_trips_sorted_and_idempotent(value in json_value()) {
            let bytes = to_canonical_json(&value)?;
            assert_sorted_keys(&bytes);
            let back: Value = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(&back, &value);
            prop_assert_eq!(to_canonical_json(&back)?, bytes);
        }

        #[test]
        fn configs_round_trip_through_canonical_bytes(
            labels in label_map(),
            env in prop::collection::vec(any::<String>(), 0..8),
            annotations in label_map(),
        ) {
            let mut config = sample_config();
            let run = config.config.get_or_insert_default();
            // Empty ones are written as absent (`omitempty`, Q-132).
            run.labels = (!labels.is_empty()).then_some(labels);
            run.env = (!env.is_empty()).then_some(env);
            let bytes = to_canonical_json(&config)?;
            assert_sorted_keys(&bytes);
            prop_assert_eq!(ImageConfig::from_json(&bytes)?, config);

            let descriptor = Descriptor::new(MediaType::OCI_CONFIG, Digest::from_sha256_bytes(&[7; 32]), 1);
            let mut manifest = ImageManifest::new(ImageFormat::Oci, descriptor, Vec::new());
            manifest.annotations = Some(annotations);
            let bytes = to_canonical_json(&manifest)?;
            assert_sorted_keys(&bytes);
            prop_assert_eq!(ImageManifest::from_json(&bytes)?, manifest);
        }
    }
}
