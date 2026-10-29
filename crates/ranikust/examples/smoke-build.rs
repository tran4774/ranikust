//! Smoke test of the finished crates before `rnk-builder` exists: pulls a
//! base image (`rnk-registry`), extracts its layers (`rnk-layer`,
//! `rnk-rootfs`), runs `RUN` commands in the sandbox (`rnk-sandbox`), packs
//! the whole root into one gzip layer and writes an OCI image layout
//! (`rnk-layer`, `rnk-oci`).
//!
//! Not a builder: no diff per instruction, no cache, one squashed layer,
//! xattrs dropped. It needs `CAP_SYS_ADMIN`, so it runs inside rootless
//! `podman run --privileged` (`just smoke`), never directly on a host.
//!
//! ```sh
//! smoke-build --out DIR [--image REF] [--work DIR] [--tag NAME] [--run CMD]...
//! ```

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rnk_layer::{
    Compression, Decoder, EncodeOptions, Entry, EntryKind, EntryPath, EntrySource, LayerWriter,
};
use rnk_oci::{
    Descriptor, Digest, History, ImageConfig, ImageFormat, ImageIndex, ImageManifest, MediaType,
    Platform, Reference, to_canonical_json,
};
use rnk_registry::{Client, ClientOptions, Credentials, TrustStore};
use rnk_rootfs::{FileType, OpenHow, RootPath, Whiteouts};
use rnk_sandbox::{RunSpec, Sandbox};
use sha2::Digest as _;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct Args {
    image: String,
    out: PathBuf,
    work: PathBuf,
    tag: String,
    runs: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smoke-build: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Args, BoxError> {
    let mut args = Args {
        image: "docker.io/library/alpine:3.20".into(),
        out: PathBuf::new(),
        work: "/work".into(),
        tag: "smoke".into(),
        runs: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--image" => args.image = value,
            "--out" => args.out = value.into(),
            "--work" => args.work = value.into(),
            "--tag" => args.tag = value,
            "--run" => args.runs.push(value),
            _ => return Err(format!("unknown argument {flag:?}").into()),
        }
    }
    if args.out.as_os_str().is_empty() {
        return Err("--out is required".into());
    }
    Ok(args)
}

fn run() -> Result<(), BoxError> {
    let args = parse_args()?;
    rnk_rootfs::probe_support()?;
    // Before any thread exists.
    rnk_sandbox::enter_build_namespace()?;
    fs::create_dir_all(&args.work)?;
    let work = args.work.canonicalize()?;
    let sandbox_dir = work.join("sandbox");
    let caps = rnk_sandbox::probe(&sandbox_dir)?;
    println!("probe: {caps:?}");

    let reference = Reference::parse(&args.image)?.with_default_tag();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (base, blobs) = runtime.block_on(pull(&reference, &work))?;
    drop(runtime);

    let rootfs = work.join("rootfs");
    fs::create_dir(&rootfs)?;
    let rootfs = rootfs.canonicalize()?;
    let root = RootPath::open(&rootfs)?;
    extract(&root, &base, &blobs)?;

    let sandbox = Sandbox::start(Some(caps), &sandbox_dir)?;
    for cmd in &args.runs {
        run_one(&sandbox, &rootfs, &base, cmd)?;
    }
    drop(sandbox);

    write_layout(&root, &base, &args)
}

async fn pull(reference: &Reference, work: &Path) -> Result<(ImageConfig, Vec<PathBuf>), BoxError> {
    let client = Client::new(
        ClientOptions::default(),
        TrustStore::load(),
        Credentials::none(),
    )?;
    let platform = Platform::new("linux", go_arch()?);
    let images = client.pull_image_manifest(reference, &platform).await?;
    let manifest = ImageManifest::from_json(images.manifest.bytes())?;
    let mut config = Vec::new();
    client
        .pull_blob(reference, &manifest.config, &mut config)
        .await?;
    let base = ImageConfig::from_json(&config)?;
    let dir = work.join("blobs");
    fs::create_dir_all(&dir)?;
    let mut blobs = Vec::new();
    for layer in &manifest.layers {
        let path = dir.join(layer.digest.encoded());
        let mut file = tokio::fs::File::create(&path).await?;
        client.pull_blob(reference, layer, &mut file).await?;
        println!("pulled {} ({} bytes)", layer.digest, layer.size);
        blobs.push(path);
    }
    Ok((base, blobs))
}

fn go_arch() -> Result<&'static str, BoxError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => Err(format!("unsupported architecture {other}").into()),
    }
}

/// Extracts the layers in order and checks each diff ID against the config.
fn extract(root: &RootPath, base: &ImageConfig, blobs: &[PathBuf]) -> Result<(), BoxError> {
    for (i, blob) in blobs.iter().enumerate() {
        let mut decoder = Decoder::new(File::open(blob)?)?;
        rnk_rootfs::extract(root, &mut decoder, Whiteouts::Apply)?;
        let diff_id = decoder.finish()?.to_string();
        let want = base.rootfs.diff_ids.get(i).map(Digest::as_str);
        if want != Some(diff_id.as_str()) {
            return Err(format!("layer {i}: diff ID {diff_id}, the config has {want:?}").into());
        }
    }
    println!("extracted {} layer(s)", blobs.len());
    Ok(())
}

/// One shell-form `RUN` with the base image's environment, user and
/// working directory.
fn run_one(
    sandbox: &Sandbox,
    rootfs: &Path,
    base: &ImageConfig,
    cmd: &str,
) -> Result<(), BoxError> {
    let mut spec = RunSpec::new(rootfs, vec!["/bin/sh".into(), "-c".into(), cmd.into()]);
    let container = base.config.clone().unwrap_or_default();
    spec.env = container
        .env
        .unwrap_or_default()
        .iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.into(), v.into()),
            None => (kv.into(), OsString::new()),
        })
        .collect();
    if let Some(user) = container.user {
        spec.user = user;
    }
    if let Some(dir) = container.working_dir.filter(|d| !d.is_empty()) {
        spec.workdir = dir.into();
    }
    println!("RUN {cmd}");
    let status = sandbox.run(spec)?;
    if !status.success() {
        return Err(format!("RUN {cmd:?}: {status}").into());
    }
    Ok(())
}

/// Packs the root into one layer and writes an OCI image layout to `out`.
fn write_layout(root: &RootPath, base: &ImageConfig, args: &Args) -> Result<(), BoxError> {
    let blobs = args.out.join("blobs").join("sha256");
    fs::create_dir_all(&blobs)?;

    let mut entries = Vec::new();
    walk(root, Path::new(""), &mut entries)?;
    let count = entries.len();
    let mut writer = LayerWriter::new(EncodeOptions {
        compression: Compression::Gzip,
        level: 6,
        threads: std::thread::available_parallelism()?,
        mtime: None,
    })?;
    for entry in entries {
        writer.add(entry)?;
    }
    let tmp = blobs.join("layer.tmp");
    let digests = writer.finish(
        &mut RootSource { root },
        BufWriter::new(File::create(&tmp)?),
    )?;
    let layer_digest = Digest::parse(&digests.digest.to_string())?;
    fs::rename(&tmp, blobs.join(layer_digest.encoded()))?;
    println!(
        "layer {layer_digest}: {count} entries, {} bytes",
        digests.size
    );

    let mut config = base.clone();
    config.rootfs.diff_ids = vec![Digest::parse(&digests.diff_id.to_string())?];
    config.history = Some(vec![History {
        created_by: Some(format!(
            "ranikust smoke-build from {}: {}",
            args.image,
            args.runs.join(" && ")
        )),
        ..History::default()
    }]);
    let config_desc = write_blob(&blobs, MediaType::OCI_CONFIG, &to_canonical_json(&config)?)?;
    let layer_desc = Descriptor::new(MediaType::OCI_LAYER_GZIP, layer_digest, digests.size);
    let manifest = ImageManifest::new(ImageFormat::Oci, config_desc, vec![layer_desc]);
    let mut manifest_desc = write_blob(
        &blobs,
        MediaType::OCI_MANIFEST,
        &to_canonical_json(&manifest)?,
    )?;
    manifest_desc.annotations = Some(BTreeMap::from([(
        "org.opencontainers.image.ref.name".to_owned(),
        args.tag.clone(),
    )]));
    let index = ImageIndex::new(ImageFormat::Oci, vec![manifest_desc]);
    fs::write(args.out.join("index.json"), to_canonical_json(&index)?)?;
    fs::write(
        args.out.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;
    println!("wrote {} with tag {}", args.out.display(), args.tag);
    Ok(())
}

fn write_blob(dir: &Path, media_type: MediaType, bytes: &[u8]) -> Result<Descriptor, BoxError> {
    let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
    let digest = Digest::from_sha256_bytes(&hash);
    fs::write(dir.join(digest.encoded()), bytes)?;
    Ok(Descriptor::new(
        media_type,
        digest,
        u64::try_from(bytes.len())?,
    ))
}

/// Every entry below `dir` (relative to the root), depth first, read
/// through `RootPath`: the root's content came from an image and from `RUN`.
fn walk(root: &RootPath, dir: &Path, out: &mut Vec<Entry>) -> Result<(), BoxError> {
    let at = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let fd = root.open_at(at, &OpenHow::read().directory().no_follow())?;
    let mut names = Vec::new();
    for item in rustix::fs::Dir::read_from(&fd)? {
        let item = item?;
        let name = item.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    for name in names {
        let path = dir.join(&name);
        let st = root.statx(&path)?;
        let kind = match st.file_type() {
            FileType::Regular => EntryKind::File { size: st.size },
            FileType::Directory => EntryKind::Dir,
            FileType::Symlink => {
                EntryKind::Symlink(root.read_link(&path)?.into_os_string().into_vec())
            }
            FileType::CharDevice => EntryKind::Char(st.rdev_major, st.rdev_minor),
            FileType::BlockDevice => EntryKind::Block(st.rdev_major, st.rdev_minor),
            FileType::Fifo => EntryKind::Fifo,
            // Sockets are not layer content.
            _ => continue,
        };
        let recurse = kind.is_dir();
        let inode = (matches!(kind, EntryKind::File { .. }) && st.nlink > 1).then_some((
            (u64::from(st.dev_major) << 32) | u64::from(st.dev_minor),
            st.ino,
        ));
        out.push(Entry {
            path: EntryPath::new(path.as_os_str().as_bytes())?,
            kind,
            mode: st.mode,
            uid: u64::from(st.uid),
            gid: u64::from(st.gid),
            mtime: st.mtime.secs,
            xattrs: BTreeMap::new(),
            inode,
        });
        if recurse {
            walk(root, &path, out)?;
        }
    }
    Ok(())
}

/// File contents for the layer writer, opened through `RootPath`.
struct RootSource<'a> {
    root: &'a RootPath,
}

impl EntrySource for RootSource<'_> {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let path = Path::new(OsStr::from_bytes(entry.path.as_bytes()));
        let fd = self.root.open_regular(path).map_err(io::Error::other)?;
        Ok(Box::new(File::from(fd)))
    }

    fn parent(&mut self, _: &EntryPath) -> io::Result<Option<Entry>> {
        // Every directory was walked and added.
        Ok(None)
    }
}
