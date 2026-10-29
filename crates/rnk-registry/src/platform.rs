//! Picking a platform's manifest from an image index.
//!
//! Here rather than in `rnk-oci` because the rule is kaniko's pull
//! behaviour, not a property of the format: the first entry, in index order,
//! whose OS and architecture equal the requested ones, whose OS version and
//! variant equal them when requested, and whose OS features and features
//! include the requested ones, as go-containerregistry's `matchesPlatform`
//! (`kaniko@44aa7ddc0 vendor/github.com/google/go-containerregistry/pkg/v1/remote/index.go:252-291`).
//! An entry without a platform never matches.

use rnk_oci::{Descriptor, ImageIndex, Platform};

/// The descriptor of the manifest for `platform` in `index`, if any.
pub fn select_platform<'a>(index: &'a ImageIndex, platform: &Platform) -> Option<&'a Descriptor> {
    index.manifests.iter().find(|d| {
        d.media_type.is_manifest()
            && d.platform
                .as_ref()
                .is_some_and(|given| matches(given, platform))
    })
}

fn matches(given: &Platform, required: &Platform) -> bool {
    let optional_equal = |required: &Option<String>, given: &Option<String>| {
        required
            .as_deref()
            .is_none_or(|r| r.is_empty() || given.as_deref() == Some(r))
    };
    let subset = |required: &Option<Vec<String>>, given: &Option<Vec<String>>| {
        required
            .iter()
            .flatten()
            .all(|r| given.iter().flatten().any(|g| g == r))
    };
    given.os == required.os
        && given.architecture == required.architecture
        && optional_equal(&required.os_version, &given.os_version)
        && optional_equal(&required.variant, &given.variant)
        && subset(&required.os_features, &given.os_features)
        && subset(&required.features, &given.features)
}

#[cfg(test)]
mod tests {
    use rnk_oci::{Digest, ImageFormat, MediaType};

    use super::*;

    fn entry(n: u8, platform: Option<Platform>) -> Descriptor {
        let mut d = Descriptor::new(
            MediaType::OCI_MANIFEST,
            Digest::from_sha256_bytes(&[n; 32]),
            100,
        );
        d.platform = platform;
        d
    }

    fn variant(os: &str, arch: &str, variant: &str) -> Platform {
        let mut p = Platform::new(os, arch);
        p.variant = Some(variant.to_owned());
        p
    }

    #[test]
    fn first_matching_entry_wins() {
        let index = ImageIndex::new(
            ImageFormat::Oci,
            vec![
                entry(0, None),
                entry(1, Some(Platform::new("linux", "amd64"))),
                entry(2, Some(variant("linux", "arm", "v6"))),
                entry(3, Some(variant("linux", "arm", "v7"))),
                entry(4, Some(variant("linux", "arm64", "v8"))),
            ],
        );
        let pick = |p: &Platform| select_platform(&index, p).map(|d| d.digest.clone());
        assert_eq!(
            pick(&Platform::new("linux", "amd64")),
            Some(index.manifests[1].digest.clone())
        );
        assert_eq!(
            pick(&Platform::new("linux", "arm")),
            Some(index.manifests[2].digest.clone())
        );
        assert_eq!(
            pick(&variant("linux", "arm", "v7")),
            Some(index.manifests[3].digest.clone())
        );
        assert_eq!(
            pick(&Platform::new("linux", "arm64")),
            Some(index.manifests[4].digest.clone())
        );
        assert_eq!(pick(&Platform::new("windows", "amd64")), None);
        assert_eq!(pick(&Platform::new("linux", "s390x")), None);
    }

    #[test]
    fn features_must_be_a_subset() {
        let mut given = Platform::new("windows", "amd64");
        given.os_version = Some("10.0.17763.1".to_owned());
        given.os_features = Some(vec!["win32k".to_owned()]);
        let index = ImageIndex::new(ImageFormat::Oci, vec![entry(1, Some(given))]);

        let mut wanted = Platform::new("windows", "amd64");
        assert!(select_platform(&index, &wanted).is_some());
        wanted.os_features = Some(vec!["win32k".to_owned()]);
        assert!(select_platform(&index, &wanted).is_some());
        wanted.os_features = Some(vec!["other".to_owned()]);
        assert!(select_platform(&index, &wanted).is_none());
        wanted.os_features = None;
        wanted.os_version = Some("10.0.20348.1".to_owned());
        assert!(select_platform(&index, &wanted).is_none());
    }

    #[test]
    fn nested_indexes_and_attestations_are_skipped() {
        let mut nested = entry(1, Some(Platform::new("linux", "amd64")));
        nested.media_type = MediaType::OCI_INDEX;
        let index = ImageIndex::new(
            ImageFormat::Oci,
            vec![nested, entry(2, Some(Platform::new("unknown", "unknown")))],
        );
        assert!(select_platform(&index, &Platform::new("linux", "amd64")).is_none());
    }
}
