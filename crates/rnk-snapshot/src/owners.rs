//! The owners of an image's base layers, read from their tar headers and
//! checked against the user namespace the build runs in before any of them
//! is extracted (ADR-0098).

use std::fs::File;
use std::path::PathBuf;

use rnk_oci::Digest;
use rnk_rootfs::{MappedIds, Unmapped};

use crate::error::{Error, Result};
use crate::layer::chain_above;

/// How the layers are extracted, which tells the entries that get an owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// In the overlay form, devices left out ([`rnk_rootfs::scan_owners`]).
    Overlay,
    /// Into a live root ([`rnk_rootfs::check_headers_and_owners`]).
    Live,
}

/// Refuses the base layers `layers` (diff ID and blob, image order) when an
/// entry of one would get an owner that `ids` does not map, naming every
/// such id found in them all. Each blob is decompressed once, its headers
/// checked as its extraction reads them and its diff ID verified; a layer
/// `kept` tells is already extracted (by chain ID) is not read: its owners
/// were mapped. Nothing is written.
///
/// # Errors
///
/// [`Error::Rootfs`] wrapping [`rnk_rootfs::Error::Unmapped`]; a decoding
/// or header error, [`Error::DiffIdMismatch`], or what `kept` returns.
pub(crate) fn check(
    layers: &[(Digest, PathBuf)],
    ids: &MappedIds,
    form: Form,
    mut kept: impl FnMut(&Digest) -> Result<bool>,
) -> Result<()> {
    let span = tracing::debug_span!(
        "rnk_snapshot.check_owners",
        layers = layers.len(),
        layers_scanned = tracing::field::Empty,
    );
    let _entered = span.enter();
    let mut found = Unmapped::owners(ids.clone());
    let mut chain: Option<Digest> = None;
    let mut scanned = 0_u64;
    for (diff_id, blob) in layers {
        let id = chain_above(chain.as_ref(), diff_id);
        let skip = kept(&id)?;
        chain = Some(id);
        if skip {
            continue;
        }
        let file = File::open(blob).map_err(|source| Error::Io {
            op: "open",
            path: blob.clone(),
            source,
        })?;
        let mut decoder = rnk_layer::Decoder::new(file)?;
        match form {
            Form::Overlay => rnk_rootfs::scan_owners(&mut decoder, &mut found)?,
            Form::Live => rnk_rootfs::check_headers_and_owners(&mut decoder, &mut found)?,
        }
        let actual = Digest::from_sha256_bytes(decoder.finish()?.as_bytes());
        if &actual != diff_id {
            return Err(Error::DiffIdMismatch {
                expected: diff_id.to_string(),
                actual: actual.to_string(),
            });
        }
        scanned += 1;
    }
    span.record("layers_scanned", scanned);
    if found.is_empty() {
        Ok(())
    } else {
        Err(rnk_rootfs::Error::Unmapped(found).into())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;

    use sha2::{Digest as _, Sha256};

    use super::*;

    /// A plain tar of `entries` (name, uid, gid; a name ending in `/` is a
    /// directory), written to `dir/<n>`, and its diff ID.
    fn layer(dir: &std::path::Path, n: usize, entries: &[(&str, u64, u64)]) -> (Digest, PathBuf) {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, uid, gid) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(if name.ends_with('/') {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            h.set_mode(0o755);
            h.set_uid(*uid);
            h.set_gid(*gid);
            h.set_size(0);
            tar.append_data(&mut h, name, std::io::empty()).unwrap();
        }
        let bytes = tar.into_inner().unwrap();
        let path = dir.join(n.to_string());
        fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        (Digest::from_sha256_bytes(&hash), path)
    }

    /// Every layer is read before the refusal, which names the ids of them
    /// all; a layer kept is not read; a blob whose tar is not its diff ID is
    /// refused as such.
    #[test]
    fn the_owners_of_every_layer_not_kept_are_checked_before_any_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let layers = vec![
            layer(dir.path(), 0, &[("etc/", 0, 0), ("etc/shadow", 0, 42)]),
            layer(dir.path(), 1, &[("home/app/", 1000, 1000)]),
        ];
        let one = MappedIds::parse("0 1000 1\n", "0 1000 1\n").unwrap();
        let mut asked = Vec::new();
        let e = check(&layers, &one, Form::Overlay, |chain| {
            asked.push(chain.clone());
            Ok(false)
        })
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "owners this user namespace does not map: uid 1000 (/home/app), gid 42 \
             (/etc/shadow), gid 1000 (/home/app); it maps uid 0 and gid 0"
        );
        assert!(matches!(e, Error::Rootfs(rnk_rootfs::Error::Unmapped(_))));
        // Chain IDs: the bottom layer's is its diff ID.
        assert_eq!(asked[0], layers[0].0);
        assert_eq!(asked[1], chain_above(Some(&layers[0].0), &layers[1].0));

        let e = check(&layers, &one, Form::Live, |chain| Ok(chain != &asked[0])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "owners this user namespace does not map: gid 42 (/etc/shadow); it maps uid 0 \
             and gid 0"
        );
        let ranges =
            MappedIds::parse("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 100000 65536\n").unwrap();
        check(&layers, &ranges, Form::Overlay, |_| Ok(false)).unwrap();
        check(&layers, &one, Form::Overlay, |_| Ok(true)).unwrap();

        let wrong = vec![(layers[1].0.clone(), layers[0].1.clone())];
        let e = check(&wrong, &ranges, Form::Overlay, |_| Ok(false)).unwrap_err();
        assert!(matches!(e, Error::DiffIdMismatch { .. }), "{e}");
    }
    /// A plain tar of `entries`, as [`layer`] writes it.
    fn tar_of(entries: &[(&str, u64, u64)]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let (_, path) = layer(dir.path(), 0, entries);
        fs::read(path).unwrap()
    }

    /// `blob` written to `dir/<n>`, with the diff ID of the tar it holds.
    fn blob(dir: &std::path::Path, n: usize, blob: &[u8], tar: &[u8]) -> (Digest, PathBuf) {
        let path = dir.join(n.to_string());
        fs::write(&path, blob).unwrap();
        let hash: [u8; 32] = Sha256::digest(tar).into();
        (Digest::from_sha256_bytes(&hash), path)
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(data).unwrap();
        gz.finish().unwrap()
    }

    /// One zstd frame of raw blocks holding `data`, made by hand.
    fn zstd(data: &[u8]) -> Vec<u8> {
        // Single segment, an 8-byte content size, no checksum.
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0xe0];
        frame.extend_from_slice(&u64::try_from(data.len()).unwrap().to_le_bytes());
        let blocks: Vec<&[u8]> = data.chunks(128 * 1024).collect();
        for (i, block) in blocks.iter().enumerate() {
            let last = u32::from(i + 1 == blocks.len());
            let header = last | u32::try_from(block.len()).unwrap() << 3;
            frame.extend_from_slice(&header.to_le_bytes()[..3]);
            frame.extend_from_slice(block);
        }
        frame
    }

    fn skippable(data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x50, 0x2a, 0x4d, 0x18];
        frame.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        frame.extend_from_slice(data);
        frame
    }

    /// Red team (ADR-0098, T16): a far owner in a later gzip member or zstd
    /// frame, the split mid-header, past a skippable frame, is still named.
    #[test]
    fn red_team_an_owner_in_a_later_member_or_frame_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("etc/", 0, 0), ("home/app/", 1000, 0), ("tmp/", 0, 0)]);
        let (head, tail) = tar.split_at(700);
        let gz = [gzip(head), gzip(tail)].concat();
        let zst = [zstd(head), skippable(b"\0\0\0\0"), zstd(tail)].concat();
        let one = MappedIds::parse("0 1000 1\n", "0 1000 1\n").unwrap();
        for (n, bytes) in [gz, zst].iter().enumerate() {
            let layers = vec![blob(dir.path(), n, bytes, &tar)];
            for form in [Form::Overlay, Form::Live] {
                let e = check(&layers, &one, form, |_| Ok(false)).unwrap_err();
                assert!(
                    matches!(e, Error::Rootfs(rnk_rootfs::Error::Unmapped(_))),
                    "{e}"
                );
                assert!(e.to_string().contains("uid 1000 (/home/app)"), "{e}");
            }
        }
    }

    /// Red team (ADR-0098, T16): a zstd blob that starts with a skippable
    /// frame reads as a plain tar and is refused as one, by the check and the
    /// extraction alike.
    #[test]
    fn red_team_a_blob_behind_a_skippable_frame_is_refused_by_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("home/app/", 1000, 0)]);
        let bytes = [skippable(&[0; 504]), zstd(&tar)].concat();
        let layers = vec![blob(dir.path(), 0, &bytes, &tar)];
        let map = MappedIds::parse("0 0 4294967295\n", "0 0 4294967295\n").unwrap();
        for form in [Form::Overlay, Form::Live] {
            let e = check(&layers, &map, form, |_| Ok(false)).unwrap_err();
            assert!(
                !matches!(e, Error::Rootfs(rnk_rootfs::Error::Unmapped(_))),
                "{e}"
            );
        }
        let state = fs::canonicalize(dir.path()).unwrap().join("state");
        let store =
            rnk_store::Store::open(&rnk_store::BuildPaths::new(dir.path(), &state)).unwrap();
        let file = fs::File::open(&layers[0].1).unwrap();
        crate::layer::extract_layer(&store, None, &layers[0].0, file, &[]).unwrap_err();
    }

    /// Red team (ADR-0098, T13): an extraction that failed on an owner part
    /// way through leaves nothing the store calls extracted, so the next
    /// check reads the layer again rather than trusting what it wrote.
    #[test]
    fn red_team_a_layer_whose_extraction_failed_is_checked_again() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (uid, gid) = (
            u64::from(rustix::process::getuid().as_raw()),
            u64::from(rustix::process::getgid().as_raw()),
        );
        let far = 3_000_000_000;
        let layers = vec![layer(
            dir.path(),
            0,
            &[
                ("a/", uid, gid),
                ("a/b", uid, gid),
                ("c/", far, far),
                ("d", uid, gid),
            ],
        )];
        let state = fs::canonicalize(dir.path()).unwrap().join("state");
        let store =
            rnk_store::Store::open(&rnk_store::BuildPaths::new(dir.path(), &state)).unwrap();
        let file = fs::File::open(&layers[0].1).unwrap();
        let e = crate::layer::extract_layer(&store, None, &layers[0].0, file, &[]).unwrap_err();
        assert!(
            format!("{e:?}").contains(r#"op: "chown", path: "./c""#),
            "{e:?}"
        );
        let key = rnk_store::EntryKey::Layer(layers[0].0.clone());
        assert_eq!(store.get(&key).unwrap(), None);
        let own =
            MappedIds::parse(&format!("{uid} {uid} 1\n"), &format!("{gid} {gid} 1\n")).unwrap();
        let mut asked = 0;
        let e = check(&layers, &own, Form::Overlay, |chain| {
            asked += 1;
            Ok(store
                .get(&rnk_store::EntryKey::Layer(chain.clone()))?
                .is_some())
        })
        .unwrap_err();
        assert_eq!(asked, 1);
        assert!(e.to_string().contains("uid 3000000000 (/c)"), "{e}");
    }
}
