//! A step's key inputs as plain data, so tests can copy and change them.

use rnk_cache::{
    CacheKey, Content, ContentHasher, CopyFrom, Encoding, File, FileKind, KeyInputs, Mount,
    Network, Sharing, TimePolicy,
};
use rnk_oci::{Compression, Digest, Platform};

/// Everything [`CacheKey::next`] takes, in the order the test gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Spec {
    /// `--reproducible`'s epoch; `None` keeps each file's mtime.
    pub(crate) epoch: Option<i64>,
    pub(crate) instruction: String,
    pub(crate) args: Vec<(String, Option<String>)>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) user: String,
    pub(crate) workdir: String,
    pub(crate) shell: Vec<String>,
    pub(crate) network: Option<Network>,
    pub(crate) mounts: Vec<Mount>,
    pub(crate) from: Option<CopyFrom>,
    pub(crate) files: Vec<File>,
    pub(crate) heredocs: Vec<(String, Vec<u8>)>,
}

impl Spec {
    pub(crate) fn inputs(&self) -> KeyInputs {
        self.encoded_as(&encoding(self.epoch))
    }

    /// The inputs with `encoding`; `epoch` is ignored.
    pub(crate) fn encoded_as(&self, encoding: &Encoding) -> KeyInputs {
        let mut inputs = KeyInputs::new(encoding);
        for (name, value) in &self.args {
            inputs.arg(name, value.as_deref());
        }
        for (name, value) in &self.env {
            inputs.env(name, value);
        }
        inputs
            .user(&self.user)
            .workdir(&self.workdir)
            .shell(&self.shell);
        if let Some(network) = self.network {
            inputs.network(network);
        }
        for mount in &self.mounts {
            inputs.mount(mount.clone());
        }
        if let Some(from) = &self.from {
            inputs.copy_from(from.clone());
        }
        for file in &self.files {
            inputs.file(file.clone());
        }
        for (name, body) in &self.heredocs {
            inputs.heredoc(name, body);
        }
        inputs
    }

    pub(crate) fn key(&self, prev: &CacheKey) -> CacheKey {
        prev.next(&self.instruction, self.inputs())
    }
}

/// gzip at level 6, with mtimes clamped to `epoch` if there is one.
pub(crate) fn encoding(epoch: Option<i64>) -> Encoding {
    Encoding {
        compression: Compression::Gzip,
        level: 6,
        times: epoch.map_or(TimePolicy::Kept, |epoch| TimePolicy::Clamped { epoch }),
    }
}

/// The epoch of the fixtures' `--reproducible` builds.
pub(crate) const EPOCH: i64 = 1_700_000_000;

pub(crate) fn content(bytes: &[u8]) -> Content {
    let mut h = ContentHasher::new();
    h.update(bytes);
    h.finish()
}

pub(crate) fn digest(seed: u8) -> Digest {
    Digest::from_sha256_bytes(&[seed; 32])
}

pub(crate) fn linux_amd64() -> Platform {
    Platform::new("linux", "amd64")
}

pub(crate) fn root() -> CacheKey {
    CacheKey::root(Some(&digest(1)), &linux_amd64())
}

pub(crate) fn file(path: &str, kind: FileKind) -> File {
    File {
        path: path.as_bytes().to_vec(),
        kind,
        mode: 0o644,
        uid: 0,
        gid: 0,
        capability: None,
        mtime: EPOCH - 1000,
    }
}

/// A step that sets every input, each kind of mount and file and two
/// heredocs included, and keeps mtimes.
pub(crate) fn full() -> Spec {
    Spec {
        epoch: None,
        instruction: "COPY --chown=1000:1000 --from=tools /src /app".to_owned(),
        args: vec![
            ("VERSION".to_owned(), Some("1.2.3".to_owned())),
            ("UNSET".to_owned(), None),
        ],
        env: vec![
            ("PATH".to_owned(), "/usr/local/bin:/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/root".to_owned()),
        ],
        user: "1000:1000".to_owned(),
        workdir: "/app".to_owned(),
        shell: vec!["/bin/sh".to_owned(), "-c".to_owned()],
        network: Some(Network::None),
        mounts: vec![
            Mount::Secret {
                id: "token".to_owned(),
                target: "/run/secrets/token".to_owned(),
                mode: 0o400,
                uid: 0,
                gid: 0,
            },
            Mount::Cache {
                id: "/root/.cache".to_owned(),
                sharing: Sharing::Shared,
                dropped: false,
            },
            Mount::Cache {
                id: "apt".to_owned(),
                sharing: Sharing::Locked,
                dropped: true,
            },
        ],
        from: Some(CopyFrom::Image(digest(2))),
        files: vec![
            File {
                mode: 0o755,
                uid: 1000,
                gid: 1000,
                capability: Some(vec![1, 0, 0, 2, 0, 0x20, 0, 0]),
                mtime: EPOCH + 1000,
                ..file("bin/tool", FileKind::Regular(content(b"#!/bin/sh\n")))
            },
            file("etc", FileKind::Directory),
            file("lib/link", FileKind::Symlink(b"../bin/tool".to_vec())),
            file("lib/hard", FileKind::Hardlink(b"bin/tool".to_vec())),
            file("run/fifo", FileKind::Fifo),
            file("dev/null", FileKind::CharDevice { major: 1, minor: 3 }),
            file("dev/sda", FileKind::BlockDevice { major: 8, minor: 0 }),
        ],
        heredocs: vec![
            ("EOF".to_owned(), b"echo $HOME\n".to_vec()),
            ("CONF".to_owned(), b"debug = true\n".to_vec()),
        ],
    }
}
