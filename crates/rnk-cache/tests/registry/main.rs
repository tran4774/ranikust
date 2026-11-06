//! The registry cache against `rnk-registry`'s in-process fake registry
//! (T0): a push then a lookup is a hit, and anything else under a tag, or
//! a registry that fails, is a miss that never fails the build
//! (ADR-0113). Spans are checked in the `spans` test binary.

#[path = "../../../rnk-registry/tests/registry/fake.rs"]
mod fake;
mod support;

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use fake::{Auth, FakeRegistry, Fault};
use hyper::Method;
use rnk_cache::{
    ENCODING_ANNOTATION, Encoding, EntryId, Error, KEY_ANNOTATION, LayerCache, Lookup, Miss,
    Pushed, Refusal, RegistryCache, Source, TimePolicy, VERSION_ANNOTATION,
};
use rnk_oci::{Compression, MediaType, Reference, Timestamp};
use rnk_registry::{Credentials, PushOutcome};
use serde_json::{Value, json};
use support::{
    GZIP, REPO, WEEK, cache, client, entry, hit, key, layer, options, repository, store,
    store_manifest, stored,
};

/// The warning of a failed lookup, or a panic naming what it found.
#[track_caller]
fn failed(lookup: Lookup) -> Error {
    match lookup {
        Lookup::Miss(Miss::Failed(e)) => e,
        other => panic!("not a failed lookup: {other:?}"),
    }
}

#[track_caller]
fn refusal(lookup: Lookup) -> Refusal {
    match failed(lookup) {
        Error::Refused { refusal, .. } => refusal,
        other => panic!("not a refusal: {other}"),
    }
}

#[track_caller]
fn registry_error(lookup: Lookup) -> rnk_registry::Error {
    match failed(lookup) {
        Error::Registry { source, .. } => *source,
        other => panic!("not a registry error: {other}"),
    }
}

#[track_caller]
fn done(pushed: Pushed) -> PushOutcome {
    match pushed {
        Pushed::Done(outcome) => outcome,
        Pushed::Failed(e) => panic!("the push failed: {e}"),
    }
}

/// Requests the registry saw other than the `/v2/` probe.
fn requests(registry: &FakeRegistry) -> Vec<String> {
    registry
        .state()
        .log
        .iter()
        .filter(|l| !l.starts_with("GET /v2/?"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_push_then_a_lookup_is_a_hit_with_the_same_digests() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let entry = entry("RUN make");
    let built = layer(b"layer bytes");
    let before = SystemTime::now();
    assert_eq!(
        done(cache.push(&entry, &built).await),
        PushOutcome::Uploaded
    );

    let found = hit(cache.lookup(&entry).await);
    assert_eq!(found.blob.digest, built.blob.digest);
    assert_eq!(found.blob.size, built.blob.size);
    assert_eq!(found.blob.media_type, MediaType::OCI_LAYER_GZIP);
    assert_eq!(found.diff_id, built.diff_id);
    let tagged = repository(&registry).with_tag(&entry.tag()).unwrap();
    assert_eq!(found.source, Source::Registry(tagged));

    // The entry's form (ADR-0113): one OCI layer, annotations naming the
    // key, its version and the encoding, and `created` the push time.
    let (manifest, config) = stored(&registry, &entry.tag());
    assert_eq!(manifest["mediaType"], MediaType::OCI_MANIFEST.as_str());
    assert_eq!(
        manifest["config"]["mediaType"],
        MediaType::OCI_CONFIG.as_str()
    );
    assert_eq!(manifest["layers"].as_array().unwrap().len(), 1);
    let annotations = &manifest["annotations"];
    let hex = rnk_oci::Digest::from_sha256_bytes(entry.key().hash());
    assert_eq!(annotations[KEY_ANNOTATION], hex.encoded());
    assert_eq!(annotations[VERSION_ANNOTATION], "1");
    assert_eq!(annotations[ENCODING_ANNOTATION], "gzip;level=6;times=kept");
    assert_eq!(
        config["rootfs"]["diff_ids"],
        json!([built.diff_id.as_str()])
    );
    let created = Timestamp::parse_rfc3339(config["created"].as_str().unwrap()).unwrap();
    assert!(created.to_system_time() >= before - Duration::from_secs(1));
    assert!(created.to_system_time() <= SystemTime::now());
}

#[tokio::test]
async fn an_unknown_key_is_a_miss_with_one_request() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let lookup = cache.lookup(&entry("RUN make")).await;
    assert!(matches!(lookup, Lookup::Miss(Miss::Absent)), "{lookup:?}");
    let seen = requests(&registry);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].starts_with(&format!("GET /v2/{REPO}/manifests/rnk-v1-")));
}

#[tokio::test]
async fn a_failing_registry_is_a_miss_with_a_warning() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let entry = entry("RUN make");
    done(cache(&registry, WEEK).push(&entry, &layer(b"l")).await);

    // A 500, after the client's retries.
    registry.state().faults.push(Fault {
        method: Method::GET,
        path_contains: "/manifests/".to_owned(),
        status: 500,
        remaining: 3,
    });
    let e = registry_error(cache(&registry, WEEK).lookup(&entry).await);
    assert!(e.to_string().contains("500"), "{e}");

    // A registry that stops answering, after the `registry-idle` limit.
    registry.state().stalled.push("/manifests/".to_owned());
    let mut quick = options(&registry);
    quick.read_timeout = Some(Duration::from_millis(50));
    let stalled = RegistryCache::new(
        client(quick, Credentials::none()),
        repository(&registry),
        WEEK,
    )
    .unwrap();
    let e = registry_error(stalled.lookup(&entry).await);
    assert!(e.to_string().contains("registry-idle"), "{e}");
}

#[tokio::test]
async fn bad_credentials_are_a_miss_and_a_failed_push_with_a_warning() {
    let registry = FakeRegistry::start(Auth::Basic {
        user: "user".to_owned(),
        password: "right".to_owned(),
    })
    .await;
    let auth = base64::engine::general_purpose::STANDARD.encode("user:wrong");
    let config = format!(
        r#"{{"auths":{{"{}":{{"auth":"{auth}"}}}}}}"#,
        registry.host()
    );
    let credentials = Credentials::from_env_value(config.as_bytes()).unwrap();
    let cache = RegistryCache::new(
        client(options(&registry), credentials),
        repository(&registry),
        WEEK,
    )
    .unwrap();
    let entry = entry("RUN make");
    let e = failed(cache.lookup(&entry).await);
    assert!(matches!(e, Error::Registry { .. }), "{e}");
    assert!(!e.to_string().contains("wrong"), "{e}");
    let Pushed::Failed(e) = cache.push(&entry, &layer(b"l")).await else {
        panic!("a push with bad credentials succeeded");
    };
    assert!(!e.to_string().contains("wrong"), "{e}");
}

#[tokio::test]
async fn an_entry_older_than_the_ttl_is_a_plain_miss() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let entry = entry("RUN make");
    let tag = entry.tag();
    done(cache(&registry, WEEK).push(&entry, &layer(b"l")).await);
    // Young enough: a hit.
    hit(cache(&registry, Duration::from_secs(60))
        .lookup(&entry)
        .await);

    let (manifest, mut config) = stored(&registry, &tag);
    config["created"] = json!("2000-01-01T00:00:00Z");
    store(&registry, &tag, manifest.clone(), &config);
    let lookup = cache(&registry, WEEK).lookup(&entry).await;
    assert!(matches!(lookup, Lookup::Miss(Miss::Expired)), "{lookup:?}");

    // A `created` ahead of the clock never expires, as in kaniko.
    config["created"] = json!("9999-01-01T00:00:00Z");
    store(&registry, &tag, manifest, &config);
    hit(cache(&registry, Duration::from_secs(1))
        .lookup(&entry)
        .await);
}

#[tokio::test]
async fn an_entry_under_another_encoding_is_a_miss() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let gzip = entry("RUN make");
    done(cache.push(&gzip, &layer(b"l")).await);
    let others = [
        Encoding { level: 9, ..GZIP },
        Encoding {
            compression: Compression::Zstd,
            level: 3,
            ..GZIP
        },
        Encoding {
            times: TimePolicy::Clamped { epoch: 0 },
            ..GZIP
        },
    ];
    let (manifest, _) = stored(&registry, &gzip.tag());
    let bytes = serde_json::to_vec(&manifest).unwrap();
    for encoding in others {
        let other = EntryId::new(*gzip.key(), encoding);
        let lookup = cache.lookup(&other).await;
        assert!(matches!(lookup, Lookup::Miss(Miss::Absent)), "{lookup:?}");
        // The gzip entry copied under the other encoding's tag names the
        // gzip encoding: refused, never a layer of another encoding.
        store_manifest(&registry, &other.tag(), &bytes);
        assert_eq!(
            refusal(cache.lookup(&other).await),
            Refusal::Annotation {
                name: ENCODING_ANNOTATION
            },
            "{encoding}"
        );
    }
}

#[tokio::test]
async fn an_entry_not_of_this_form_is_refused_as_a_miss() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let entry = entry("RUN make");
    let tag = entry.tag();
    done(cache.push(&entry, &layer(b"l")).await);
    let (manifest, config) = stored(&registry, &tag);
    let other_key = rnk_oci::Digest::from_sha256_bytes(key("RUN other").hash());
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut m = manifest.clone();
        f(&mut m);
        m
    };
    let annotation = |name: &str, value: Value| {
        edit(&|m| {
            if value.is_null() {
                m["annotations"].as_object_mut().unwrap().remove(name);
            } else {
                m["annotations"][name] = value.clone();
            }
        })
    };
    let layer0 = manifest["layers"][0].clone();
    let cases: Vec<(&str, Value, Refusal)> = vec![
        (
            "two layers",
            edit(&|m| m["layers"] = json!([layer0, layer0])),
            Refusal::Layers { count: 2 },
        ),
        (
            "no layer",
            edit(&|m| m["layers"] = json!([])),
            Refusal::Layers { count: 0 },
        ),
        (
            "a zstd layer under a gzip entry",
            edit(&|m| m["layers"][0]["mediaType"] = json!(MediaType::OCI_LAYER_ZSTD.as_str())),
            Refusal::LayerType {
                media_type: MediaType::OCI_LAYER_ZSTD.as_str().to_owned(),
            },
        ),
        (
            "a foreign layer",
            edit(&|m| {
                m["layers"][0]["mediaType"] = json!(MediaType::OCI_FOREIGN_LAYER_GZIP.as_str());
            }),
            Refusal::LayerType {
                media_type: MediaType::OCI_FOREIGN_LAYER_GZIP.as_str().to_owned(),
            },
        ),
        (
            "a Docker config",
            edit(&|m| m["config"]["mediaType"] = json!(MediaType::DOCKER_CONFIG.as_str())),
            Refusal::ConfigType {
                media_type: MediaType::DOCKER_CONFIG.as_str().to_owned(),
            },
        ),
        (
            "an artifact",
            edit(&|m| m["artifactType"] = json!("application/x-anything")),
            Refusal::NotAnImage,
        ),
        (
            "another key",
            annotation(KEY_ANNOTATION, json!(other_key.encoded())),
            Refusal::Annotation {
                name: KEY_ANNOTATION,
            },
        ),
        (
            "another version",
            annotation(VERSION_ANNOTATION, json!("2")),
            Refusal::Annotation {
                name: VERSION_ANNOTATION,
            },
        ),
        (
            "another encoding",
            annotation(ENCODING_ANNOTATION, json!("gzip;level=9;times=kept")),
            Refusal::Annotation {
                name: ENCODING_ANNOTATION,
            },
        ),
        (
            "no key annotation",
            annotation(KEY_ANNOTATION, Value::Null),
            Refusal::Annotation {
                name: KEY_ANNOTATION,
            },
        ),
        (
            "no annotations",
            edit(&|m| {
                m.as_object_mut().unwrap().remove("annotations");
            }),
            Refusal::Annotation {
                name: KEY_ANNOTATION,
            },
        ),
    ];
    for (what, manifest, want) in cases {
        store(&registry, &tag, manifest, &config);
        assert_eq!(refusal(cache.lookup(&entry).await), want, "{what}");
    }

    let config_cases: Vec<(&str, Value, Refusal)> = vec![
        (
            "two diff IDs",
            {
                let mut c = config.clone();
                let id = c["rootfs"]["diff_ids"][0].clone();
                c["rootfs"]["diff_ids"] = json!([id, id]);
                c
            },
            Refusal::DiffIds,
        ),
        (
            "no created",
            {
                let mut c = config.clone();
                c.as_object_mut().unwrap().remove("created");
                c
            },
            Refusal::Created {
                reason: "missing".to_owned(),
            },
        ),
    ];
    for (what, config, want) in config_cases {
        store(&registry, &tag, manifest.clone(), &config);
        assert_eq!(refusal(cache.lookup(&entry).await), want, "{what}");
    }
    // Go's `time.Time.String()` form, and one too long to quote whole:
    // each refusal keeps the parser's reason.
    for created in [
        "2026-11-07 03:25:10.123456789 +0700 +07".to_owned(),
        format!("2026-11-07T03:25:10Z{}", " ".repeat(10_000)),
    ] {
        let mut bad = config.clone();
        bad["created"] = json!(created);
        store(&registry, &tag, manifest.clone(), &bad);
        let Refusal::Created { reason } = refusal(cache.lookup(&entry).await) else {
            panic!("a created that is not RFC 3339 was not refused as such");
        };
        assert!(reason.starts_with("invalid timestamp"), "{reason}");
        assert!(reason.len() < 200, "{reason}");
        assert!(!reason.ends_with('…'), "the reason was cut: {reason}");
    }
    let mut not_json = config.clone();
    not_json["rootfs"] = json!("layers");
    store(&registry, &tag, manifest.clone(), &not_json);
    assert!(matches!(
        refusal(cache.lookup(&entry).await),
        Refusal::Malformed {
            document: "config",
            ..
        }
    ));

    // An index under the tag, and a Docker manifest: not an OCI image.
    store(&registry, &tag, manifest.clone(), &config);
    let (manifest_bytes, digest) = {
        let state = registry.state();
        let (_, bytes) = &state.manifests[&(REPO.to_owned(), tag.clone())];
        (bytes.clone(), fake::sha256(bytes))
    };
    let index = json!({
        "schemaVersion": 2,
        "mediaType": MediaType::OCI_INDEX.as_str(),
        "manifests": [{
            "mediaType": MediaType::OCI_MANIFEST.as_str(),
            "digest": digest,
            "size": manifest_bytes.len(),
        }],
        "annotations": manifest["annotations"].clone(),
    });
    store_manifest(&registry, &tag, &serde_json::to_vec(&index).unwrap());
    assert_eq!(
        refusal(cache.lookup(&entry).await),
        Refusal::NotAnImageManifest {
            media_type: MediaType::OCI_INDEX.as_str().to_owned()
        }
    );
    let docker = edit(&|m| m["mediaType"] = json!(MediaType::DOCKER_MANIFEST.as_str()));
    store(&registry, &tag, docker, &config);
    assert_eq!(
        refusal(cache.lookup(&entry).await),
        Refusal::NotAnImageManifest {
            media_type: MediaType::DOCKER_MANIFEST.as_str().to_owned()
        }
    );
    let untyped = edit(&|m| {
        m.as_object_mut().unwrap().remove("mediaType");
    });
    store(&registry, &tag, untyped, &config);
    let untyped = refusal(cache.lookup(&entry).await);
    assert_eq!(
        untyped,
        Refusal::NotAnImageManifest {
            media_type: String::new()
        }
    );
    assert!(untyped.to_string().contains("no media type"), "{untyped}");
    // Not a manifest at all: the client refuses it.
    store_manifest(&registry, &tag, b"not json");
    assert!(matches!(
        registry_error(cache.lookup(&entry).await),
        rnk_registry::Error::Manifest { .. }
    ));

    // The control: the entry as pushed is still a hit.
    store(&registry, &tag, manifest, &config);
    hit(cache.lookup(&entry).await);
}

#[tokio::test]
async fn a_hit_carries_only_the_layers_type_digest_and_size() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let entry = entry("RUN make");
    let built = layer(b"l");
    done(cache.push(&entry, &built).await);
    let (mut manifest, config) = stored(&registry, &entry.tag());
    let blob = &mut manifest["layers"][0];
    blob["urls"] = json!(["https://elsewhere.example/blob"]);
    blob["data"] = json!("bA==");
    blob["annotations"] = json!({"org.opencontainers.image.title": "x"});
    blob["platform"] = json!({"architecture": "arm64", "os": "linux"});
    store(&registry, &entry.tag(), manifest, &config);
    let found = hit(cache.lookup(&entry).await);
    assert_eq!(
        found.blob,
        rnk_oci::Descriptor::new(
            MediaType::OCI_LAYER_GZIP,
            built.blob.digest,
            built.blob.size
        )
    );
}

#[tokio::test]
async fn documents_over_the_limits_are_a_miss_with_a_warning() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let entry = entry("RUN make");
    let tag = entry.tag();
    done(cache(&registry, WEEK).push(&entry, &layer(b"l")).await);
    let limited = |manifest: u64, config: u64| {
        let mut options = options(&registry);
        options.max_manifest_bytes = manifest;
        options.max_config_bytes = config;
        RegistryCache::new(
            client(options, Credentials::none()),
            repository(&registry),
            WEEK,
        )
        .unwrap()
    };
    let (mut manifest, config) = stored(&registry, &tag);
    manifest["annotations"]["padding"] = json!("x".repeat(64 << 10));
    store(&registry, &tag, manifest, &config);

    let e = registry_error(limited(32 << 10, 1 << 20).lookup(&entry).await);
    assert!(
        matches!(
            e,
            rnk_registry::Error::TooLarge {
                limit: "manifest",
                ..
            }
        ),
        "{e}"
    );
    let e = registry_error(limited(1 << 20, 16).lookup(&entry).await);
    assert!(
        matches!(
            e,
            rnk_registry::Error::TooLarge {
                limit: "config",
                ..
            }
        ),
        "{e}"
    );
    // The control: within both limits, a hit.
    hit(limited(1 << 20, 1 << 20).lookup(&entry).await);
}

#[tokio::test]
async fn a_push_skips_or_mounts_a_blob_the_registry_holds() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let built = layer(&b"shared layer ".repeat(100));
    let uploads = || registry.count(&format!("PUT /v2/{REPO}/blobs/uploads/"));

    assert_eq!(
        done(cache.push(&entry("RUN one"), &built).await),
        PushOutcome::Uploaded
    );
    // The layer and the config.
    assert_eq!(uploads(), 2);
    assert_eq!(
        done(cache.push(&entry("RUN two"), &built).await),
        PushOutcome::Existed
    );
    // The second entry's config only.
    assert_eq!(uploads(), 3);

    // Held by a destination of the same registry: mounted, not uploaded.
    let mut elsewhere = layer(b"pushed to the destination");
    let blob = registry.add_blob("app", b"pushed to the destination");
    assert_eq!(blob, elsewhere.blob.digest.as_str());
    let app = Reference::parse(&format!("{}/app", registry.host())).unwrap();
    elsewhere.mount_from = Some(app);
    assert_eq!(
        done(cache.push(&entry("RUN three"), &elsewhere).await),
        PushOutcome::Mounted
    );
    assert_eq!(uploads(), 4);
    hit(cache.lookup(&entry("RUN three")).await);
}

#[tokio::test]
async fn a_failed_push_is_a_warning_and_publishes_no_entry() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = cache(&registry, WEEK);
    let entry = entry("RUN make");
    registry.state().faults.push(Fault {
        method: Method::PUT,
        path_contains: "/manifests/".to_owned(),
        status: 500,
        remaining: 3,
    });
    let Pushed::Failed(e) = cache.push(&entry, &layer(b"l")).await else {
        panic!("the push succeeded");
    };
    assert!(matches!(e, Error::Registry { .. }), "{e}");
    let lookup = cache.lookup(&entry).await;
    assert!(matches!(lookup, Lookup::Miss(Miss::Absent)), "{lookup:?}");

    // A layer of another encoding than its entry's is never sent.
    let before = requests(&registry).len();
    let mut zstd = layer(b"z");
    zstd.blob.media_type = MediaType::OCI_LAYER_ZSTD;
    let Pushed::Failed(e) = cache.push(&entry, &zstd).await else {
        panic!("a zstd layer was pushed under a gzip entry");
    };
    assert!(matches!(e, Error::Encoding { .. }), "{e}");
    assert_eq!(requests(&registry).len(), before);

    // A Docker gzip layer is of the entry's compression: named as OCI.
    let mut docker = layer(b"d");
    docker.blob.media_type = MediaType::DOCKER_LAYER_GZIP;
    done(cache.push(&entry, &docker).await);
    let found = hit(cache.lookup(&entry).await);
    assert_eq!(found.blob.media_type, MediaType::OCI_LAYER_GZIP);
}

#[tokio::test]
async fn a_cache_repository_has_no_tag_or_digest() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let client = client(options(&registry), Credentials::none());
    let tagged = repository(&registry).with_tag("latest").unwrap();
    let e = RegistryCache::new(client.clone(), tagged, WEEK).unwrap_err();
    assert!(matches!(e, Error::Repository { .. }), "{e}");
    let digest = repository(&registry).with_digest(rnk_oci::Digest::from_sha256_bytes(&[1; 32]));
    assert!(RegistryCache::new(client, digest, WEEK).is_err());
}
